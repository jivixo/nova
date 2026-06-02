// nova_rt.c — Nova language runtime (C implementation)
//
// This file is the full runtime support library for Nova programs compiled to
// LLVM IR and linked via clang. Every Nova value is a NovaValue tagged union:
//
//   typedef struct { uint64_t tag; uint64_t payload; } NovaValue;
//
// Tag  Payload interpretation
// ---  ----------------------
//  0   NIL     — payload is 0
//  1   BOOL    — payload is 0 (false) or 1 (true)
//  2   INT     — payload reinterpreted as int64_t (cast, not bit-copy)
//  3   FLOAT   — payload holds the IEEE-754 bit pattern of a double (memcpy)
//  4   STR     — payload is a pointer to a null-terminated C string (heap or const)
//  5   ARRAY   — payload is a pointer to a heap-allocated NovaArray
//  6   MAP     — payload is a pointer to a heap-allocated NovaMap (linked list)
//  7   CLOSURE — payload is a pointer to a heap-allocated NovaClosure
//  8   ENUM    — payload is a pointer to a heap-allocated NovaEnum
//  9   TASK    — payload is a pointer to a heap-allocated NovaTask (thread handle)
// 10   CHAN    — payload is a pointer to a heap-allocated NovaChan (channel)
//
// All output parameters follow a consistent convention:
//   void nova_op(const NovaValue* a, ..., NovaValue* out)
// The 'out' slot is always pre-allocated on the caller's LLVM stack frame.
// Functions write their result into *out; they never return NovaValue by value.
//
// Runtime errors call nova_throw_error(), which sets a global thrown flag.
// The LLVM codegen emits nova_is_thrown() checks after each statement inside
// a try block to detect errors thrown from called functions.
//
// Compile: clang -c nova_rt.c -o nova_rt.o

// Suppress MSVC deprecation warnings for POSIX functions (strdup, strcat, etc.)
// These functions are safe and portable; MSVC just prefers its own _xxx variants.
#define _CRT_SECURE_NO_WARNINGS
#define _CRT_NONSTDC_NO_DEPRECATE

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <math.h>
#include <ctype.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#include <time.h>
#endif

#define TAG_NIL     0
#define TAG_BOOL    1
#define TAG_INT     2
#define TAG_FLOAT   3
#define TAG_STR     4
#define TAG_ARRAY   5
#define TAG_MAP     6
#define TAG_CLOSURE 7
#define TAG_ENUM    8
#define TAG_TASK    9
#define TAG_CHAN    10

typedef struct { uint64_t tag; uint64_t payload; } NovaValue;

// Array: heap-allocated dynamic array of NovaValues
typedef struct {
    int64_t    length;
    int64_t    capacity;
    NovaValue* data;
} NovaArray;

// Hashmap: singly-linked list of entries (simple, not bucket-hashed)
typedef struct NovaEntry {
    const char*       key;
    NovaValue         val;
    struct NovaEntry* next;
} NovaEntry;

typedef struct {
    int64_t    count;
    NovaEntry* head;
} NovaMap;

// Closure: a function pointer paired with a captured environment.
// env[] is a flexible array member — the captured NovaValues live inline
// in the heap allocation immediately after the struct header.
// All compiled lambdas share the same dispatch signature regardless of arity:
//   void dispatch(NovaValue* env, NovaValue* args, NovaValue* out)
// The caller packs arguments into a flat NovaValue array before calling.
typedef void (*ClosureDispatch)(NovaValue* env, NovaValue* args, NovaValue* out);

typedef struct {
    ClosureDispatch dispatch;
    int64_t         env_size;
    NovaValue        env[];   // captured values, inline after the header
} NovaClosure;

// Task: a running or completed background thread.
// The thread receives a pointer to this struct as its argument; it fills result when done.
#ifdef _WIN32
typedef struct { HANDLE    thread; NovaValue result; } NovaTask;
#else
typedef struct { pthread_t thread; NovaValue result; } NovaTask;
#endif

// Channel: a single-slot rendezvous channel (blocking send + blocking recv).
// Send blocks until the previous value is consumed; recv blocks until a value is ready.
#ifdef _WIN32
typedef struct {
    CRITICAL_SECTION   mutex;
    CONDITION_VARIABLE not_empty;
    CONDITION_VARIABLE not_full;
    NovaValue          value;
    int                ready;
} NovaChan;
#else
typedef struct {
    pthread_mutex_t mutex;
    pthread_cond_t  not_empty;
    pthread_cond_t  not_full;
    NovaValue       value;
    int             ready;
} NovaChan;
#endif

// Enum variant: a named constructor with zero or more payload values.
// variant is a pointer to a string constant (not heap-allocated).
// payload[] is a flexible array member — values live inline after the header.
typedef struct {
    const char* variant;
    int64_t     count;
    NovaValue   payload[];
} NovaEnum;

static double   bits_to_f64(uint64_t u) { double  d; memcpy(&d, &u, 8); return d; }
static uint64_t f64_to_bits(double   d) { uint64_t u; memcpy(&u, &d, 8); return u; }

// ── Allocation pool ───────────────────────────────────────────────────────────
//
// All heap allocations made by Nova runtime functions go through nova_alloc /
// nova_strdup / nova_realloc, which register each pointer in a flat array.
// nova_gc() frees every registered pointer in one pass and clears the pool.
// This is called at the end of @main to reclaim all program memory cleanly.
//
// The pool itself is managed with raw malloc/realloc (not through nova_alloc)
// so it is never double-tracked.
//
// Limitation: this frees at program exit only — not during execution.
// A proper incremental GC is planned for Phase 13.

static void** g_pool     = NULL;
static size_t g_pool_n   = 0;
static size_t g_pool_cap = 0;

static void pool_track(void* p) {
    if (!p) return;
    if (g_pool_n >= g_pool_cap) {
        g_pool_cap = g_pool_cap ? g_pool_cap * 2 : 1024;
        g_pool = (void**)realloc(g_pool, g_pool_cap * sizeof(void*));
    }
    g_pool[g_pool_n++] = p;
}

// Remove a pointer from the pool (called before realloc replaces it).
// Linear scan is acceptable for the scale of typical Nova programs.
static void pool_untrack(void* p) {
    for (size_t i = 0; i < g_pool_n; i++) {
        if (g_pool[i] == p) { g_pool[i] = g_pool[--g_pool_n]; return; }
    }
}

static void* nova_alloc(size_t n) {
    void* p = malloc(n);
    pool_track(p);
    return p;
}

static char* nova_strdup(const char* s) {
    size_t n = strlen(s) + 1;
    char* p = (char*)malloc(n);
    memcpy(p, s, n);
    pool_track(p);
    return p;
}

static void* nova_realloc(void* old_p, size_t n) {
    pool_untrack(old_p);
    void* p = realloc(old_p, n);
    pool_track(p);
    return p;
}

// Free all registered allocations and reset the pool.
// Called at the end of @main by generated LLVM IR.
void nova_gc(void) {
    for (size_t i = 0; i < g_pool_n; i++) free(g_pool[i]);
    free(g_pool);
    g_pool     = NULL;
    g_pool_n   = 0;
    g_pool_cap = 0;
}

// Value constructors — write a typed NovaValue into *o
void nova_make_int  (int64_t n,     NovaValue* o) { o->tag = TAG_INT;   o->payload = (uint64_t)n;       }
void nova_make_float(double  f,     NovaValue* o) { o->tag = TAG_FLOAT; o->payload = f64_to_bits(f);    }
void nova_make_bool (int64_t b,     NovaValue* o) { o->tag = TAG_BOOL;  o->payload = (uint64_t)(b!=0);  }
void nova_make_nil  (               NovaValue* o) { o->tag = TAG_NIL;   o->payload = 0;                 }
void nova_make_str  (const char* s, NovaValue* o) { o->tag = TAG_STR;   o->payload = (uint64_t)s;       }

// Forward declaration — nova_throw is implemented in the throw/catch section at the end
void nova_throw(const NovaValue* val);

// Throw a string error message as a Nova runtime exception.
// Callers set *out = nil before returning so the LLVM frame is well-defined even on error.
static void nova_throw_error(const char* msg) {
    NovaValue v; nova_make_str(msg, &v); nova_throw(&v);
}

// Copy
void nova_copy(const NovaValue* src, NovaValue* dst) { *dst = *src; }

// Truthy
int nova_truthy(const NovaValue* v) {
    switch (v->tag) {
        case TAG_NIL:   return 0;
        case TAG_BOOL:  return v->payload != 0;
        case TAG_INT:   return (int64_t)v->payload != 0;
        case TAG_FLOAT: return bits_to_f64(v->payload) != 0.0;
        case TAG_ARRAY:   return ((NovaArray*)v->payload)->length > 0;
        case TAG_MAP:     return ((NovaMap*)v->payload)->count > 0;
        case TAG_CLOSURE: return 1;
        case TAG_ENUM:    return 1;
        case TAG_TASK:    return 1;
        case TAG_CHAN:    return 1;
        default:          return 1;
    }
}

// Forward declaration so nova_print can call print_value recursively
static void print_value(const NovaValue* v);

void nova_print(const NovaValue* v) {
    print_value(v);
    printf("\n");
}

static void print_value(const NovaValue* v) {
    switch (v->tag) {
        case TAG_NIL:   printf("nil");                                break;
        case TAG_BOOL:  printf("%s", v->payload ? "true" : "false"); break;
        case TAG_INT:   printf("%lld", (long long)(int64_t)v->payload); break;
        case TAG_FLOAT: printf("%g",   bits_to_f64(v->payload));      break;
        case TAG_STR:   printf("%s",   (const char*)v->payload);      break;
        case TAG_ARRAY: {
            NovaArray* a = (NovaArray*)v->payload;
            printf("[");
            for (int64_t i = 0; i < a->length; i++) {
                if (i > 0) printf(", ");
                print_value(&a->data[i]);
            }
            printf("]");
            break;
        }
        case TAG_MAP: {
            NovaMap* m = (NovaMap*)v->payload;
            printf("{");
            int first = 1;
            for (NovaEntry* e = m->head; e; e = e->next) {
                if (!first) printf(", ");
                printf("%s: ", e->key);
                print_value(&e->val);
                first = 0;
            }
            printf("}");
            break;
        }
        case TAG_CLOSURE: printf("<closure>"); break;
        case TAG_TASK:    printf("<task>");    break;
        case TAG_CHAN:    printf("<channel>"); break;
        case TAG_ENUM: {
            NovaEnum* e = (NovaEnum*)v->payload;
            printf("%s", e->variant);
            if (e->count > 0) {
                printf("(");
                for (int64_t i = 0; i < e->count; i++) {
                    if (i > 0) printf(", ");
                    print_value(&e->payload[i]);
                }
                printf(")");
            }
            break;
        }
        default: printf("<unknown>"); break;
    }
}

// Closure: create a closure value from a dispatch function pointer and an env array.
// Copies env_size NovaValues from env_vals into a heap-allocated NovaClosure.
void nova_make_closure(void* dispatch_fn, NovaValue* env_vals, int64_t env_size, NovaValue* out) {
    NovaClosure* c = nova_alloc(sizeof(NovaClosure) + (size_t)env_size * sizeof(NovaValue));
    c->dispatch  = (ClosureDispatch)dispatch_fn;
    c->env_size  = env_size;
    if (env_size > 0) memcpy(c->env, env_vals, (size_t)env_size * sizeof(NovaValue));
    out->tag     = TAG_CLOSURE;
    out->payload = (uint64_t)c;
}

// Closure: call a closure value with a flat array of arguments.
// Extracts the dispatch function and env from the closure, then calls dispatch(env, args, out).
void nova_invoke_closure(NovaValue* closure_val, NovaValue* args, int64_t nargs, NovaValue* out) {
    if (closure_val->tag != TAG_CLOSURE) {
        nova_throw_error("call: value is not a function");
        out->tag = TAG_NIL; out->payload = 0; return;
    }
    NovaClosure* c = (NovaClosure*)closure_val->payload;
    c->dispatch(c->env, args, out);
    (void)nargs;
}

// Arithmetic helpers
static double to_f(const NovaValue* v) {
    return v->tag == TAG_INT ? (double)(int64_t)v->payload : bits_to_f64(v->payload);
}

#define ARITH_OP(fn, op) \
void fn(const NovaValue* a, const NovaValue* b, NovaValue* o) { \
    if (a->tag == TAG_INT && b->tag == TAG_INT) { \
        o->tag = TAG_INT; o->payload = (uint64_t)((int64_t)a->payload op (int64_t)b->payload); \
    } else if ((a->tag == TAG_INT || a->tag == TAG_FLOAT) && \
               (b->tag == TAG_INT || b->tag == TAG_FLOAT)) { \
        o->tag = TAG_FLOAT; o->payload = f64_to_bits(to_f(a) op to_f(b)); \
    } else { o->tag = TAG_NIL; o->payload = 0; } \
}

void nova_add(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    if (a->tag == TAG_INT && b->tag == TAG_INT) {
        o->tag = TAG_INT; o->payload = (uint64_t)((int64_t)a->payload + (int64_t)b->payload);
    } else if ((a->tag == TAG_INT || a->tag == TAG_FLOAT) &&
               (b->tag == TAG_INT || b->tag == TAG_FLOAT)) {
        o->tag = TAG_FLOAT; o->payload = f64_to_bits(to_f(a) + to_f(b));
    } else if (a->tag == TAG_STR && b->tag == TAG_STR) {
        const char* sa = (const char*)a->payload;
        const char* sb = (const char*)b->payload;
        size_t la = strlen(sa), lb = strlen(sb);
        char* r = nova_alloc(la + lb + 1);
        memcpy(r, sa, la); memcpy(r + la, sb, lb + 1);
        o->tag = TAG_STR; o->payload = (uint64_t)r;
    } else { o->tag = TAG_NIL; o->payload = 0; }
}
ARITH_OP(nova_sub, -)
ARITH_OP(nova_mul, *)

void nova_div(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    if (a->tag == TAG_INT && b->tag == TAG_INT) {
        int64_t bv = (int64_t)b->payload;
        if (bv == 0) { nova_throw_error("div: division by zero"); o->tag = TAG_NIL; o->payload = 0; return; }
        o->tag = TAG_INT; o->payload = (uint64_t)((int64_t)a->payload / bv);
        return;
    }
    if ((b->tag == TAG_INT   && (int64_t)b->payload == 0) ||
        (b->tag == TAG_FLOAT && bits_to_f64(b->payload) == 0.0)) {
        nova_throw_error("div: division by zero");
        o->tag = TAG_NIL; o->payload = 0; return;
    }
    o->tag = TAG_FLOAT;
    o->payload = f64_to_bits(to_f(a) / to_f(b));
}

void nova_mod(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    if (a->tag == TAG_INT && b->tag == TAG_INT) {
        int64_t bv = (int64_t)b->payload;
        if (bv == 0) {
            nova_throw_error("mod: division by zero");
            o->tag = TAG_NIL; o->payload = 0; return;
        }
        o->tag = TAG_INT; o->payload = (uint64_t)((int64_t)a->payload % bv);
    } else { o->tag = TAG_NIL; o->payload = 0; }
}

// Comparisons
#define CMP_OP(fn, op) \
void fn(const NovaValue* a, const NovaValue* b, NovaValue* o) { \
    int result = 0; \
    if (a->tag == TAG_INT && b->tag == TAG_INT) \
        result = (int64_t)a->payload op (int64_t)b->payload; \
    else if ((a->tag == TAG_INT || a->tag == TAG_FLOAT) && \
             (b->tag == TAG_INT || b->tag == TAG_FLOAT)) \
        result = to_f(a) op to_f(b); \
    o->tag = TAG_BOOL; o->payload = result; \
}

CMP_OP(nova_lt,  <)
CMP_OP(nova_lte, <=)
CMP_OP(nova_gt,  >)
CMP_OP(nova_gte, >=)

void nova_eq(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    int r = 0;
    if      (a->tag == TAG_NIL   && b->tag == TAG_NIL)   r = 1;
    else if (a->tag == TAG_BOOL  && b->tag == TAG_BOOL)  r = a->payload == b->payload;
    else if (a->tag == TAG_INT   && b->tag == TAG_INT)   r = (int64_t)a->payload == (int64_t)b->payload;
    else if (a->tag == TAG_FLOAT && b->tag == TAG_FLOAT) r = bits_to_f64(a->payload) == bits_to_f64(b->payload);
    else if (a->tag == TAG_INT   && b->tag == TAG_FLOAT) r = (double)(int64_t)a->payload == bits_to_f64(b->payload);
    else if (a->tag == TAG_FLOAT && b->tag == TAG_INT)   r = bits_to_f64(a->payload) == (double)(int64_t)b->payload;
    else if (a->tag == TAG_STR   && b->tag == TAG_STR)   r = strcmp((const char*)a->payload, (const char*)b->payload) == 0;
    o->tag = TAG_BOOL; o->payload = r;
}

void nova_neq(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    nova_eq(a, b, o); o->payload = !o->payload;
}

// Logic
void nova_and(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    o->tag = TAG_BOOL; o->payload = nova_truthy(a) && nova_truthy(b);
}
void nova_or(const NovaValue* a, const NovaValue* b, NovaValue* o) {
    o->tag = TAG_BOOL; o->payload = nova_truthy(a) || nova_truthy(b);
}
void nova_not(const NovaValue* a, NovaValue* o) {
    o->tag = TAG_BOOL; o->payload = !nova_truthy(a);
}

// clock() — milliseconds since Unix epoch (Wall clock, not CPU time)
void nova_clock(NovaValue* out) {
#ifdef _WIN32
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    uint64_t t = ((uint64_t)ft.dwHighDateTime << 32) | (uint64_t)ft.dwLowDateTime;
    t -= 116444736000000000ULL; // 100-ns intervals from 1601 to 1970
    out->tag = TAG_INT; out->payload = (uint64_t)(int64_t)(t / 10000); // to ms
#else
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    int64_t ms = (int64_t)ts.tv_sec * 1000LL + (int64_t)ts.tv_nsec / 1000000LL;
    out->tag = TAG_INT; out->payload = (uint64_t)ms;
#endif
}

// make_array(n, default) — create a pre-sized array filled with default value
void nova_make_array_n(NovaValue* n_val, NovaValue* def_val, NovaValue* out) {
    int64_t n = (n_val->tag == TAG_INT) ? (int64_t)n_val->payload : 0;
    if (n < 0) n = 0;
    NovaArray* a = nova_alloc(sizeof(NovaArray));
    a->length   = n;
    a->capacity = n > 0 ? n : 1;
    a->data     = nova_alloc((size_t)a->capacity * sizeof(NovaValue));
    for (int64_t i = 0; i < n; i++) a->data[i] = *def_val;
    out->tag = TAG_ARRAY; out->payload = (uint64_t)a;
}

// Array: create an empty array with a small initial capacity
void nova_make_array(NovaValue* out) {
    NovaArray* a = nova_alloc(sizeof(NovaArray));
    a->length   = 0;
    a->capacity = 8;
    a->data     = nova_alloc(8 * sizeof(NovaValue));
    out->tag     = TAG_ARRAY;
    out->payload = (uint64_t)a;
}

// Array: append an element in place — used during array literal construction
// Not CoW; callers that need value semantics should use nova_array_push instead.
void nova_array_append(NovaValue* arr, NovaValue* elem) {
    if (arr->tag != TAG_ARRAY) return;
    NovaArray* a = (NovaArray*)arr->payload;
    if (a->length >= a->capacity) {
        a->capacity *= 2;
        a->data = nova_realloc(a->data, (size_t)a->capacity * sizeof(NovaValue));
    }
    a->data[a->length++] = *elem;
}

// Array: push — CoW, returns a new array with the element appended
// This is Nova's push() builtin: let arr2 = push(arr, x)
void nova_array_push(NovaValue* arr, NovaValue* elem, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaArray* src = (NovaArray*)arr->payload;
    NovaArray* dst = nova_alloc(sizeof(NovaArray));
    dst->length   = src->length + 1;
    dst->capacity = dst->length + 4;
    dst->data = nova_alloc((size_t)dst->capacity * sizeof(NovaValue));
    memcpy(dst->data, src->data, (size_t)src->length * sizeof(NovaValue));
    dst->data[src->length] = *elem;
    out->tag     = TAG_ARRAY;
    out->payload = (uint64_t)dst;
}

// Map: create an empty hashmap
void nova_make_map(NovaValue* out) {
    NovaMap* m = nova_alloc(sizeof(NovaMap));
    m->count     = 0;
    m->head      = NULL;
    out->tag     = TAG_MAP;
    out->payload = (uint64_t)m;
}

// Map: insert or update a key in place — used during map literal construction
void nova_map_insert(NovaValue* map, NovaValue* key, NovaValue* val) {
    if (map->tag != TAG_MAP || key->tag != TAG_STR) return;
    NovaMap*    m = (NovaMap*)map->payload;
    const char* k = (const char*)key->payload;
    for (NovaEntry* e = m->head; e; e = e->next) {
        if (strcmp(e->key, k) == 0) { e->val = *val; return; }
    }
    NovaEntry* e = nova_alloc(sizeof(NovaEntry));
    e->key  = k;
    e->val  = *val;
    e->next = m->head;
    m->head = e;
    m->count++;
}

// Index read: dispatch on tag — array uses integer index, map uses string key.
// Array out-of-bounds and wrong-type subscript throw runtime errors.
// Map missing key returns nil (not an error — checking membership is common).
void nova_index_get(NovaValue* obj, NovaValue* idx, NovaValue* out) {
    if (obj->tag == TAG_ARRAY && idx->tag == TAG_INT) {
        NovaArray* a = (NovaArray*)obj->payload;
        int64_t i = (int64_t)idx->payload;
        if (i < 0) i += a->length;
        if (i < 0 || i >= a->length) {
            nova_throw_error("index: array index out of bounds");
            out->tag = TAG_NIL; out->payload = 0; return;
        }
        *out = a->data[i];
    } else if (obj->tag == TAG_MAP && idx->tag == TAG_STR) {
        NovaMap*    m = (NovaMap*)obj->payload;
        const char* k = (const char*)idx->payload;
        for (NovaEntry* e = m->head; e; e = e->next) {
            if (strcmp(e->key, k) == 0) { *out = e->val; return; }
        }
        out->tag = TAG_NIL; out->payload = 0;  // missing key → nil, not an error
    } else {
        nova_throw_error("index: value does not support subscript");
        out->tag = TAG_NIL; out->payload = 0;
    }
}

// Index write: mutates in place — arr[i] = val or map[key] = val.
// Array out-of-bounds throws rather than silently doing nothing.
void nova_index_set(NovaValue* obj, NovaValue* idx, NovaValue* val) {
    if (obj->tag == TAG_ARRAY && idx->tag == TAG_INT) {
        NovaArray* a = (NovaArray*)obj->payload;
        int64_t i = (int64_t)idx->payload;
        if (i < 0) i += a->length;
        if (i < 0 || i >= a->length) {
            nova_throw_error("index: array index out of bounds");
            return;
        }
        a->data[i] = *val;
    } else if (obj->tag == TAG_MAP) {
        nova_map_insert(obj, idx, val);
    }
}

// len() — returns the length of an array, map, or string
void nova_len(NovaValue* v, NovaValue* out) {
    out->tag = TAG_INT;
    if      (v->tag == TAG_ARRAY) out->payload = (uint64_t)((NovaArray*)v->payload)->length;
    else if (v->tag == TAG_MAP)   out->payload = (uint64_t)((NovaMap*)v->payload)->count;
    else if (v->tag == TAG_STR)   out->payload = (uint64_t)strlen((const char*)v->payload);
    else                          out->payload = 0;
}

// Enum: create an enum variant value.
// variant must be a pointer to a string constant (not heap-allocated; the NovaEnum stores the ptr).
// payload_vals is a flat array of count NovaValues copied into the heap allocation.
void nova_make_enum(const char* variant, NovaValue* payload_vals, int64_t count, NovaValue* out) {
    NovaEnum* e = nova_alloc(sizeof(NovaEnum) + (size_t)count * sizeof(NovaValue));
    e->variant = variant;
    e->count   = count;
    if (count > 0) memcpy(e->payload, payload_vals, (size_t)count * sizeof(NovaValue));
    out->tag     = TAG_ENUM;
    out->payload = (uint64_t)e;
}

// Enum: check whether a value is an enum variant with the given name.
// Writes TAG_BOOL true/false into out.
void nova_check_enum(NovaValue* v, const char* variant, NovaValue* out) {
    out->tag = TAG_BOOL;
    if (v->tag != TAG_ENUM) { out->payload = 0; return; }
    NovaEnum* e = (NovaEnum*)v->payload;
    out->payload = (strcmp(e->variant, variant) == 0) ? 1 : 0;
}

// Enum: extract the i-th payload value from an enum variant.
// Writes nil into out if v is not an enum or i is out of range.
void nova_get_enum_payload(NovaValue* v, int64_t i, NovaValue* out) {
    if (v->tag != TAG_ENUM) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaEnum* e = (NovaEnum*)v->payload;
    if (i < 0 || i >= e->count) { out->tag = TAG_NIL; out->payload = 0; return; }
    *out = e->payload[i];
}

// Thread argument: struct passed to the new thread; shared by Win32 and POSIX paths.
// The new thread receives a pointer to this, runs the closure, writes result into task->result.
typedef struct { NovaClosure* closure; NovaTask* task; } NovaThreadArg;

// ── Concurrency (Win32 / POSIX)
//
// Nova's concurrency model: spawn() launches a closure on a new OS thread and
// returns a Task handle; wait() blocks until the thread finishes and yields the
// return value. Channels are single-slot rendezvous: send blocks until the
// previous value is consumed, recv blocks until a value is ready.
//
// The implementation is split into two platform paths via preprocessor:
//   _WIN32 path  — Win32 threads (CreateThread/WaitForSingleObject),
//                  critical sections, and condition variables.
//   POSIX path   — pthreads (pthread_create/pthread_join),
//                  pthread_mutex_t, and pthread_cond_t.
//
// The POSIX path compiles cleanly but has NOT been tested on Linux or macOS.
// The logic mirrors the Win32 path 1-to-1. To verify on Linux/macOS, compile:
//   clang -c nova_rt.c -o nova_rt.o && clang test.ll nova_rt.o -lpthread -o test

#ifdef _WIN32

static DWORD WINAPI nova_thread_fn(LPVOID param) {
    NovaThreadArg* a = (NovaThreadArg*)param;
    NovaValue dummy;
    a->closure->dispatch(a->closure->env, &dummy, &a->task->result);
    free(a);
    return 0;
}

void nova_spawn(NovaValue* closure_val, NovaValue* out) {
    if (closure_val->tag != TAG_CLOSURE) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaClosure*   c = (NovaClosure*)closure_val->payload;
    NovaTask*      t = nova_alloc(sizeof(NovaTask));
    t->result.tag = TAG_NIL; t->result.payload = 0;
    NovaThreadArg* a = malloc(sizeof(NovaThreadArg)); // freed by thread; not pool-tracked
    a->closure = c; a->task = t;
    t->thread = CreateThread(NULL, 0, nova_thread_fn, a, 0, NULL);
    out->tag = TAG_TASK; out->payload = (uint64_t)t;
}

void nova_wait(NovaValue* task_val, NovaValue* out) {
    if (task_val->tag != TAG_TASK) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaTask* t = (NovaTask*)task_val->payload;
    WaitForSingleObject(t->thread, INFINITE);
    CloseHandle(t->thread);
    *out = t->result;
}

void nova_make_chan(NovaValue* out) {
    NovaChan* ch = nova_alloc(sizeof(NovaChan));
    InitializeCriticalSection(&ch->mutex);
    InitializeConditionVariable(&ch->not_empty);
    InitializeConditionVariable(&ch->not_full);
    ch->ready = 0;
    out->tag = TAG_CHAN; out->payload = (uint64_t)ch;
}

void nova_send(NovaValue* chan_val, NovaValue* val_ptr, NovaValue* out) {
    if (chan_val->tag == TAG_CHAN) {
        NovaChan* ch = (NovaChan*)chan_val->payload;
        EnterCriticalSection(&ch->mutex);
        while (ch->ready) SleepConditionVariableCS(&ch->not_full, &ch->mutex, INFINITE);
        ch->value = *val_ptr; ch->ready = 1;
        WakeConditionVariable(&ch->not_empty);
        LeaveCriticalSection(&ch->mutex);
    }
    out->tag = TAG_NIL; out->payload = 0;
}

void nova_recv(NovaValue* chan_val, NovaValue* out) {
    if (chan_val->tag != TAG_CHAN) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaChan* ch = (NovaChan*)chan_val->payload;
    EnterCriticalSection(&ch->mutex);
    while (!ch->ready) SleepConditionVariableCS(&ch->not_empty, &ch->mutex, INFINITE);
    *out = ch->value; ch->ready = 0;
    WakeConditionVariable(&ch->not_full);
    LeaveCriticalSection(&ch->mutex);
}

#else  // POSIX path — pthread-based; see concurrency header comment above

static void* nova_thread_fn(void* param) {
    NovaThreadArg* a = (NovaThreadArg*)param;
    NovaValue dummy;
    a->closure->dispatch(a->closure->env, &dummy, &a->task->result);
    free(a);
    return NULL;
}

void nova_spawn(NovaValue* closure_val, NovaValue* out) {
    if (closure_val->tag != TAG_CLOSURE) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaClosure*   c = (NovaClosure*)closure_val->payload;
    NovaTask*      t = nova_alloc(sizeof(NovaTask));
    t->result.tag = TAG_NIL; t->result.payload = 0;
    NovaThreadArg* a = malloc(sizeof(NovaThreadArg)); // freed by thread; not pool-tracked
    a->closure = c; a->task = t;
    pthread_create(&t->thread, NULL, nova_thread_fn, a);
    out->tag = TAG_TASK; out->payload = (uint64_t)t;
}

void nova_wait(NovaValue* task_val, NovaValue* out) {
    if (task_val->tag != TAG_TASK) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaTask* t = (NovaTask*)task_val->payload;
    pthread_join(t->thread, NULL);
    *out = t->result;
}

void nova_make_chan(NovaValue* out) {
    NovaChan* ch = nova_alloc(sizeof(NovaChan));
    pthread_mutex_init(&ch->mutex, NULL);
    pthread_cond_init(&ch->not_empty, NULL);
    pthread_cond_init(&ch->not_full, NULL);
    ch->ready = 0;
    out->tag = TAG_CHAN; out->payload = (uint64_t)ch;
}

void nova_send(NovaValue* chan_val, NovaValue* val_ptr, NovaValue* out) {
    if (chan_val->tag == TAG_CHAN) {
        NovaChan* ch = (NovaChan*)chan_val->payload;
        pthread_mutex_lock(&ch->mutex);
        while (ch->ready) pthread_cond_wait(&ch->not_full, &ch->mutex);
        ch->value = *val_ptr; ch->ready = 1;
        pthread_cond_signal(&ch->not_empty);
        pthread_mutex_unlock(&ch->mutex);
    }
    out->tag = TAG_NIL; out->payload = 0;
}

void nova_recv(NovaValue* chan_val, NovaValue* out) {
    if (chan_val->tag != TAG_CHAN) { out->tag = TAG_NIL; out->payload = 0; return; }
    NovaChan* ch = (NovaChan*)chan_val->payload;
    pthread_mutex_lock(&ch->mutex);
    while (!ch->ready) pthread_cond_wait(&ch->not_empty, &ch->mutex);
    *out = ch->value; ch->ready = 0;
    pthread_cond_signal(&ch->not_full);
    pthread_mutex_unlock(&ch->mutex);
}

#endif

// Convert any NovaValue to a heap-allocated C string (caller must free).
static char* value_to_cstr(const NovaValue* v) {
    char buf[64];
    switch (v->tag) {
        case TAG_NIL:   return strdup("nil");
        case TAG_BOOL:  return strdup(v->payload ? "true" : "false");
        case TAG_INT:   snprintf(buf, sizeof(buf), "%lld", (long long)(int64_t)v->payload); return strdup(buf);
        case TAG_FLOAT: snprintf(buf, sizeof(buf), "%g", bits_to_f64(v->payload)); return strdup(buf);
        case TAG_STR:   return strdup((const char*)v->payload);
        default:        return strdup("<value>");
    }
}

// Build an interpolated string from n NovaValue parts.
// Each part is stringified and concatenated; result is a TAG_STR NovaValue.
void nova_str_build(NovaValue* parts, int64_t n, NovaValue* out) {
    // strs is a temporary work buffer — use plain malloc/free, not tracked
    char** strs = malloc((size_t)n * sizeof(char*));
    size_t total = 0;
    for (int64_t i = 0; i < n; i++) {
        strs[i] = value_to_cstr(&parts[i]); // plain malloc — freed in loop below
        total += strlen(strs[i]);
    }
    char* result = nova_alloc(total + 1); // tracked — becomes the output string
    result[0] = '\0';
    for (int64_t i = 0; i < n; i++) {
        strcat(result, strs[i]);
        free(strs[i]);
    }
    free(strs);
    out->tag = TAG_STR;
    out->payload = (uint64_t)result;
}

// Type conversion 

void nova_to_str(NovaValue* v, NovaValue* out) {
    char* s = value_to_cstr(v); // value_to_cstr uses plain malloc; track it so nova_gc frees it
    pool_track(s);
    out->tag = TAG_STR; out->payload = (uint64_t)s;
}

void nova_to_int(NovaValue* v, NovaValue* out) {
    switch (v->tag) {
        case TAG_INT:   *out = *v; break;
        case TAG_FLOAT: nova_make_int((int64_t)bits_to_f64(v->payload), out); break;
        case TAG_STR:   nova_make_int((int64_t)atoll((const char*)v->payload), out); break;
        case TAG_BOOL:  nova_make_int((int64_t)v->payload, out); break;
        default:        nova_make_int(0, out); break;
    }
}

void nova_to_float(NovaValue* v, NovaValue* out) {
    switch (v->tag) {
        case TAG_FLOAT: *out = *v; break;
        case TAG_INT:   nova_make_float((double)(int64_t)v->payload, out); break;
        case TAG_STR:   nova_make_float(atof((const char*)v->payload), out); break;
        case TAG_BOOL:  nova_make_float((double)v->payload, out); break;
        default:        nova_make_float(0.0, out); break;
    }
}

void nova_type_of(NovaValue* v, NovaValue* out) {
    const char* t;
    switch (v->tag) {
        case TAG_NIL:     t = "nil";     break;
        case TAG_BOOL:    t = "bool";    break;
        case TAG_INT:     t = "int";     break;
        case TAG_FLOAT:   t = "float";   break;
        case TAG_STR:     t = "str";     break;
        case TAG_ARRAY:   t = "array";   break;
        case TAG_MAP:     t = "map";     break;
        case TAG_CLOSURE: t = "closure"; break;
        case TAG_ENUM:    t = "enum";    break;
        default:          t = "unknown"; break;
    }
    nova_make_str(t, out);
}

// Math

void nova_abs(NovaValue* v, NovaValue* out) {
    if (v->tag == TAG_INT) {
        int64_t n = (int64_t)v->payload;
        nova_make_int(n < 0 ? -n : n, out);
    } else {
        double f = v->tag == TAG_FLOAT ? bits_to_f64(v->payload) : (double)(int64_t)v->payload;
        nova_make_float(f < 0 ? -f : f, out);
    }
}

void nova_sqrt(NovaValue* v, NovaValue* out) {
    double f = v->tag == TAG_FLOAT ? bits_to_f64(v->payload) : (double)(int64_t)v->payload;
    nova_make_float(sqrt(f), out);
}

void nova_floor(NovaValue* v, NovaValue* out) {
    double f = v->tag == TAG_FLOAT ? bits_to_f64(v->payload) : (double)(int64_t)v->payload;
    nova_make_int((int64_t)floor(f), out);
}

void nova_ceil(NovaValue* v, NovaValue* out) {
    double f = v->tag == TAG_FLOAT ? bits_to_f64(v->payload) : (double)(int64_t)v->payload;
    nova_make_int((int64_t)ceil(f), out);
}

void nova_round(NovaValue* v, NovaValue* out) {
    double f = v->tag == TAG_FLOAT ? bits_to_f64(v->payload) : (double)(int64_t)v->payload;
    nova_make_int((int64_t)round(f), out);
}

void nova_min(NovaValue* a, NovaValue* b, NovaValue* out) {
    if (a->tag == TAG_INT && b->tag == TAG_INT) {
        int64_t ia = (int64_t)a->payload, ib = (int64_t)b->payload;
        nova_make_int(ia < ib ? ia : ib, out);
    } else {
        double da = a->tag == TAG_FLOAT ? bits_to_f64(a->payload) : (double)(int64_t)a->payload;
        double db = b->tag == TAG_FLOAT ? bits_to_f64(b->payload) : (double)(int64_t)b->payload;
        *out = (da <= db) ? *a : *b;
    }
}

void nova_max(NovaValue* a, NovaValue* b, NovaValue* out) {
    if (a->tag == TAG_INT && b->tag == TAG_INT) {
        int64_t ia = (int64_t)a->payload, ib = (int64_t)b->payload;
        nova_make_int(ia > ib ? ia : ib, out);
    } else {
        double da = a->tag == TAG_FLOAT ? bits_to_f64(a->payload) : (double)(int64_t)a->payload;
        double db = b->tag == TAG_FLOAT ? bits_to_f64(b->payload) : (double)(int64_t)b->payload;
        *out = (da >= db) ? *a : *b;
    }
}

// String operations

void nova_upper(NovaValue* s, NovaValue* out) {
    if (s->tag != TAG_STR) { nova_make_nil(out); return; }
    const char* src = (const char*)s->payload;
    size_t len = strlen(src);
    char* r = nova_alloc(len + 1);
    for (size_t i = 0; i <= len; i++) r[i] = (char)toupper((unsigned char)src[i]);
    out->tag = TAG_STR; out->payload = (uint64_t)r;
}

void nova_lower(NovaValue* s, NovaValue* out) {
    if (s->tag != TAG_STR) { nova_make_nil(out); return; }
    const char* src = (const char*)s->payload;
    size_t len = strlen(src);
    char* r = nova_alloc(len + 1);
    for (size_t i = 0; i <= len; i++) r[i] = (char)tolower((unsigned char)src[i]);
    out->tag = TAG_STR; out->payload = (uint64_t)r;
}

void nova_trim(NovaValue* s, NovaValue* out) {
    if (s->tag != TAG_STR) { nova_make_nil(out); return; }
    const char* p = (const char*)s->payload;
    while (isspace((unsigned char)*p)) p++;
    size_t len = strlen(p);
    while (len > 0 && isspace((unsigned char)p[len - 1])) len--;
    char* r = nova_alloc(len + 1);
    memcpy(r, p, len); r[len] = '\0';
    out->tag = TAG_STR; out->payload = (uint64_t)r;
}

void nova_contains(NovaValue* s, NovaValue* sub, NovaValue* out) {
    if (s->tag != TAG_STR || sub->tag != TAG_STR) { nova_make_bool(0, out); return; }
    nova_make_bool(strstr((const char*)s->payload, (const char*)sub->payload) != NULL ? 1 : 0, out);
}

void nova_starts_with(NovaValue* s, NovaValue* prefix, NovaValue* out) {
    if (s->tag != TAG_STR || prefix->tag != TAG_STR) { nova_make_bool(0, out); return; }
    const char* pre = (const char*)prefix->payload;
    nova_make_bool(strncmp((const char*)s->payload, pre, strlen(pre)) == 0 ? 1 : 0, out);
}

void nova_ends_with(NovaValue* s, NovaValue* suffix, NovaValue* out) {
    if (s->tag != TAG_STR || suffix->tag != TAG_STR) { nova_make_bool(0, out); return; }
    const char* str = (const char*)s->payload;
    const char* suf = (const char*)suffix->payload;
    size_t slen = strlen(str), suflen = strlen(suf);
    if (suflen > slen) { nova_make_bool(0, out); return; }
    nova_make_bool(strcmp(str + slen - suflen, suf) == 0 ? 1 : 0, out);
}

void nova_replace(NovaValue* s, NovaValue* from, NovaValue* to, NovaValue* out) {
    if (s->tag != TAG_STR) { nova_make_nil(out); return; }
    const char* str   = (const char*)s->payload;
    const char* from_s = from->tag == TAG_STR ? (const char*)from->payload : "";
    const char* to_s   = to->tag   == TAG_STR ? (const char*)to->payload   : "";
    size_t flen = strlen(from_s), tlen = strlen(to_s);
    if (flen == 0) { out->tag = TAG_STR; out->payload = (uint64_t)nova_strdup(str); return; }
    size_t count = 0;
    const char* p = str;
    while ((p = strstr(p, from_s)) != NULL) { count++; p += flen; }
    size_t slen = strlen(str);
    char* result = nova_alloc(slen + count * (tlen > flen ? tlen - flen : 0) + count + 1);
    char* rp = result; p = str;
    const char* found;
    while ((found = strstr(p, from_s)) != NULL) {
        size_t part = (size_t)(found - p);
        memcpy(rp, p, part); rp += part;
        memcpy(rp, to_s, tlen); rp += tlen;
        p = found + flen;
    }
    size_t rest = strlen(p);
    memcpy(rp, p, rest); rp[rest] = '\0';
    out->tag = TAG_STR; out->payload = (uint64_t)result;
}

void nova_split(NovaValue* s, NovaValue* sep, NovaValue* out) {
    nova_make_array(out);
    if (s->tag != TAG_STR) return;
    const char* str   = (const char*)s->payload;
    const char* delim = sep->tag == TAG_STR ? (const char*)sep->payload : " ";
    size_t dlen = strlen(delim);
    if (dlen == 0) {
        for (const char* cp = str; *cp; cp++) {
            char* ch = nova_alloc(2); ch[0] = *cp; ch[1] = '\0';
            NovaValue v; nova_make_str(ch, &v);
            nova_array_append(out, &v);
        }
        return;
    }
    const char* p = str;
    const char* found;
    while ((found = strstr(p, delim)) != NULL) {
        size_t part_len = (size_t)(found - p);
        char* part = nova_alloc(part_len + 1);
        memcpy(part, p, part_len); part[part_len] = '\0';
        NovaValue v; nova_make_str(part, &v);
        nova_array_append(out, &v);
        p = found + dlen;
    }
    NovaValue last; nova_make_str(nova_strdup(p), &last);
    nova_array_append(out, &last);
}

void nova_join(NovaValue* arr, NovaValue* sep, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { char* s = nova_strdup(""); out->tag = TAG_STR; out->payload = (uint64_t)s; return; }
    NovaArray* a = (NovaArray*)arr->payload;
    const char* delim = sep->tag == TAG_STR ? (const char*)sep->payload : "";
    size_t dlen = strlen(delim);
    if (a->length == 0) { char* s = nova_strdup(""); out->tag = TAG_STR; out->payload = (uint64_t)s; return; }
    // strs/lens are temporary work buffers freed before return — use plain malloc
    char** strs = malloc((size_t)a->length * sizeof(char*));
    size_t* lens = malloc((size_t)a->length * sizeof(size_t));
    size_t total = 0;
    for (int64_t i = 0; i < a->length; i++) {
        strs[i] = value_to_cstr(&a->data[i]); // plain malloc — freed in loop below
        lens[i] = strlen(strs[i]);
        total += lens[i];
    }
    total += dlen * (size_t)(a->length - 1);
    char* result = nova_alloc(total + 1); // tracked — becomes the output string
    char* rp = result;
    for (int64_t i = 0; i < a->length; i++) {
        memcpy(rp, strs[i], lens[i]); rp += lens[i];
        free(strs[i]);
        if (i < a->length - 1) { memcpy(rp, delim, dlen); rp += dlen; }
    }
    *rp = '\0';
    free(strs); free(lens);
    out->tag = TAG_STR; out->payload = (uint64_t)result;
}

// Array operations

static int nova_value_cmp(const void* a, const void* b) {
    const NovaValue* va = (const NovaValue*)a;
    const NovaValue* vb = (const NovaValue*)b;
    if (va->tag == TAG_INT && vb->tag == TAG_INT) {
        int64_t ia = (int64_t)va->payload, ib = (int64_t)vb->payload;
        return (ia > ib) - (ia < ib);
    }
    if ((va->tag == TAG_INT || va->tag == TAG_FLOAT) &&
        (vb->tag == TAG_INT || vb->tag == TAG_FLOAT)) {
        double da = va->tag == TAG_FLOAT ? bits_to_f64(va->payload) : (double)(int64_t)va->payload;
        double db = vb->tag == TAG_FLOAT ? bits_to_f64(vb->payload) : (double)(int64_t)vb->payload;
        return (da > db) - (da < db);
    }
    char* sa = value_to_cstr(va), *sb = value_to_cstr(vb);
    int cmp = strcmp(sa, sb);
    free(sa); free(sb);
    return cmp;
}

void nova_sort(NovaValue* arr, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { nova_make_nil(out); return; }
    NovaArray* src = (NovaArray*)arr->payload;
    nova_make_array(out);
    for (int64_t i = 0; i < src->length; i++) nova_array_append(out, &src->data[i]);
    NovaArray* dst = (NovaArray*)out->payload;
    qsort(dst->data, (size_t)dst->length, sizeof(NovaValue), nova_value_cmp);
}

void nova_reverse(NovaValue* arr, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { nova_make_nil(out); return; }
    NovaArray* src = (NovaArray*)arr->payload;
    nova_make_array(out);
    for (int64_t i = src->length - 1; i >= 0; i--) nova_array_append(out, &src->data[i]);
}

void nova_pop(NovaValue* arr, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { nova_make_nil(out); return; }
    NovaArray* a = (NovaArray*)arr->payload;
    if (a->length == 0) { nova_make_nil(out); return; }
    *out = a->data[--a->length];
}

// Map operations

void nova_keys(NovaValue* map, NovaValue* out) {
    nova_make_array(out);
    if (map->tag != TAG_MAP) return;
    NovaMap* m = (NovaMap*)map->payload;
    for (NovaEntry* e = m->head; e; e = e->next) {
        NovaValue k; nova_make_str(e->key, &k);
        nova_array_append(out, &k);
    }
}

void nova_values(NovaValue* map, NovaValue* out) {
    nova_make_array(out);
    if (map->tag != TAG_MAP) return;
    NovaMap* m = (NovaMap*)map->payload;
    for (NovaEntry* e = m->head; e; e = e->next) nova_array_append(out, &e->val);
}

// setKey(map, key, val) — insert/update key in the map, return the map
void nova_set_key(NovaValue* map, NovaValue* key, NovaValue* val, NovaValue* out) {
    nova_map_insert(map, key, val);
    *out = *map;
}

// sum(arr) — sum all numeric elements; returns int if all elements are int, float otherwise
void nova_sum(NovaValue* arr, NovaValue* out) {
    if (arr->tag != TAG_ARRAY) { nova_make_int(0, out); return; }
    NovaArray* a = (NovaArray*)arr->payload;
    int has_float = 0;
    for (int64_t i = 0; i < a->length; i++) {
        if (a->data[i].tag == TAG_FLOAT) { has_float = 1; break; }
    }
    if (has_float) {
        double s = 0.0;
        for (int64_t i = 0; i < a->length; i++) {
            if      (a->data[i].tag == TAG_INT)   s += (double)(int64_t)a->data[i].payload;
            else if (a->data[i].tag == TAG_FLOAT) s += bits_to_f64(a->data[i].payload);
        }
        nova_make_float(s, out);
    } else {
        int64_t s = 0;
        for (int64_t i = 0; i < a->length; i++) {
            if (a->data[i].tag == TAG_INT) s += (int64_t)a->data[i].payload;
        }
        nova_make_int(s, out);
    }
}

// map(arr, fn) — apply fn to every element, return new array of results
void nova_hof_map(NovaValue* arr, NovaValue* fn_val, NovaValue* out) {
    nova_make_array(out);
    if (arr->tag != TAG_ARRAY || fn_val->tag != TAG_CLOSURE) return;
    NovaArray* src = (NovaArray*)arr->payload;
    for (int64_t i = 0; i < src->length; i++) {
        NovaValue elem_result;
        nova_invoke_closure(fn_val, &src->data[i], 1, &elem_result);
        nova_array_append(out, &elem_result);
    }
}

// filter(arr, fn) — return new array containing only elements where fn(elem) is truthy
void nova_hof_filter(NovaValue* arr, NovaValue* fn_val, NovaValue* out) {
    nova_make_array(out);
    if (arr->tag != TAG_ARRAY || fn_val->tag != TAG_CLOSURE) return;
    NovaArray* src = (NovaArray*)arr->payload;
    for (int64_t i = 0; i < src->length; i++) {
        NovaValue test;
        nova_invoke_closure(fn_val, &src->data[i], 1, &test);
        if (nova_truthy(&test)) nova_array_append(out, &src->data[i]);
    }
}

// Character operations

void nova_ord(NovaValue* s, NovaValue* out) {
    if (s->tag != TAG_STR) { nova_make_int(0, out); return; }
    nova_make_int((int64_t)(unsigned char)((const char*)s->payload)[0], out);
}

void nova_chr(NovaValue* n, NovaValue* out) {
    int64_t code = n->tag == TAG_INT ? (int64_t)n->payload : 0;
    char* s = nova_alloc(2); s[0] = (char)(code & 0xFF); s[1] = '\0';
    out->tag = TAG_STR; out->payload = (uint64_t)s;
}

// I/O

void nova_println(NovaValue* v, NovaValue* out) {
    print_value(v);
    printf("\n");
    nova_make_nil(out);
}

void nova_printn(NovaValue* v, NovaValue* out) {
    print_value(v);
    nova_make_nil(out);
}

void nova_read_file(NovaValue* path, NovaValue* out) {
    if (path->tag != TAG_STR) { nova_make_nil(out); return; }
    FILE* f = fopen((const char*)path->payload, "r");
    if (!f) { nova_make_nil(out); return; }
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    char* buf = nova_alloc((size_t)size + 1);
    size_t nread = fread(buf, 1, (size_t)size, f);
    buf[nread] = '\0';
    fclose(f);
    out->tag = TAG_STR; out->payload = (uint64_t)buf;
}

void nova_write_file(NovaValue* path, NovaValue* content, NovaValue* out) {
    if (path->tag != TAG_STR) { nova_make_nil(out); return; }
    FILE* f = fopen((const char*)path->payload, "w");
    if (!f) { nova_make_nil(out); return; }
    const char* data = content->tag == TAG_STR ? (const char*)content->payload : "";
    fputs(data, f);
    fclose(f);
    nova_make_nil(out);
}

void nova_input(NovaValue* prompt, NovaValue* out) {
    if (prompt->tag == TAG_STR) { printf("%s", (const char*)prompt->payload); fflush(stdout); }
    char buf[4096];
    if (fgets(buf, sizeof(buf), stdin)) {
        size_t len = strlen(buf);
        if (len > 0 && buf[len - 1] == '\n') buf[len - 1] = '\0';
        out->tag = TAG_STR; out->payload = (uint64_t)nova_strdup(buf);
    } else {
        nova_make_nil(out);
    }
}

// ── throw / try / catch
//
// Nova uses a global flag+value to propagate exceptions. When nova_throw() is
// called, it sets g_thrown_flag and stores the thrown value in g_thrown_value.
// The LLVM codegen emits a nova_is_thrown() check after each statement inside
// a try block; if the flag is set, control jumps to the catch block, which
// calls nova_get_thrown() to retrieve and clear the thrown value.
//
// Throws that happen *directly* in the try body (via `throw expr`) branch
// straight to the catch label via LLVM br. Throws from called functions are
// caught by the post-statement flag check.
//
// Not thread-safe across spawned tasks by design: throw/catch is within-task
// control flow only. Each task would need its own flag if cross-task exception
// propagation were desired (it is not part of Nova's design).

static NovaValue g_thrown_value = {0, 0};
static int       g_thrown_flag  = 0;

// Set the global thrown flag and store val as the active exception.
void nova_throw(const NovaValue* val) {
    nova_copy(val, &g_thrown_value);
    g_thrown_flag = 1;
}

// Return 1 if a throw is in-flight, 0 otherwise.
int nova_is_thrown(void) {
    return g_thrown_flag;
}

// Retrieve the thrown value into *out and reset the flag.
// Called at the start of a catch block.
void nova_get_thrown(NovaValue* out) {
    nova_copy(&g_thrown_value, out);
    g_thrown_flag  = 0;
    g_thrown_value.tag     = 0;
    g_thrown_value.payload = 0;
}
