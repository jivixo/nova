// evaluator.rs — tree-walking interpreter. The --tree backend.
//
// eval(expr, env) takes one AST node and an environment (variable map) and returns a Value.
// The environment is passed as &mut so let/assign statements can add/update variables.
// eval is called recursively — evaluating a BinaryOp calls eval on its left and right sub-trees.
//
// Memory model:
//   Arrays and HashMaps are wrapped in Rc<RefCell<...>>
//   Rc = reference counting — cloning a Value::Array is O(1) (just increments the counter)
//   RefCell = allows mutation at runtime despite Rust's borrow rules
//   Copy-on-Write (CoW): when writing to an array, if Rc::strong_count > 1 (shared),
//   clone the inner data first so other variables are unaffected
//
// Cycle detection:
//   collect_cycles(env) runs after every top-level statement in file mode.
//   It marks everything reachable from env, then clears the contents of any unreachable
//   Rc objects (breaking the reference cycle so their count can finally reach 0).
//   In practice CoW prevents cycles from forming, so cycles_collected stays 0 for most programs.
//
// This file is the reference implementation. The VM (vm.rs + compiler.rs) replicates its
// semantics in bytecode form and should always produce identical output for the same input.
use std::collections::{HashMap, HashSet};
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use crossbeam_channel::{Sender, Receiver, TryRecvError, Select};

pub static NEEDS_NEWLINE: AtomicBool = AtomicBool::new(false);

thread_local! {
    // holds the thrown value until a try/catch block catches it
    static THROWN: RefCell<Option<Value>> = RefCell::new(None);
    // tracks files currently being imported to prevent circular imports
    static IMPORTING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    // every allocated array/map registers a Weak here so the cycle collector can scan them
    static ARRAY_HEAP: RefCell<Vec<Weak<Mutex<Vec<Value>>>>> = RefCell::new(Vec::new());
    static MAP_HEAP: RefCell<Vec<Weak<Mutex<Vec<(Value, Value)>>>>> = RefCell::new(Vec::new());
    // method registry: type_name → method_name → (params, body)
    // populated by ImplBlock; looked up by MethodCall
    static METHODS: RefCell<HashMap<String, HashMap<String, (Vec<String>, Vec<Expr>)>>> =
        RefCell::new(HashMap::new());
}
use crate::lexer::{Token, Lexer};
use crate::parser::{Expr, Parser};
use crate::error::nova_error;

// tracks the line number of the statement currently being evaluated
// updated every time eval hits an Expr::Line node so errors can report the right line
thread_local! {
    static CURRENT_LINE: Cell<usize> = Cell::new(0);
    static CALL_DEPTH: Cell<usize> = Cell::new(0);
    // memory stats — updated by make_array, make_map, and collect_cycles
    static ARRAYS_ALLOCATED: Cell<usize> = Cell::new(0);
    static MAPS_ALLOCATED:   Cell<usize> = Cell::new(0);
    static CYCLES_COLLECTED: Cell<usize> = Cell::new(0); // objects cleared by the cycle collector
    static PEAK_LIVE:        Cell<usize> = Cell::new(0); // highest live object count seen at any GC point
    // stack of local-declaration sets — one entry per active (non-closure) call frame.
    // lets inside a fn body register here so write-back doesn't promote them to globals.
    static LOCAL_DECLS: RefCell<Vec<HashSet<String>>> = RefCell::new(vec![]);
    // defer stack — one Vec per active function call; each Vec holds (expr, env_snapshot) pairs
    // pushed by `defer` statements; drained LIFO on function return.
    static DEFERRED: RefCell<Vec<Vec<(Expr, HashMap<String, Value>)>>> = RefCell::new(vec![]);
}

fn set_eval_line(line: usize) {
    CURRENT_LINE.with(|l| l.set(line));
}

fn eval_line() -> usize {
    CURRENT_LINE.with(|l| l.get())
}

fn push_local_frame() {
    LOCAL_DECLS.with(|s| s.borrow_mut().push(HashSet::new()));
}

fn pop_local_frame() -> HashSet<String> {
    LOCAL_DECLS.with(|s| s.borrow_mut().pop().unwrap_or_default())
}

fn declare_local(name: &str) {
    LOCAL_DECLS.with(|s| {
        if let Some(top) = s.borrow_mut().last_mut() {
            top.insert(name.to_string());
        }
    });
}

// Value is what Nova expressions evaluate to at runtime.
// Every eval call returns one of these.
//
// Array and HashMap are wrapped in Rc<RefCell<...>> for reference counting.
// Rc = reference counted pointer — cloning a Value::Array is cheap (just increments a counter).
// RefCell = allows interior mutability — lets us borrow the inner Vec at runtime.
// Together they enable copy-on-write: arrays are shared until one is written to,
// at which point Nova makes a private copy before mutating (see IndexAssign).
// Inner state for a first-class channel value.
// Wraps a crossbeam bounded channel pair so both ends can be held in a single Nova value.
// Wrapped in Arc so cloning a Value::Channel is cheap and all copies share the same channel.
pub struct ChannelInner {
    pub sender:   Sender<Value>,
    pub receiver: Receiver<Value>,
    // mirrors every in-transit value so the GC can mark heap objects reachable
    pub pending:  Mutex<Vec<Value>>,
}

impl std::fmt::Debug for ChannelInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<channel>")
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Array(Arc<Mutex<Vec<Value>>>),            // reference-counted array with CoW on write
    HashMap(Arc<Mutex<Vec<(Value, Value)>>>), // reference-counted hashmap, stored as key-value pairs
    Range(i64, i64),              // a range like 0..10 — stored as (start, end) integers
    Nil,                          // the "no value" value — returned by let, print, while, etc.
    Break,                        // internal signal — bubbles up through eval_block to exit a loop
    Continue,                     // internal signal — bubbles up through eval_block to skip to next iteration
    Return(Box<Value>),           // internal signal — bubbles up to the function call site and unwrapped there
    Function {
        params: Vec<String>,              // parameter names in order
        param_types: Vec<Option<String>>, // per-param declared type annotations (None = unannotated)
        defaults: Vec<Option<Value>>,     // per-param default values (None = required)
        body: Vec<crate::parser::Expr>,   // the function body (cloned from the AST when called)
        variadic: bool,                   // true if the last param collects all extra args as an array
        captured_env: Option<Arc<Mutex<Env>>>, // Some for lambdas — shared mutable scope so mutations persist across calls
    },
    VmFunc(u64), // handle to a VM-compiled lambda; the u64 is a key into the VM's closure table
    Channel(Arc<ChannelInner>), // first-class channel for cross-thread message passing
    Task(Arc<Receiver<Result<Value, String>>>), // handle to a spawned task; recv() blocks until done
    Mutex(Arc<ChannelInner>),  // mutual exclusion lock — bounded(1) channel used as a semaphore
    StructDef { name: String, field_names: Vec<String> },
    Struct { name: String, fields: Arc<Mutex<HashMap<String, Value>>> },
    EnumDef { name: String, variants: Vec<(String, usize)> },
    EnumVariant { enum_name: String, variant: String, payload: Vec<Value> },
    EnumConstructor { enum_name: String, variant: String, arity: usize },
}

// Env maps variable names to their current values.
// It's passed through every eval call as mutable so let statements can add new variables.
// type alias — Env is just a shorthand for HashMap<String, Value>
pub type Env = HashMap<String, Value>;

// Convenience constructors — wraps a Vec in Rc<RefCell> so callers don't repeat the boilerplate.
// Every array or hashmap created at runtime goes through one of these.
// Also registers a Weak pointer in the heap registry so collect_cycles can find it later.
pub fn make_array(v: Vec<Value>) -> Value {
    let rc = Arc::new(Mutex::new(v));
    ARRAY_HEAP.with(|h| h.borrow_mut().push(Arc::downgrade(&rc)));
    ARRAYS_ALLOCATED.with(|c| c.set(c.get() + 1));
    Value::Array(rc)
}

pub fn make_map(v: Vec<(Value, Value)>) -> Value {
    let rc = Arc::new(Mutex::new(v));
    MAP_HEAP.with(|h| h.borrow_mut().push(Arc::downgrade(&rc)));
    MAPS_ALLOCATED.with(|c| c.set(c.get() + 1));
    Value::HashMap(rc)
}

// Deep-clone a value — creates a fully independent copy with no shared Arc pointers.
// Used when sending values across thread boundaries (channel send, spawn args) so each
// thread owns its own data and the GC can collect independently per thread.
pub fn deep_clone(v: &Value) -> Value {
    match v {
        // primitives — plain Clone is already a full copy
        Value::Int(n)    => Value::Int(*n),
        Value::Float(n)  => Value::Float(*n),
        Value::Str(s)    => Value::Str(s.clone()),
        Value::Bool(b)   => Value::Bool(*b),
        Value::Nil       => Value::Nil,
        Value::Range(a, b) => Value::Range(*a, *b),

        // internal signals — should never cross thread boundaries, clone as-is
        Value::Break     => Value::Break,
        Value::Continue  => Value::Continue,
        Value::Return(v) => Value::Return(Box::new(deep_clone(v))),

        // arrays — new Arc, deep-clone every element
        Value::Array(arc) => {
            let inner = arc.lock().unwrap();
            make_array(inner.iter().map(deep_clone).collect())
        }

        // hashmaps — new Arc, deep-clone every key-value pair
        Value::HashMap(arc) => {
            let inner = arc.lock().unwrap();
            make_map(inner.iter().map(|(k, v)| (deep_clone(k), deep_clone(v))).collect())
        }

        // structs — new Arc, deep-clone every field value
        Value::Struct { name, fields } => {
            let inner = fields.lock().unwrap();
            let cloned: HashMap<String, Value> = inner.iter()
                .map(|(k, v)| (k.clone(), deep_clone(v)))
                .collect();
            Value::Struct { name: name.clone(), fields: Arc::new(Mutex::new(cloned)) }
        }

        // functions — body is read-only AST (safe to share); captured_env gets a fresh Arc
        Value::Function { params, param_types, defaults, body, variadic, captured_env } => {
            let cloned_env = captured_env.as_ref().map(|arc| {
                let inner = arc.lock().unwrap();
                let cloned: Env = inner.iter().map(|(k, v)| (k.clone(), deep_clone(v))).collect();
                Arc::new(Mutex::new(cloned))
            });
            Value::Function {
                params: params.clone(),
                param_types: param_types.clone(),
                defaults: defaults.iter().map(|d| d.as_ref().map(deep_clone)).collect(),
                body: body.clone(),
                variadic: *variadic,
                captured_env: cloned_env,
            }
        }

        // enum payload values need deep-cloning
        Value::EnumVariant { enum_name, variant, payload } => Value::EnumVariant {
            enum_name: enum_name.clone(),
            variant: variant.clone(),
            payload: payload.iter().map(deep_clone).collect(),
        },

        // channels are shared by Arc — both ends live in the same ChannelInner;
        // deep_clone shares the channel so sender and receiver stay connected across threads
        Value::Channel(arc) => Value::Channel(arc.clone()),
        // mutexes are shared by Arc — sharing is required so lock/unlock see the same semaphore
        Value::Mutex(arc)   => Value::Mutex(arc.clone()),
        // tasks hold a channel receiver — share the Arc so wait() can be called from any alias
        Value::Task(arc) => Value::Task(arc.clone()),

        // these carry no mutable heap data — clone as-is
        Value::VmFunc(id)                          => Value::VmFunc(*id),
        Value::StructDef { name, field_names }     => Value::StructDef { name: name.clone(), field_names: field_names.clone() },
        Value::EnumDef { name, variants }          => Value::EnumDef { name: name.clone(), variants: variants.clone() },
        Value::EnumConstructor { enum_name, variant, arity } => Value::EnumConstructor {
            enum_name: enum_name.clone(), variant: variant.clone(), arity: *arity
        },
    }
}

// Like deep_clone but does NOT register arrays/maps in ARRAY_HEAP/MAP_HEAP.
// Used for spawn args: values created here are owned entirely by the spawned thread.
// If make_array were used, the parent thread's GC would see the Arc in its HEAP but not
// in its env, mark it unreachable, and clear it while the spawned thread is still using it.
fn deep_clone_unregistered(v: &Value) -> Value {
    match v {
        Value::Int(n)      => Value::Int(*n),
        Value::Float(n)    => Value::Float(*n),
        Value::Str(s)      => Value::Str(s.clone()),
        Value::Bool(b)     => Value::Bool(*b),
        Value::Nil         => Value::Nil,
        Value::Range(a, b) => Value::Range(*a, *b),
        Value::Break       => Value::Break,
        Value::Continue    => Value::Continue,
        Value::Return(v)   => Value::Return(Box::new(deep_clone_unregistered(v))),
        Value::Array(arc) => {
            let inner = arc.lock().unwrap();
            Value::Array(Arc::new(Mutex::new(inner.iter().map(deep_clone_unregistered).collect())))
        }
        Value::HashMap(arc) => {
            let inner = arc.lock().unwrap();
            Value::HashMap(Arc::new(Mutex::new(inner.iter().map(|(k, v)| (deep_clone_unregistered(k), deep_clone_unregistered(v))).collect())))
        }
        Value::Struct { name, fields } => {
            let inner = fields.lock().unwrap();
            let cloned: HashMap<String, Value> = inner.iter()
                .map(|(k, v)| (k.clone(), deep_clone_unregistered(v)))
                .collect();
            Value::Struct { name: name.clone(), fields: Arc::new(Mutex::new(cloned)) }
        }
        Value::Function { params, param_types, defaults, body, variadic, captured_env } => {
            let cloned_env = captured_env.as_ref().map(|arc| {
                let inner = arc.lock().unwrap();
                let cloned: Env = inner.iter().map(|(k, v)| (k.clone(), deep_clone_unregistered(v))).collect();
                Arc::new(Mutex::new(cloned))
            });
            Value::Function {
                params: params.clone(), param_types: param_types.clone(),
                defaults: defaults.iter().map(|d| d.as_ref().map(deep_clone_unregistered)).collect(),
                body: body.clone(), variadic: *variadic, captured_env: cloned_env,
            }
        }
        Value::EnumVariant { enum_name, variant, payload } => Value::EnumVariant {
            enum_name: enum_name.clone(), variant: variant.clone(),
            payload: payload.iter().map(deep_clone_unregistered).collect(),
        },
        Value::Channel(arc) => Value::Channel(arc.clone()),
        Value::Mutex(arc)   => Value::Mutex(arc.clone()),
        Value::Task(arc)    => Value::Task(arc.clone()), // share receiver so wait() works from any alias
        Value::VmFunc(id)   => Value::VmFunc(*id),
        Value::StructDef { name, field_names } => Value::StructDef { name: name.clone(), field_names: field_names.clone() },
        Value::EnumDef { name, variants }      => Value::EnumDef { name: name.clone(), variants: variants.clone() },
        Value::EnumConstructor { enum_name, variant, arity } => Value::EnumConstructor {
            enum_name: enum_name.clone(), variant: variant.clone(), arity: *arity
        },
    }
}

// Walk the value graph starting from val, recording every Rc address we visit in `reachable`.
// Using the raw pointer address as the object ID lets us track identity across clones.
// The visited check (reachable.insert returns false if already present) breaks infinite loops
// when values form cycles — e.g. a[0] = a.
fn mark_reachable(val: &Value, reachable: &mut HashSet<usize>) {
    match val {
        Value::Array(arr) => {
            let ptr = Arc::as_ptr(arr) as usize;
            if reachable.insert(ptr) { // false if already visited — stops cycles
                let items = arr.lock().unwrap().clone();
                for item in &items {
                    mark_reachable(item, reachable);
                }
            }
        }
        Value::HashMap(map) => {
            let ptr = Arc::as_ptr(map) as usize;
            if reachable.insert(ptr) {
                let pairs = map.lock().unwrap().clone();
                for (k, v) in &pairs {
                    mark_reachable(k, reachable);
                    mark_reachable(v, reachable);
                }
            }
        }
        // lambdas carry a captured scope — scan it too so its arrays are marked live
        Value::Function { captured_env: Some(arc), .. } => {
            for v in arc.lock().unwrap().values() {
                mark_reachable(v, reachable);
            }
        }
        // enum variants carry payload values — scan them so nested arrays/maps aren't collected
        Value::EnumVariant { payload, .. } => {
            for v in payload {
                mark_reachable(v, reachable);
            }
        }
        // struct fields may contain arrays/maps — scan them too
        Value::Struct { fields, .. } => {
            for v in fields.lock().unwrap().values() {
                mark_reachable(v, reachable);
            }
        }
        // walk in-transit values so the GC doesn't collect arrays/maps mid-flight
        Value::Channel(arc) => {
            for v in arc.pending.lock().unwrap().iter() {
                mark_reachable(v, reachable);
            }
        }
        // mutex holds the current locked-in value in its pending buffer
        Value::Mutex(arc) => {
            for v in arc.pending.lock().unwrap().iter() {
                mark_reachable(v, reachable);
            }
        }
        Value::Task(_) => {}
        _ => {}
    }
}

// Cycle collector — finds arrays and maps that are unreachable from the environment
// and frees them by clearing their contents (which breaks the reference cycle so Rc
// counts can reach zero).
//
// How it works:
//   1. Every make_array / make_map registers a Weak pointer in ARRAY_HEAP / MAP_HEAP.
//   2. We upgrade all live Weaks to temporary Rcs so we have a snapshot of every object.
//   3. We walk from the env (the roots) and mark everything we can reach.
//   4. Any object NOT reachable from the env is cyclic garbage — we clear its Vec,
//      which drops the internal references and lets Rc counts fall to zero.
//   5. The temporary Rcs from step 2 drop, and freed objects are deallocated.
//   6. Dead Weaks are pruned from the registry.
//
// Safe to call between statements, when no Nova Values live on the Rust call stack
// outside of env.
pub fn collect_cycles(env: &Env) {
    // Step 1 — snapshot every live heap object as a strong Arc
    let live_arrays: Vec<Arc<Mutex<Vec<Value>>>> =
        ARRAY_HEAP.with(|h| h.borrow().iter().filter_map(|w| w.upgrade()).collect());
    let live_maps: Vec<Arc<Mutex<Vec<(Value, Value)>>>> =
        MAP_HEAP.with(|h| h.borrow().iter().filter_map(|w| w.upgrade()).collect());

    // Track peak — record the live count before any clearing happens this GC run
    let current_live = live_arrays.len() + live_maps.len();
    PEAK_LIVE.with(|p| if current_live > p.get() { p.set(current_live) });

    // Step 2 — mark everything reachable from the environment
    let mut reachable: HashSet<usize> = HashSet::new();
    for val in env.values() {
        mark_reachable(val, &mut reachable);
    }

    // Step 3 — clear any heap object not reachable from the env
    // Clearing drops the internal Values, which decrements Rc counts for anything they point to.
    // This breaks cycles: once all internal references drop, the Rc count hits zero and memory is freed.
    let mut cleared = 0;
    for rc in &live_arrays {
        let ptr = Arc::as_ptr(rc) as usize;
        if !reachable.contains(&ptr) {
            rc.lock().unwrap().clear();
            cleared += 1;
        }
    }
    for rc in &live_maps {
        let ptr = Arc::as_ptr(rc) as usize;
        if !reachable.contains(&ptr) {
            rc.lock().unwrap().clear();
            cleared += 1;
        }
    }
    CYCLES_COLLECTED.with(|c| c.set(c.get() + cleared));

    // Step 4 — live_arrays and live_maps drop here; any Rc now at count 0 is freed

    // Step 5 — prune dead Weak pointers from the registries
    ARRAY_HEAP.with(|h| h.borrow_mut().retain(|w| w.upgrade().is_some()));
    MAP_HEAP.with(|h| h.borrow_mut().retain(|w| w.upgrade().is_some()));
}

// Builds the memory report string — called at end of program when --memory flag is set.
pub fn memory_report() -> String {
    let arrays_alloc = ARRAYS_ALLOCATED.with(|c| c.get());
    let maps_alloc   = MAPS_ALLOCATED.with(|c| c.get());
    let cycles       = CYCLES_COLLECTED.with(|c| c.get());
    let peak         = PEAK_LIVE.with(|c| c.get());
    let live_arrays  = ARRAY_HEAP.with(|h| h.borrow().iter().filter(|w| w.upgrade().is_some()).count());
    let live_maps    = MAP_HEAP.with(|h| h.borrow().iter().filter(|w| w.upgrade().is_some()).count());

    format!(
        "\n--- memory report ---\narrays allocated:    {}\nhashmaps allocated:  {}\ncycles collected:    {}\nlive arrays:         {}\nlive hashmaps:       {}\npeak live objects:   {}",
        arrays_alloc, maps_alloc, cycles, live_arrays, live_maps, peak
    )
}

// The main eval function — walks the AST and computes the final value.
pub fn eval(expr: &Expr, env: &mut Env) -> Value {
    // if a throw is in flight, skip all further evaluation until a try/catch catches it
    if THROWN.with(|t| t.borrow().is_some()) {
        return Value::Nil;
    }

    match expr {
        // Literals just return their value directly — nothing to compute
        Expr::IntLit(n)   => Value::Int(*n),
        Expr::FloatLit(n) => Value::Float(*n),
        Expr::BoolLit(b) => Value::Bool(*b),
        Expr::NilLit     => Value::Nil,
        Expr::StrLit(s)  => Value::Str(s.clone()), // clone because we can't move out of &Expr

        // Interpolated string — walk the parts and substitute {expr} with evaluated results.
        // Each {expr} can be a variable, function call, index access, or any valid Nova expression.
        Expr::StrInterp(parts) => {
            let mut result = String::new();
            for part in parts {
                match part {
                    crate::lexer::StringPart::Literal(s) => result.push_str(s),
                    crate::lexer::StringPart::Interp(expr_text) => {
                        if expr_text.is_empty() { continue; }
                        // parse the expression text as Nova, then evaluate it in the current env
                        let mut lex = Lexer::new(expr_text);
                        let tokens = lex.tokenize();
                        let mut parser = Parser::new(tokens);
                        let expr = parser.parse_expression();
                        let val = eval(&expr, env);
                        result.push_str(&format_value(&val));
                    }
                }
            }
            Value::Str(result)
        }

        // Variable read — look up the name in the environment
        Expr::Ident(name) => env
            .get(name)
            .cloned()
            .unwrap_or_else(|| {
                let candidates: Vec<&str> = env.keys().map(|s| s.as_str()).collect();
                let suggestion = did_you_mean(name, &candidates)
                    .map(|s| format!("\n  Did you mean '{}'?", s))
                    .unwrap_or_default(); // empty string if no suggestion found
                nova_error(eval_line(), &format!("undefined variable '{}'{}", name, suggestion))
            }),

        // Variable declaration — evaluate the right side, store it in env, return Nil
        Expr::Let { name, value } => {
            let val = eval(value, env);
            env.insert(name.clone(), val);
            declare_local(name);
            Value::Nil
        }

        // Reassignment — update an existing variable; error if it was never declared with let
        Expr::Assign { name, value } => {
            if !env.contains_key(name) {
                nova_error(eval_line(), &format!("'{}' is not declared — use 'let {} = ...' first", name, name));
            }
            let val = eval(value, env);
            env.insert(name.clone(), val);
            Value::Nil
        }

        // Print — evaluate, print with newline, return Nil
        Expr::Print(expr) => {
            let val = eval(expr, env);
            if THROWN.with(|t| t.borrow().is_some()) { return Value::Nil; }
            println!("{}", format_value(&val));
            NEEDS_NEWLINE.store(false, Ordering::Relaxed);
            Value::Nil
        }

        // Printn — evaluate, print WITHOUT newline, return Nil
        Expr::Printn(expr) => {
            let val = eval(expr, env);
            if THROWN.with(|t| t.borrow().is_some()) { return Value::Nil; }
            print!("{}", format_value(&val)); // print! not println! — no newline
            use std::io::Write;
            std::io::stdout().flush().unwrap(); // flush so output appears immediately
            NEEDS_NEWLINE.store(true, Ordering::Relaxed);
            Value::Nil
        }

        // If — evaluate condition, run the matching block
        Expr::If { condition, then_block, else_block } => {
            let cond = match eval(condition, env) {
                Value::Bool(b) => b,
                _ => nova_error(eval_line(), "if condition must be a boolean"),
            };
            if cond {
                eval_block(then_block, env)
            } else if let Some(else_b) = else_block {
                eval_block(else_b, env)
            } else {
                Value::Nil
            }
        }

        // Line wrapper — update the current line tracker, then evaluate the inner expression
        Expr::Line(line, inner) => {
            set_eval_line(*line);
            eval(inner, env)
        }

        // !expr — logical NOT, flips a boolean
        Expr::Not(expr) => {
            match eval(expr, env) {
                Value::Bool(b) => Value::Bool(!b),
                _ => nova_error(eval_line(), "! requires a boolean"),
            }
        }

        // break — produces the Break sentinel; eval_block propagates it up to the loop
        Expr::Break => Value::Break,

        // continue — produces the Continue sentinel; eval_block propagates it up to the loop
        Expr::Continue => Value::Continue,

        // return expr — wraps the evaluated value in Return; bubbles up through eval_block
        // until it reaches the function call handler, which unwraps it
        Expr::Return(expr) => Value::Return(Box::new(eval(expr, env))),

        // defer expr — snapshot the current env and register expr to run LIFO when the
        // enclosing function returns. Has no effect at the top level (no function frame active).
        Expr::Defer(inner) => {
            let snapshot = env.clone();
            DEFERRED.with(|d| {
                if let Some(frame) = d.borrow_mut().last_mut() {
                    frame.push((*inner.clone(), snapshot));
                }
            });
            Value::Nil
        }

        // spawn expr — deep-clones the current env, evaluates expr on a new OS thread,
        // select { case ch -> v { body } ... default { body } }
        // without default: blocks until a channel fires.
        // with default: non-blocking — runs default body if no channel is ready right now.
        Expr::Select { arms, default_body } => {
            // Evaluate all channel expressions upfront
            let mut channel_arcs: Vec<Arc<ChannelInner>> = Vec::new();
            for (ch_expr, _, _) in arms {
                match eval(ch_expr, env) {
                    Value::Channel(arc) => channel_arcs.push(arc),
                    _ => nova_error(eval_line(), "select: each case must be a channel"),
                }
            }
            // Register all receivers with crossbeam's dynamic Select
            let mut sel = Select::new();
            let mut op_indices: Vec<usize> = Vec::new();
            for arc in &channel_arcs {
                op_indices.push(sel.recv(&arc.receiver));
            }
            if let Some(default_stmts) = default_body {
                // Non-blocking: try_select returns Err if nothing is ready
                match sel.try_select() {
                    Ok(oper) => {
                        let ready = oper.index();
                        let arm_idx = op_indices.iter().position(|&i| i == ready).unwrap();
                        let msg = oper.recv(&channel_arcs[arm_idx].receiver).unwrap_or(Value::Nil);
                        let (_, bind_var, body) = &arms[arm_idx];
                        env.insert(bind_var.clone(), msg);
                        eval_block(body, env)
                    }
                    Err(_) => eval_block(default_stmts, env),
                }
            } else {
                // Blocking: wait until a channel fires
                let oper = sel.select();
                let ready = oper.index();
                let arm_idx = op_indices.iter().position(|&i| i == ready).unwrap();
                let msg = oper.recv(&channel_arcs[arm_idx].receiver).unwrap_or(Value::Nil);
                let (_, bind_var, body) = &arms[arm_idx];
                env.insert(bind_var.clone(), msg);
                eval_block(body, env)
            }
        }

        // returns a Task handle that wait() can join
        Expr::Spawn(inner) => {
            let inner_expr = *inner.clone();
            let mut thread_env: Env = env.iter()
                .map(|(k, v)| (k.clone(), deep_clone_unregistered(v)))
                .collect();
            let (s, r) = crossbeam_channel::bounded::<Result<Value, String>>(1);
            rayon::spawn(move || {
                // clear any stale error from a previous task on this worker thread
                crate::error::LAST_ERROR.with(|e| *e.borrow_mut() = String::new());
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    eval(&inner_expr, &mut thread_env)
                })).map_err(|_| crate::error::LAST_ERROR.with(|e| e.borrow().clone()));
                let _ = s.send(result);
            });
            Value::Task(Arc::new(r))
        }

        // throw expr — stores the value in THROWN; the propagation guard at the top of eval
        // then short-circuits all further evaluation until a try/catch catches it
        Expr::Throw(expr) => {
            let val = eval(expr, env);
            THROWN.with(|t| *t.borrow_mut() = Some(val));
            Value::Nil
        }

        // try { body } catch name { handler }
        // Evaluates body statement by statement; after each one, checks whether THROWN was set.
        // If it was, clears it, binds the thrown value to catch_var, and runs the handler.
        // If no throw happens, the catch block is skipped entirely.
        Expr::Try { body, catch_var, catch_body } => {
            use std::panic;
            use crate::error::{LAST_ERROR, TRY_DEPTH};

            TRY_DEPTH.with(|d| d.set(d.get() + 1));
            let mut caught: Option<Value> = None;

            'try_block: for stmt in body {
                let env_ptr = env as *mut Env;
                let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                    eval(stmt, unsafe { &mut *env_ptr })
                }));
                match result {
                    Ok(val) => {
                        if matches!(val, Value::Return(_)) {
                            TRY_DEPTH.with(|d| d.set(d.get() - 1));
                            return val;
                        }
                        if let Some(thrown) = THROWN.with(|t| t.borrow_mut().take()) {
                            caught = Some(thrown);
                            break 'try_block;
                        }
                    }
                    Err(_) => {
                        let msg = LAST_ERROR.with(|e| e.borrow().clone());
                        THROWN.with(|t| t.borrow_mut().take());
                        caught = Some(Value::Str(msg));
                        break 'try_block;
                    }
                }
            }

            TRY_DEPTH.with(|d| d.set(d.get() - 1));

            if let Some(thrown_val) = caught {
                // save whatever was in the outer scope under catch_var (may be nothing)
                let outer = env.get(catch_var).cloned();
                env.insert(catch_var.clone(), thrown_val);
                for stmt in catch_body {
                    let r = eval(stmt, env);
                    if matches!(r, Value::Return(_)) {
                        // restore before propagating return
                        match outer { Some(v) => { env.insert(catch_var.clone(), v); } None => { env.remove(catch_var); } }
                        return r;
                    }
                }
                // restore — catch variable is scoped to the catch block
                match outer {
                    Some(v) => { env.insert(catch_var.clone(), v); }
                    None    => { env.remove(catch_var); }
                }
            }
            Value::Nil
        }

        // import "path.nova" — reads the file, runs it in the current env.
        // All functions and variables declared in the file become available in the caller's scope.
        // Circular imports are detected via IMPORTING and produce a runtime error.
        Expr::LetArrayDestructure { names, value } => {
            let val = eval(value, env);
            match val {
                Value::Array(arr) => {
                    let items = arr.lock().unwrap();
                    for (i, name) in names.iter().enumerate() {
                        let v = items.get(i).cloned().unwrap_or(Value::Nil);
                        env.insert(name.clone(), v);
                    }
                }
                _ => nova_error(eval_line(), "array destructuring requires an array on the right side"),
            }
            Value::Nil
        }

        Expr::LetMapDestructure { names, value } => {
            let val = eval(value, env);
            match val {
                Value::HashMap(map) => {
                    let m = map.lock().unwrap();
                    for name in names {
                        let key = Value::Str(name.clone());
                        let v = m.iter()
                            .find(|(k, _)| values_equal(k, &key))
                            .map(|(_, v)| v.clone())
                            .unwrap_or(Value::Nil);
                        env.insert(name.clone(), v);
                    }
                }
                _ => nova_error(eval_line(), "hashmap destructuring requires a hashmap on the right side"),
            }
            Value::Nil
        }

        Expr::Import(path) => {
            // canonicalize the path so "a.nova" and "./a.nova" are treated the same
            let canonical = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.clone());

            // circular import check — if this file is already being imported, stop
            let already_importing = IMPORTING.with(|s| s.borrow().contains(&canonical));
            if already_importing {
                nova_error(eval_line(), &format!("circular import detected: '{}'", path));
            }

            let source = std::fs::read_to_string(path).unwrap_or_else(|_| {
                nova_error(eval_line(), &format!("cannot import '{}': file not found", path));
            });

            IMPORTING.with(|s| s.borrow_mut().insert(canonical.clone()));

            let mut lex = Lexer::new(&source);
            let tokens = lex.tokenize();
            let mut parser = Parser::new(tokens);
            while !matches!(parser.current_token(), Token::EOF) {
                let stmt = parser.parse_statement();
                eval(&stmt, env);
            }

            IMPORTING.with(|s| s.borrow_mut().remove(&canonical));
            Value::Nil
        }

        Expr::StructDef { name, fields } => {
            env.insert(name.clone(), Value::StructDef { name: name.clone(), field_names: fields.clone() });
            Value::Nil
        }

        Expr::EnumDef { name, variants } => {
            env.insert(name.clone(), Value::EnumDef { name: name.clone(), variants: variants.clone() });
            Value::Nil
        }

        Expr::StructLit { name, fields } => {
            let mut map = HashMap::new();
            for (fname, fexpr) in fields {
                map.insert(fname.clone(), eval(fexpr, env));
            }
            Value::Struct { name: name.clone(), fields: Arc::new(Mutex::new(map)) }
        }

        Expr::FieldAccess { object, field } => {
            match eval(object, env) {
                Value::Struct { fields, .. } => {
                    fields.lock().unwrap().get(field).cloned()
                        .unwrap_or_else(|| nova_error(eval_line(), &format!("no field '{}' on struct", field)))
                }
                Value::EnumDef { name: enum_name, variants } => {
                    if let Some(&(_, arity)) = variants.iter().find(|(v, _)| v == field) {
                        if arity == 0 {
                            Value::EnumVariant { enum_name, variant: field.clone(), payload: vec![] }
                        } else {
                            Value::EnumConstructor { enum_name, variant: field.clone(), arity }
                        }
                    } else {
                        nova_error(eval_line(), &format!("no variant '{}' on enum '{}'", field, enum_name))
                    }
                }
                _ => nova_error(eval_line(), "field access requires a struct or enum"),
            }
        }

        Expr::FieldAssign { object, field, value } => {
            let new_val = eval(value, env);
            match eval(object, env) {
                Value::Struct { fields, .. } => {
                    fields.lock().unwrap().insert(field.clone(), new_val);
                }
                _ => nova_error(eval_line(), "field assignment requires a struct"),
            }
            Value::Nil
        }

        Expr::ImplBlock { type_name, methods } => {
            METHODS.with(|m| {
                let mut registry = m.borrow_mut();
                let entry = registry.entry(type_name.clone()).or_insert_with(HashMap::new);
                for method in methods {
                    // each method is wrapped in Expr::Line — unwrap to get the Fn inside
                    let func = match method {
                        Expr::Line(_, inner) => inner.as_ref(),
                        other => other,
                    };
                    if let Expr::Fn { name, params, body, .. } = func {
                        let param_names: Vec<String> = params.iter().map(|(p, _, _)| p.clone()).collect();
                        entry.insert(name.clone(), (param_names, body.clone()));
                    }
                }
            });
            Value::Nil
        }

        Expr::MethodCall { object, method, args } => {
            let receiver = eval(object, env);
            // EnumDef receiver means this is an enum constructor call: Shape.Circle(5)
            // The chaining loop in parse_primary turns `X.Y(args)` into MethodCall,
            // so we must handle EnumDef here the same way FieldAccess + DynCall used to.
            if let Value::EnumDef { name: enum_name, variants } = &receiver {
                if let Some(&(_, _arity)) = variants.iter().find(|(v, _)| v == method) {
                    let payload: Vec<Value> = args.iter().map(|a| eval(a, env)).collect();
                    return Value::EnumVariant { enum_name: enum_name.clone(), variant: method.clone(), payload };
                } else {
                    nova_error(eval_line(), &format!("no variant '{}' on enum '{}'", method, enum_name));
                }
            }
            let type_name = match &receiver {
                Value::Struct { name, .. } => name.clone(),
                Value::EnumVariant { enum_name, .. } => enum_name.clone(),
                _ => nova_error(eval_line(), "method calls are only supported on structs and enums"),
            };
            let (params, body) = METHODS.with(|m| {
                m.borrow()
                    .get(&type_name)
                    .and_then(|methods| methods.get(method.as_str()))
                    .cloned()
            }).unwrap_or_else(|| nova_error(eval_line(),
                &format!("no method '{}' on type '{}'", method, type_name)));

            let mut local_env = env.clone();
            // bind self first, then remaining args in order
            local_env.insert(params[0].clone(), receiver);
            for (param, arg_expr) in params.iter().skip(1).zip(args.iter()) {
                local_env.insert(param.clone(), eval(arg_expr, env));
            }
            let result = eval_block(&body, &mut local_env);
            match result {
                Value::Return(v) => *v,
                other => other,
            }
        }

        // While — re-evaluate condition each iteration, run body until false, break, or continue
        Expr::While { condition, body } => {
            loop {
                let cond = match eval(condition, env) {
                    Value::Bool(b) => b,
                    _ => nova_error(eval_line(), "while condition must be a boolean"),
                };
                if !cond { break; }
                match eval_block(body, env) {
                    Value::Break             => break,    // exit the loop entirely
                    Value::Continue          => continue, // skip to the next condition check
                    r @ Value::Return(_)     => return r, // propagate return out of the loop
                    _ => {}
                }
            }
            Value::Nil
        }

        // Binary operations (+, -, *, ==, ??, etc.)
        Expr::BinaryOp { left, op, right } => {
            // ?? is handled first and evaluated LAZILY — we only evaluate right if left is nil.
            // This is different from other operators which always evaluate both sides.
            if matches!(op, Token::QuestionQuestion) {
                return match eval(left, env) {
                    Value::Nil => eval(right, env), // left is nil — use the fallback
                    v => v,                         // left has a value — use it
                };
            }

            // All other operators evaluate both sides eagerly before deciding what to do
            let lval = eval(left, env);
            let rval = eval(right, env);

            // && and || operate on booleans — handle before the number extraction below
            if matches!(op, Token::And | Token::Or) {
                let l = match lval { Value::Bool(b) => b, _ => nova_error(eval_line(), "expected a boolean on the left side of && / ||") };
                let r = match rval { Value::Bool(b) => b, _ => nova_error(eval_line(), "expected a boolean on the right side of && / ||") };
                return Value::Bool(if matches!(op, Token::And) { l && r } else { l || r });
            }

            // Bool equality/inequality — handle before number extraction
            if matches!(op, Token::EqualsEquals | Token::BangEquals) {
                if let (Value::Bool(l), Value::Bool(r)) = (&lval, &rval) {
                    return Value::Bool(if matches!(op, Token::EqualsEquals) { l == r } else { l != r });
                }
            }

            // String equality/inequality — handle before number extraction
            if matches!(op, Token::EqualsEquals | Token::BangEquals) {
                if let (Value::Str(l), Value::Str(r)) = (&lval, &rval) {
                    return Value::Bool(if matches!(op, Token::EqualsEquals) { l == r } else { l != r });
                }
            }

            // String ordering — lexicographic comparison
            if matches!(op, Token::Less | Token::LessEquals | Token::Greater | Token::GreaterEquals) {
                if let (Value::Str(l), Value::Str(r)) = (&lval, &rval) {
                    return Value::Bool(match op {
                        Token::Less         => l < r,
                        Token::LessEquals   => l <= r,
                        Token::Greater      => l > r,
                        Token::GreaterEquals => l >= r,
                        _ => unreachable!(),
                    });
                }
            }

            // General equality/inequality — covers nil, arrays, hashmaps, and cross-type comparisons
            if matches!(op, Token::EqualsEquals | Token::BangEquals) {
                let eq = values_equal(&lval, &rval);
                return Value::Bool(if matches!(op, Token::EqualsEquals) { eq } else { !eq });
            }

            // Bitwise operators — integer only, before f64 extraction
            if matches!(op, Token::BitAnd | Token::BitOr | Token::BitXor | Token::Shl | Token::Shr) {
                let l = match &lval { Value::Int(n) => *n, _ => nova_error(eval_line(), "bitwise operators require integer operands") };
                let r = match &rval { Value::Int(n) => *n, _ => nova_error(eval_line(), "bitwise operators require integer operands") };
                return Value::Int(match op {
                    Token::BitAnd => l & r,
                    Token::BitOr  => l | r,
                    Token::BitXor => l ^ r,
                    Token::Shl    => l << r,
                    Token::Shr    => l >> r,
                    _ => unreachable!(),
                });
            }

            // String concatenation with + is handled before the number extraction below
            // because we'd otherwise panic trying to convert strings to numbers
            if let (Token::Plus, Value::Str(l), Value::Str(r)) = (op, &lval, &rval) {
                return Value::Str(l.clone() + r);
            }

            // array + array → concatenation
            if let (Token::Plus, Value::Array(l), Value::Array(r)) = (&op, &lval, &rval) {
                let mut combined = l.lock().unwrap().clone();
                combined.extend(r.lock().unwrap().clone());
                return make_array(combined);
            }

            // All remaining operators expect numbers — extract as f64 and track whether either side is float.
            // int op int → Int (truncating for /), anything with float → Float
            let ltype = runtime_type_of(&lval);
            let (l, l_float) = match lval {
                Value::Int(n)   => (n as f64, false),
                Value::Float(n) => (n, true),
                Value::Bool(b)  => (if b { 1.0 } else { 0.0 }, false),
                _ => nova_error(eval_line(), &format!("expected a number on the left side of binary operator, got {}", ltype)),
            };
            let rtype = runtime_type_of(&rval);
            let (r, r_float) = match rval {
                Value::Int(n)   => (n as f64, false),
                Value::Float(n) => (n, true),
                Value::Bool(b)  => (if b { 1.0 } else { 0.0 }, false),
                _ => nova_error(eval_line(), &format!("expected a number on the right side of binary operator, got {}", rtype)),
            };
            let is_float = l_float || r_float;
            let num = |n: f64| if is_float { Value::Float(n) } else { Value::Int(n as i64) };
            match op {
                Token::Plus          => num(l + r),
                Token::Minus         => num(l - r),
                Token::Star          => num(l * r),
                Token::Slash         => {
                    if r == 0.0 && !is_float {
                        nova_error(eval_line(), "division by zero");
                    }
                    Value::Float(l / r) // division always returns float
                }
                Token::Percent       => {
                    if r == 0.0 && !is_float {
                        nova_error(eval_line(), "modulo by zero");
                    }
                    num(l % r)
                }
                Token::EqualsEquals  => Value::Bool(l == r),
                Token::BangEquals    => Value::Bool(l != r),
                Token::Less          => Value::Bool(l < r),
                Token::LessEquals    => Value::Bool(l <= r),
                Token::Greater       => Value::Bool(l > r),
                Token::GreaterEquals => Value::Bool(l >= r),
                _ => nova_error(eval_line(), &format!("unknown operator {:?}", op)),
            }
        }

        // fn declaration — store the function as a value in the environment
        // The body is cloned from the AST so it can be called multiple times later
        // Type annotations live in the AST only; the runtime only needs the param names
        Expr::Fn { name, params, body, variadic, .. } => {
            let defaults = params.iter().map(|(_, _, d)| d.as_ref().map(|e| eval(e, env))).collect();
            env.insert(name.clone(), Value::Function {
                params: params.iter().map(|(n, _, _)| n.clone()).collect(),
                param_types: params.iter().map(|(_, t, _)| t.clone()).collect(),
                defaults,
                body: body.clone(),
                variadic: *variadic,
                captured_env: None,
            });
            Value::Nil
        }

        // Lambda — capture the current env so variables in the enclosing scope are accessible
        // when the lambda is called later (true closure behaviour)
        Expr::Lambda { params, body } => {
            Value::Function {
                defaults: vec![None; params.len()],
                params: params.clone(),
                param_types: vec![None; params.len()],
                body: body.clone(),
                variadic: false,
                captured_env: Some(Arc::new(Mutex::new(env.clone()))), // shared mutable scope — mutations inside the closure persist across calls
            }
        }

        // Function call — built-ins are checked first by name; if none match, look in env
        Expr::Call { name, args } => {

            // len — works on both arrays and strings
            if name == "len" {
                let val = eval(&args[0], env);
                return match val {
                    Value::Array(arr)   => Value::Int(arr.lock().unwrap().len() as i64),
                    Value::Str(s)       => Value::Int(s.chars().count() as i64),
                    Value::HashMap(map) => Value::Int(map.lock().unwrap().len() as i64),
                    _ => nova_error(eval_line(), "len() requires an array, string, or hashmap"),
                };
            }

            // str / int — type conversion
            if name == "str" {
                return Value::Str(format_value(&eval(&args[0], env))); // converts any value to its string representation
            }
            if name == "mod" {
                let a = eval(&args[0], env);
                let b = eval(&args[1], env);
                let (n, d) = match (&a, &b) {
                    (Value::Int(n),   Value::Int(d))   => (*n as f64, *d as f64),
                    (Value::Float(n), Value::Float(d)) => (*n, *d),
                    (Value::Int(n),   Value::Float(d)) => (*n as f64, *d),
                    (Value::Float(n), Value::Int(d))   => (*n, *d as f64),
                    _ => nova_error(eval_line(), "mod() requires two numbers"),
                };
                if d == 0.0 { nova_error(eval_line(), "mod() division by zero"); }
                let result = ((n % d) + d) % d;
                return match (&a, &b) {
                    (Value::Int(_), Value::Int(_)) => Value::Int(result as i64),
                    _ => Value::Float(result),
                };
            }
            if name == "int" {
                return match eval(&args[0], env) {
                    Value::Str(s)    => match s.trim().parse::<i64>() {
                        Ok(n)  => Value::Int(n),
                        Err(_) => nova_error(eval_line(), &format!("int() cannot convert \"{s}\" to int")),
                    },
                    Value::Int(n)    => Value::Int(n),
                    Value::Float(n)  => Value::Int(n as i64),
                    Value::Bool(b)   => Value::Int(if b { 1 } else { 0 }),
                    _ => nova_error(eval_line(), "int() requires a string, number, or bool"),
                };
            }

            if name == "float" {
                return match eval(&args[0], env) {
                    Value::Str(s)    => match s.trim().parse::<f64>() {
                        Ok(n)  => Value::Float(n),
                        Err(_) => nova_error(eval_line(), &format!("float() cannot convert \"{s}\" to float")),
                    },
                    Value::Int(n)    => Value::Float(n as f64),
                    Value::Float(n)  => Value::Float(n),
                    Value::Bool(b)   => Value::Float(if b { 1.0 } else { 0.0 }),
                    _ => nova_error(eval_line(), "float() requires a string, number, or bool"),
                };
            }

            if name == "bool" {
                return match eval(&args[0], env) {
                    Value::Bool(b) => Value::Bool(b),
                    Value::Int(n)  => Value::Bool(n != 0),
                    Value::Nil     => Value::Bool(false),
                    _              => Value::Bool(true),
                };
            }

            // string built-ins — all return new strings, none mutate in place
            if name == "ord" {
                return match eval(&args[0], env) {
                    Value::Str(s) => {
                        let mut chars = s.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) => Value::Int(c as i64),
                            _ => nova_error(eval_line(), "ord() requires a single character string"),
                        }
                    }
                    _ => nova_error(eval_line(), "ord() requires a string"),
                };
            }
            if name == "chr" {
                return match eval(&args[0], env) {
                    Value::Int(n) => match char::from_u32(n as u32) {
                        Some(c) => Value::Str(c.to_string()),
                        None    => nova_error(eval_line(), &format!("chr() invalid code point {}", n)),
                    },
                    _ => nova_error(eval_line(), "chr() requires an int"),
                };
            }
            if name == "upper" {
                return match eval(&args[0], env) {
                    Value::Str(s) => Value::Str(s.to_uppercase()),
                    _ => nova_error(eval_line(), "upper() requires a string"),
                };
            }
            if name == "lower" {
                return match eval(&args[0], env) {
                    Value::Str(s) => Value::Str(s.to_lowercase()),
                    _ => nova_error(eval_line(), "lower() requires a string"),
                };
            }
            if name == "trim" {
                return match eval(&args[0], env) {
                    Value::Str(s) => Value::Str(s.trim().to_string()), // removes leading and trailing whitespace
                    _ => nova_error(eval_line(), "trim() requires a string"),
                };
            }
            if name == "split" {
                let s = match eval(&args[0], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "split() first argument must be a string"),
                };
                let delim = match eval(&args[1], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "split() second argument must be a string"),
                };
                // empty delimiter: split into individual characters (avoids Rust's boundary empty strings)
                if delim.is_empty() {
                    return make_array(s.chars().map(|c| Value::Str(c.to_string())).collect());
                }
                return make_array(s.split(delim.as_str()).map(|p| Value::Str(p.to_string())).collect());
            }
            if name == "join" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "join() first argument must be an array"),
                };
                let delim = match eval(&args[1], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "join() second argument must be a string"),
                };
                // convert each element to a string, then join with the delimiter
                return Value::Str(arr.lock().unwrap().iter().map(|v| format_value(v)).collect::<Vec<_>>().join(&delim));
            }
            if name == "contains" {
                let first  = eval(&args[0], env);
                let second = eval(&args[1], env);
                return match (first, second) {
                    (Value::Str(s), Value::Str(sub)) => Value::Bool(s.contains(sub.as_str())),
                    (Value::Array(arr), val)          => Value::Bool(arr.lock().unwrap().iter().any(|v| values_equal(v, &val))),
                    _ => nova_error(eval_line(), "contains() requires (string, string) or (array, value)"),
                };
            }
            if name == "substr" {
                let s      = match eval(&args[0], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "substr() first argument must be a string") };
                let start  = match eval(&args[1], env) { Value::Int(n) => n as usize, Value::Float(n) => n as usize, _ => nova_error(eval_line(), "substr() start must be a number") };
                let length = match eval(&args[2], env) { Value::Int(n) => n as usize, Value::Float(n) => n as usize, _ => nova_error(eval_line(), "substr() length must be a number") };
                let chars: Vec<char> = s.chars().collect();
                if start >= chars.len() { return Value::Str(String::new()); }
                let end = (start + length).min(chars.len());
                return Value::Str(chars[start..end].iter().collect());
            }
            if name == "startsWith" {
                let s      = match eval(&args[0], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "startsWith() first argument must be a string") };
                let prefix = match eval(&args[1], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "startsWith() second argument must be a string") };
                return Value::Bool(s.starts_with(prefix.as_str()));
            }
            if name == "endsWith" {
                let s      = match eval(&args[0], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "endsWith() first argument must be a string") };
                let suffix = match eval(&args[1], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "endsWith() second argument must be a string") };
                return Value::Bool(s.ends_with(suffix.as_str()));
            }
            if name == "replace" {
                let s    = match eval(&args[0], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "replace() requires strings") };
                let from = match eval(&args[1], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "replace() requires strings") };
                let to   = match eval(&args[2], env) { Value::Str(s) => s, _ => nova_error(eval_line(), "replace() requires strings") };
                return Value::Str(s.replace(from.as_str(), to.as_str())); // replaces ALL occurrences
            }

            // array mutation built-ins — all return a new array, none modify in place
            if name == "push" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "push() first argument must be an array"),
                };
                let val = eval(&args[1], env);
                let mut new_arr = arr.lock().unwrap().clone(); // clone the inner vec — we're making a new array
                new_arr.push(val);
                return make_array(new_arr);
            }
            if name == "pop" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "pop() requires an array"),
                };
                let mut new_arr = arr.lock().unwrap().clone();
                new_arr.pop(); // remove from the end, discarding the value
                return make_array(new_arr);
            }
            if name == "reverse" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "reverse() requires an array"),
                };
                let mut new_arr = arr.lock().unwrap().clone();
                new_arr.reverse();
                return make_array(new_arr);
            }
            if name == "slice" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "slice() first argument must be an array"),
                };
                let start = match eval(&args[1], env) {
                    Value::Int(n)   => n as usize,
                    Value::Float(n) => n as usize,
                    _ => nova_error(eval_line(), "slice() start must be a number"),
                };
                let end = match eval(&args[2], env) {
                    Value::Int(n)   => n as usize,
                    Value::Float(n) => n as usize,
                    _ => nova_error(eval_line(), "slice() end must be a number"),
                };
                let inner = arr.lock().unwrap();
                return make_array(inner[start..end.min(inner.len())].to_vec()); // end clamped to array length
            }
            if name == "sort" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "sort() requires an array"),
                };
                let mut new_arr = arr.lock().unwrap().clone();
                new_arr.sort_by(|a, b| match (a, b) {
                    (Value::Int(x),   Value::Int(y))   => x.cmp(y),
                    (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
                    (Value::Int(x),   Value::Float(y)) => (*x as f64).total_cmp(y),
                    (Value::Float(x), Value::Int(y))   => x.total_cmp(&(*y as f64)),
                    (Value::Str(x),   Value::Str(y))   => x.cmp(y),
                    _ => nova_error(eval_line(), "sort() only supports arrays of numbers or strings"),
                });
                return make_array(new_arr);
            }
            if name == "concat" {
                let a = match eval(&args[0], env) { Value::Array(a) => a, _ => nova_error(eval_line(), "concat() first argument must be an array") };
                let b = match eval(&args[1], env) { Value::Array(b) => b, _ => nova_error(eval_line(), "concat() second argument must be an array") };
                // chain joins two iterators end-to-end into a new vec
                return make_array(a.lock().unwrap().iter().cloned().chain(b.lock().unwrap().iter().cloned()).collect());
            }
            if name == "zip" {
                let a = match eval(&args[0], env) { Value::Array(a) => a, _ => nova_error(eval_line(), "zip() first argument must be an array") };
                let b = match eval(&args[1], env) { Value::Array(b) => b, _ => nova_error(eval_line(), "zip() second argument must be an array") };
                // pairs up elements by position — stops at the shorter array, never pads with nil
                let zipped = a.lock().unwrap().iter().cloned()
                    .zip(b.lock().unwrap().iter().cloned())
                    .map(|(x, y)| make_array(vec![x, y])) // each pair is itself an array
                    .collect();
                return make_array(zipped);
            }

            // higher-order array functions — each takes a function as the last argument
            if name == "map" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "map() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                let result = arr.lock().unwrap().iter().cloned()
                    .map(|v| call_function(func.clone(), vec![v], env)) // call func once per element
                    .collect();
                return make_array(result);
            }
            if name == "filter" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "filter() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                let result = arr.lock().unwrap().iter().cloned()
                    .filter(|v| match call_function(func.clone(), vec![v.clone()], env) {
                        Value::Bool(b) => b, // keep element if function returns true
                        _ => nova_error(eval_line(), "filter() function must return a boolean"),
                    })
                    .collect();
                return make_array(result);
            }
            if name == "reduce" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "reduce() first argument must be an array"),
                };
                let initial = eval(&args[1], env); // starting value for the accumulator
                let func = eval(&args[2], env);
                // fold walks the array left to right, threading the accumulator through each call
                return arr.lock().unwrap().iter().cloned()
                    .fold(initial, |acc, v| call_function(func.clone(), vec![acc, v], env));
            }
            if name == "sum" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "sum() requires an array"),
                };
                let mut total = 0.0f64;
                let mut has_float = false;
                for v in arr.lock().unwrap().iter() {
                    match v {
                        Value::Int(n)   => total += *n as f64,
                        Value::Float(n) => { has_float = true; total += n; }
                        _ => nova_error(eval_line(), "sum() requires an array of numbers"),
                    }
                }
                return if has_float { Value::Float(total) } else { Value::Int(total as i64) };
            }
            if name == "product" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "product() requires an array"),
                };
                let mut total = 1.0f64;
                let mut has_float = false;
                for v in arr.lock().unwrap().iter() {
                    match v {
                        Value::Int(n)   => total *= *n as f64,
                        Value::Float(n) => { has_float = true; total *= n; }
                        _ => nova_error(eval_line(), "product() requires an array of numbers"),
                    }
                }
                return if has_float { Value::Float(total) } else { Value::Int(total as i64) };
            }
            if name == "any" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "any() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                // short-circuits — stops as soon as one element returns true
                return Value::Bool(arr.lock().unwrap().iter().cloned().any(|v| match call_function(func.clone(), vec![v], env) {
                    Value::Bool(b) => b,
                    _ => nova_error(eval_line(), "any() function must return a boolean"),
                }));
            }
            if name == "all" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "all() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                // short-circuits — stops as soon as one element returns false
                return Value::Bool(arr.lock().unwrap().iter().cloned().all(|v| match call_function(func.clone(), vec![v], env) {
                    Value::Bool(b) => b,
                    _ => nova_error(eval_line(), "all() function must return a boolean"),
                }));
            }
            if name == "count" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "count() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                let n = arr.lock().unwrap().iter().cloned().filter(|v| match call_function(func.clone(), vec![v.clone()], env) {
                    Value::Bool(b) => b,
                    _ => nova_error(eval_line(), "count() function must return a boolean"),
                }).count();
                return Value::Int(n as i64);
            }
            if name == "find" {
                let arr = match eval(&args[0], env) {
                    Value::Array(a) => a,
                    _ => nova_error(eval_line(), "find() first argument must be an array"),
                };
                let func = eval(&args[1], env);
                let inner = arr.lock().unwrap().clone();
                for v in inner {
                    if let Value::Bool(true) = call_function(func.clone(), vec![v.clone()], env) {
                        return v; // return the first matching element
                    }
                }
                return Value::Nil; // no match found
            }

            // math built-ins
            if name == "sqrt" {
                return match eval(&args[0], env) {
                    Value::Int(n)   => Value::Float((n as f64).sqrt()),
                    Value::Float(n) => Value::Float(n.sqrt()),
                    _ => nova_error(eval_line(), "sqrt() requires a number"),
                };
            }
            if name == "abs" {
                return match eval(&args[0], env) {
                    Value::Int(n)   => Value::Int(n.abs()),    // abs of int → int
                    Value::Float(n) => Value::Float(n.abs()),  // abs of float → float
                    _ => nova_error(eval_line(), "abs() requires a number"),
                };
            }
            if name == "floor" {
                return match eval(&args[0], env) {
                    Value::Int(n)   => Value::Int(n),
                    Value::Float(n) => Value::Int(n.floor() as i64),
                    _ => nova_error(eval_line(), "floor() requires a number"),
                };
            }
            if name == "ceil" {
                return match eval(&args[0], env) {
                    Value::Int(n)   => Value::Int(n),
                    Value::Float(n) => Value::Int(n.ceil() as i64),
                    _ => nova_error(eval_line(), "ceil() requires a number"),
                };
            }
            if name == "round" {
                let n = match eval(&args[0], env) {
                    Value::Int(n)   => n as f64,
                    Value::Float(n) => n,
                    _ => nova_error(eval_line(), "round() requires a number"),
                };
                return if args.len() == 1 {
                    Value::Int(n.round() as i64)  // round to whole number → Int
                } else {
                    let decimals = match eval(&args[1], env) {
                        Value::Int(d)   => d as i32,
                        Value::Float(d) => d as i32,
                        _ => nova_error(eval_line(), "round() second argument must be a number"),
                    };
                    let factor = 10f64.powi(decimals);
                    Value::Float((n * factor).round() / factor)  // round to N decimals → Float
                };
            }
            if name == "max" {
                if args.len() == 1 {
                    let arr = match eval(&args[0], env) { Value::Array(a) => a, _ => nova_error(eval_line(), "max() with one argument requires an array") };
                    let inner = arr.lock().unwrap();
                    if inner.is_empty() { return Value::Nil; }
                    let mut has_float = false;
                    let mut best = f64::NEG_INFINITY;
                    for v in inner.iter() {
                        let vf = match v {
                            Value::Int(n)   => *n as f64,
                            Value::Float(n) => { has_float = true; *n }
                            _ => nova_error(eval_line(), "max() requires an array of numbers"),
                        };
                        if vf > best { best = vf; }
                    }
                    return if has_float { Value::Float(best) } else { Value::Int(best as i64) };
                }
                let (a, af) = match eval(&args[0], env) { Value::Int(n) => (n as f64, false), Value::Float(n) => (n, true), _ => nova_error(eval_line(), "max() requires numbers") };
                let (b, bf) = match eval(&args[1], env) { Value::Int(n) => (n as f64, false), Value::Float(n) => (n, true), _ => nova_error(eval_line(), "max() requires numbers") };
                let result = a.max(b);
                return if af || bf { Value::Float(result) } else { Value::Int(result as i64) };
            }
            if name == "min" {
                if args.len() == 1 {
                    let arr = match eval(&args[0], env) { Value::Array(a) => a, _ => nova_error(eval_line(), "min() with one argument requires an array") };
                    let inner = arr.lock().unwrap();
                    if inner.is_empty() { return Value::Nil; }
                    let mut has_float = false;
                    let mut best = f64::INFINITY;
                    for v in inner.iter() {
                        let vf = match v {
                            Value::Int(n)   => *n as f64,
                            Value::Float(n) => { has_float = true; *n }
                            _ => nova_error(eval_line(), "min() requires an array of numbers"),
                        };
                        if vf < best { best = vf; }
                    }
                    return if has_float { Value::Float(best) } else { Value::Int(best as i64) };
                }
                let (a, af) = match eval(&args[0], env) { Value::Int(n) => (n as f64, false), Value::Float(n) => (n, true), _ => nova_error(eval_line(), "min() requires numbers") };
                let (b, bf) = match eval(&args[1], env) { Value::Int(n) => (n as f64, false), Value::Float(n) => (n, true), _ => nova_error(eval_line(), "min() requires numbers") };
                let result = a.min(b);
                return if af || bf { Value::Float(result) } else { Value::Int(result as i64) };
            }
            if name == "pow" {
                let base = match eval(&args[0], env) { Value::Int(n) => n as f64, Value::Float(n) => n, _ => nova_error(eval_line(), "pow() requires numbers") };
                let exp  = match eval(&args[1], env) { Value::Int(n) => n as f64, Value::Float(n) => n, _ => nova_error(eval_line(), "pow() requires numbers") };
                return Value::Float(base.powf(exp));
            }
            if name == "random" {
                return Value::Float(rand::random::<f64>()); // uniform float in [0, 1)
            }

            // type inspection built-ins
            if name == "type" {
                return Value::Str(match eval(&args[0], env) {
                    Value::Int(_)        => "int".to_string(),
                    Value::Float(_)      => "float".to_string(),
                    Value::Str(_)        => "string".to_string(),
                    Value::Bool(_)       => "bool".to_string(),
                    Value::Array(_)      => "array".to_string(),
                    Value::HashMap(_)    => "hashmap".to_string(),
                    Value::Range(..)     => "range".to_string(),
                    Value::Function {..} => "function".to_string(),
                    Value::VmFunc(_)     => "function".to_string(),
                    Value::Nil                => "nil".to_string(),
                    Value::Break              => "break".to_string(),
                    Value::Continue           => "continue".to_string(),
                    Value::Return(_)          => "return".to_string(),
                    Value::StructDef { .. }   => "structdef".to_string(),
                    Value::Struct { name, .. } => format!("struct:{}", name),
                    Value::EnumDef { .. }      => "enumdef".to_string(),
                    Value::EnumVariant { enum_name, variant, .. } => format!("enum:{}:{}", enum_name, variant),
                    Value::EnumConstructor { .. } => "enumconstructor".to_string(),
                    Value::Channel(_)             => "channel".to_string(),
                    Value::Mutex(_)               => "mutex".to_string(),
                    Value::Task(_)                => "task".to_string(),
                });
            }
            if name == "isInt"    { return Value::Bool(matches!(eval(&args[0], env), Value::Int(_))); }
            if name == "isFloat"  { return Value::Bool(matches!(eval(&args[0], env), Value::Float(_))); }
            if name == "isString" { return Value::Bool(matches!(eval(&args[0], env), Value::Str(_))); }
            if name == "isBool"   { return Value::Bool(matches!(eval(&args[0], env), Value::Bool(_))); }
            if name == "isArray"  { return Value::Bool(matches!(eval(&args[0], env), Value::Array(_))); }
            if name == "isNil"     { return Value::Bool(matches!(eval(&args[0], env), Value::Nil)); }
            if name == "isHashmap" { return Value::Bool(matches!(eval(&args[0], env), Value::HashMap(_))); }

            // hashmap built-ins
            if name == "keys" {
                return match eval(&args[0], env) {
                    Value::HashMap(map) => make_array(map.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()),
                    _ => nova_error(eval_line(), "keys() requires a hashmap"),
                };
            }
            if name == "values" {
                return match eval(&args[0], env) {
                    Value::HashMap(map) => make_array(map.lock().unwrap().iter().map(|(_, v)| v.clone()).collect()),
                    _ => nova_error(eval_line(), "values() requires a hashmap"),
                };
            }
            if name == "hasKey" {
                let key = eval(&args[1], env);
                return match eval(&args[0], env) {
                    Value::HashMap(map) => Value::Bool(map.lock().unwrap().iter().any(|(k, _)| values_equal(k, &key))),
                    _ => nova_error(eval_line(), "hasKey() requires a hashmap"),
                };
            }
            if name == "delete" {
                let key = eval(&args[1], env);
                return match eval(&args[0], env) {
                    Value::HashMap(map) => make_map(
                        map.lock().unwrap().iter()
                            .filter(|(k, _)| !values_equal(k, &key))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect() // keep all entries except the one with this key
                    ),
                    _ => nova_error(eval_line(), "delete() requires a hashmap"),
                };
            }
            if name == "mergeMap" {
                let map1 = match eval(&args[0], env) { Value::HashMap(m) => m, _ => nova_error(eval_line(), "mergeMap() requires hashmaps") };
                let map2 = match eval(&args[1], env) { Value::HashMap(m) => m, _ => nova_error(eval_line(), "mergeMap() requires hashmaps") };
                let mut result: Vec<(Value, Value)> = map1.lock().unwrap().clone();
                for (k, v) in map2.lock().unwrap().iter() {
                    result.retain(|(ek, _)| !values_equal(ek, k)); // remove existing entry so map2 wins on conflicts
                    result.push((k.clone(), v.clone()));
                }
                return make_map(result);
            }
            if name == "setKey" {
                let key = eval(&args[1], env);
                let val = eval(&args[2], env);
                return match eval(&args[0], env) {
                    Value::HashMap(map) => {
                        let mut new_map: Vec<(Value, Value)> = map.lock().unwrap().clone();
                        new_map.retain(|(k, _)| !values_equal(k, &key)); // remove any existing entry for that key
                        new_map.push((key, val));                          // insert the new one at the end
                        make_map(new_map)
                    }
                    _ => nova_error(eval_line(), "setKey() first argument must be a hashmap"),
                };
            }

            // IO built-ins
            if name == "input" {
                if !args.is_empty() {
                    let prompt = match eval(&args[0], env) {
                        Value::Str(s) => s,
                        v => format_value(&v), // non-string prompt is printed as-is
                    };
                    print!("{}", prompt);
                    use std::io::Write;
                    std::io::stdout().flush().unwrap(); // flush so prompt appears before waiting for input
                }
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).unwrap();
                return Value::Str(line.trim_end_matches('\n').trim_end_matches('\r').to_string()); // strip newline from end
            }
            if name == "println" {
                let val = eval(&args[0], env);
                println!("{}", format_value(&val)); // function form of print — same behaviour
                return Value::Nil;
            }
            if name == "readFile" {
                let path = match eval(&args[0], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "readFile() requires a string path"),
                };
                return match std::fs::read_to_string(&path) {
                    Ok(content) => Value::Str(content),
                    Err(e) => nova_error(eval_line(), &format!("readFile() failed: {}", e)),
                };
            }
            if name == "writeFile" {
                let path = match eval(&args[0], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "writeFile() first argument must be a string path"),
                };
                let content = match eval(&args[1], env) {
                    Value::Str(s) => s,
                    _ => nova_error(eval_line(), "writeFile() second argument must be a string"),
                };
                std::fs::write(&path, content).unwrap_or_else(|e| nova_error(eval_line(), &format!("writeFile() failed: {}", e)));
                return Value::Nil;
            }

            // ── channel primitives ────────────────────────────────────────────
            if name == "channel" {
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                return Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }));
            }
            if name == "send" {
                let ch = eval(&args[0], env);
                let val = eval(&args[1], env);
                match ch {
                    Value::Channel(arc) => {
                        let sent = deep_clone(&val);
                        arc.pending.lock().unwrap().push(sent.clone());
                        arc.sender.send(sent).unwrap_or_else(|_| nova_error(eval_line(), "send() failed: channel is closed"));
                    }
                    _ => nova_error(eval_line(), "send() first argument must be a channel"),
                }
                return Value::Nil;
            }
            if name == "recv" {
                let ch = eval(&args[0], env);
                match ch {
                    Value::Channel(arc) => {
                        return match arc.receiver.recv() {
                            Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                            Err(_) => Value::Nil,
                        };
                    }
                    _ => nova_error(eval_line(), "recv() argument must be a channel"),
                }
            }
            if name == "tryRecv" {
                let ch = eval(&args[0], env);
                match ch {
                    Value::Channel(arc) => {
                        return match arc.receiver.try_recv() {
                            Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                            Err(TryRecvError::Empty) => Value::Nil,
                            Err(TryRecvError::Disconnected) => Value::Nil,
                        };
                    }
                    _ => nova_error(eval_line(), "tryRecv() argument must be a channel"),
                }
            }
            if name == "close" {
                let ch = eval(&args[0], env);
                match ch {
                    Value::Channel(_) => {}
                    _ => nova_error(eval_line(), "close() argument must be a channel"),
                }
                return Value::Nil;
            }
            if name == "ticker" {
                let ms = match eval(&args[0], env) {
                    Value::Int(n) => n as u64,
                    Value::Float(f) => f as u64,
                    _ => nova_error(eval_line(), "ticker(): expected integer milliseconds"),
                };
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                let tick_sender = s.clone();
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        if tick_sender.send(Value::Bool(true)).is_err() { break; }
                    }
                });
                return Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }));
            }
            if name == "timeout" {
                let ms = match eval(&args[0], env) {
                    Value::Int(n) => n as u64,
                    Value::Float(f) => f as u64,
                    _ => nova_error(eval_line(), "timeout(): expected integer milliseconds"),
                };
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                let timeout_sender = s.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    let _ = timeout_sender.send(Value::Bool(true));
                });
                return Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }));
            }
            if name == "mutex" {
                if args.is_empty() { nova_error(eval_line(), "mutex(): expected initial value"); }
                let initial = eval(&args[0], env);
                let sent = deep_clone(&initial);
                let (s, r) = crossbeam_channel::bounded::<Value>(1);
                let inner = Arc::new(ChannelInner {
                    sender: s, receiver: r,
                    pending: Mutex::new(vec![sent.clone()]), // track for GC
                });
                inner.sender.send(sent).unwrap();
                return Value::Mutex(inner);
            }
            if name == "lock" {
                let m = eval(&args[0], env);
                match m {
                    Value::Mutex(arc) => {
                        return match arc.receiver.recv() {
                            Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                            Err(_) => nova_error(eval_line(), "lock(): mutex is poisoned"),
                        };
                    }
                    _ => nova_error(eval_line(), "lock() argument must be a mutex"),
                }
            }
            if name == "unlock" {
                if args.len() < 2 { nova_error(eval_line(), "unlock(): expected (mutex, value)"); }
                let m = eval(&args[0], env);
                let new_val = eval(&args[1], env);
                match m {
                    Value::Mutex(arc) => {
                        let sent = deep_clone(&new_val);
                        arc.pending.lock().unwrap().push(sent.clone());
                        arc.sender.send(sent).unwrap_or_else(|_| nova_error(eval_line(), "unlock(): mutex is full — was it already unlocked?"));
                        return Value::Nil;
                    }
                    _ => nova_error(eval_line(), "unlock() first argument must be a mutex"),
                }
            }
            if name == "withLock" {
                if args.len() < 2 { nova_error(eval_line(), "withLock(): expected (mutex, function)"); }
                let m = eval(&args[0], env);
                let f = eval(&args[1], env);
                match m {
                    Value::Mutex(arc) => {
                        let current = match arc.receiver.recv() {
                            Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                            Err(_) => nova_error(eval_line(), "withLock(): mutex is poisoned"),
                        };
                        let result = call_function(f, vec![current], env);
                        let sent = deep_clone(&result);
                        arc.pending.lock().unwrap().push(sent.clone());
                        arc.sender.send(sent).unwrap_or_else(|_| nova_error(eval_line(), "withLock(): mutex is full after callback"));
                        return result;
                    }
                    _ => nova_error(eval_line(), "withLock() first argument must be a mutex"),
                }
            }
            if name == "wait" {
                let task = eval(&args[0], env);
                match task {
                    Value::Task(arc) => {
                        return match arc.recv() {
                            Ok(Ok(v))    => v,
                            Ok(Err(msg)) => nova_error(eval_line(), &format!("spawned task threw: {}", msg)),
                            Err(_)       => nova_error(eval_line(), "wait(): task channel disconnected"),
                        };
                    }
                    _ => nova_error(eval_line(), "wait() argument must be a task"),
                }
            }

            if name == "spawnAll" {
                if args.len() != 2 {
                    nova_error(eval_line(), "spawnAll(): expected 2 arguments (array, function)");
                }
                let arr  = eval(&args[0], env);
                let func = eval(&args[1], env);
                let elements: Vec<Value> = match arr {
                    Value::Array(arc) => arc.lock().unwrap().clone(),
                    _ => nova_error(eval_line(), "spawnAll(): first argument must be an array"),
                };
                // Reuse existing spawn path: bind __sa_fn__ and __sa_elem__ in env,
                // then eval spawn(DynCall(__sa_fn__, __sa_elem__)) for each element.
                // spawn deep_clone_unregistered's the entire env snapshot, so each
                // thread gets a fully isolated copy.
                let spawn_expr = Expr::Spawn(Box::new(Expr::DynCall {
                    callee: Box::new(Expr::Ident("__sa_fn__".to_string())),
                    args:   vec![Expr::Ident("__sa_elem__".to_string())],
                }));
                let mut tasks: Vec<Value> = Vec::new();
                for elem in &elements {
                    env.insert("__sa_fn__".to_string(),   deep_clone_unregistered(&func));
                    env.insert("__sa_elem__".to_string(), deep_clone_unregistered(elem));
                    tasks.push(eval(&spawn_expr, env));
                }
                env.remove("__sa_fn__");
                env.remove("__sa_elem__");
                let mut results: Vec<Value> = Vec::new();
                for task in tasks {
                    if let Value::Task(arc) = task {
                        match arc.recv() {
                            Ok(Ok(v))    => results.push(v),
                            Ok(Err(msg)) => nova_error(eval_line(), &format!("spawnAll task threw: {}", msg)),
                            Err(_)       => nova_error(eval_line(), "spawnAll: task channel disconnected"),
                        }
                    }
                }
                return make_array(results);
            }

            // not a built-in — look up the function in the environment
            let func = env.get(name).cloned()
                .unwrap_or_else(|| {
                    let candidates: Vec<&str> = env.keys().map(|s| s.as_str()).collect();
                    let suggestion = did_you_mean(name, &candidates)
                        .map(|s| format!("\n  Did you mean '{}'?", s))
                        .unwrap_or_default();
                    nova_error(eval_line(), &format!("undefined function '{}'{}", name, suggestion))
                });

            match func {
                Value::Function { params, param_types, defaults, body, variadic, captured_env } => {
                    let depth = CALL_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v + 1 });
                    if depth > 2000 {
                        CALL_DEPTH.with(|d| d.set(0));
                        nova_error(eval_line(), "stack overflow: recursion depth exceeded 2000");
                    }
                    let evaluated_args: Vec<Value> = args.iter().map(|a| eval(a, env)).collect();
                    // runtime type check: reject args that violate explicit param type annotations
                    for (i, param) in params.iter().enumerate() {
                        if let Some(Some(expected)) = param_types.get(i) {
                            if let Some(val) = evaluated_args.get(i) {
                                if !runtime_type_matches(expected, val) {
                                    nova_error(eval_line(), &format!(
                                        "type error: argument '{}' in call to '{}' expected {}, got {}",
                                        param, name, expected, runtime_type_of(val)
                                    ));
                                }
                            }
                        }
                    }
                    let captured_arc = captured_env.clone(); // clone the Arc (cheap — shared reference)
                    let is_closure = captured_arc.is_some();
                    let outer_keys: Vec<String> = if !is_closure { env.keys().cloned().collect() } else { vec![] };
                    let mut fn_env = match captured_arc.as_ref() {
                        Some(arc) => arc.lock().unwrap().clone(),
                        None => env.clone(),
                    };
                    if variadic {
                        let regular = params.len() - 1;
                        for (param, val) in params[..regular].iter().zip(evaluated_args.iter()) {
                            fn_env.insert(param.clone(), val.clone());
                        }
                        fn_env.insert(params[regular].clone(), make_array(evaluated_args[regular..].to_vec()));
                    } else {
                        for (i, param) in params.iter().enumerate() {
                            let val = evaluated_args.get(i).cloned()
                                .or_else(|| defaults.get(i).and_then(|d| d.clone()))
                                .unwrap_or_else(|| nova_error(eval_line(), &format!("missing argument '{}' in call to '{}'", param, name)));
                            fn_env.insert(param.clone(), val);
                        }
                    }
                    if !is_closure { push_local_frame(); }
                    DEFERRED.with(|d| d.borrow_mut().push(Vec::new()));
                    let result = match eval_block(&body, &mut fn_env) {
                        Value::Return(v) => *v,
                        other => other,
                    };
                    // run deferred items LIFO (last-registered runs first)
                    let deferred_items = DEFERRED.with(|d| d.borrow_mut().pop().unwrap_or_default());
                    for (expr, mut captured) in deferred_items.into_iter().rev() {
                        eval(&expr, &mut captured);
                    }
                    let local_set = if !is_closure { pop_local_frame() } else { HashSet::new() };
                    // write closure mutations back to the shared captured env
                    if let Some(arc) = &captured_arc {
                        let mut guard = arc.lock().unwrap();
                        for k in guard.keys().cloned().collect::<Vec<_>>() {
                            if let Some(v) = fn_env.get(&k) { guard.insert(k, v.clone()); }
                        }
                    }
                    CALL_DEPTH.with(|d| d.set(d.get() - 1));
                    // write back any globals that were mutated inside the function body
                    // (excluded: closures use a captured snapshot; params shadow globals; let-locals stay local)
                    if !is_closure {
                        let param_set: std::collections::HashSet<&str> =
                            params.iter().map(|p| p.as_str()).collect();
                        for k in &outer_keys {
                            if !param_set.contains(k.as_str()) && !local_set.contains(k) {
                                if let Some(v) = fn_env.get(k) {
                                    env.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                    result
                }
                Value::EnumConstructor { enum_name, variant, arity } => {
                    let evaluated_args: Vec<Value> = args.iter().map(|a| eval(a, env)).collect();
                    if evaluated_args.len() != arity {
                        nova_error(eval_line(), &format!(
                            "enum constructor '{}.{}' expects {} args, got {}",
                            enum_name, variant, arity, evaluated_args.len()
                        ))
                    }
                    Value::EnumVariant { enum_name, variant, payload: evaluated_args }
                }
                _ => nova_error(eval_line(), &format!("'{}' is not a function", name)),
            }
        }

        // expr(args) — calling the result of any expression: f(1)(2), arr[0](x), ((x)->x)(7)
        Expr::DynCall { callee, args } => {
            let func = eval(callee, env);
            let evaluated_args: Vec<Value> = args.iter().map(|a| eval(a, env)).collect();
            match func {
                Value::Function { params, param_types: _, defaults, body, variadic, captured_env } => {
                    let depth = CALL_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v + 1 });
                    if depth > 2000 {
                        CALL_DEPTH.with(|d| d.set(0));
                        nova_error(eval_line(), "stack overflow: recursion depth exceeded 2000");
                    }
                    let captured_arc = captured_env.clone();
                    let is_closure = captured_arc.is_some();
                    let outer_keys: Vec<String> = if !is_closure { env.keys().cloned().collect() } else { vec![] };
                    let mut fn_env = match captured_arc.as_ref() {
                        Some(arc) => arc.lock().unwrap().clone(),
                        None => env.clone(),
                    };
                    if variadic {
                        let regular = params.len() - 1;
                        for (param, val) in params[..regular].iter().zip(evaluated_args.iter()) {
                            fn_env.insert(param.clone(), val.clone());
                        }
                        fn_env.insert(params[regular].clone(), make_array(evaluated_args[regular..].to_vec()));
                    } else {
                        for (i, param) in params.iter().enumerate() {
                            let val = evaluated_args.get(i).cloned()
                                .or_else(|| defaults.get(i).and_then(|d| d.clone()))
                                .unwrap_or(Value::Nil);
                            fn_env.insert(param.clone(), val);
                        }
                    }
                    if !is_closure { push_local_frame(); }
                    let result = match eval_block(&body, &mut fn_env) {
                        Value::Return(v) => *v,
                        other => other,
                    };
                    let local_set = if !is_closure { pop_local_frame() } else { HashSet::new() };
                    if let Some(arc) = &captured_arc {
                        let mut guard = arc.lock().unwrap();
                        for k in guard.keys().cloned().collect::<Vec<_>>() {
                            if let Some(v) = fn_env.get(&k) { guard.insert(k, v.clone()); }
                        }
                    }
                    CALL_DEPTH.with(|d| d.set(d.get() - 1));
                    if !is_closure {
                        let param_set: std::collections::HashSet<&str> =
                            params.iter().map(|p| p.as_str()).collect();
                        for k in &outer_keys {
                            if !param_set.contains(k.as_str()) && !local_set.contains(k) {
                                if let Some(v) = fn_env.get(k) {
                                    env.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                    result
                }
                Value::EnumConstructor { enum_name, variant, arity } => {
                    if evaluated_args.len() != arity {
                        nova_error(eval_line(), &format!(
                            "enum constructor '{}.{}' expects {} args, got {}",
                            enum_name, variant, arity, evaluated_args.len()
                        ))
                    }
                    Value::EnumVariant { enum_name, variant, payload: evaluated_args }
                }
                _ => nova_error(eval_line(), "attempt to call a non-function value"),
            }
        }

        Expr::Array(elements) => {
            let vals = elements.iter().map(|e| eval(e, env)).collect();
            make_array(vals) // wrap in Rc<RefCell> so copies are cheap
        }

        Expr::HashMap(pairs) => {
            let vals = pairs.iter().map(|(k, v)| (eval(k, env), eval(v, env))).collect();
            make_map(vals) // wrap in Rc<RefCell> so copies are cheap
        }

        Expr::Index { object, index } => {
            let obj = eval(object, env);
            let idx = eval(index, env);
            match (obj, idx) {
                (Value::Array(arr), Value::Int(n)) => {
                    let len = arr.lock().unwrap().len() as i64;
                    let i = if n < 0 { len + n } else { n };
                    if i < 0 || i >= len {
                        nova_error(eval_line(), &format!("array index {} out of bounds", n))
                    } else {
                        arr.lock().unwrap()[i as usize].clone()
                    }
                }
                (Value::Array(arr), Value::Float(n)) => {
                    let i = n as usize;
                    arr.lock().unwrap()
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| nova_error(eval_line(), &format!("array index {} out of bounds", n)))
                }
                (Value::Str(s), Value::Int(n)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as i64;
                    let i = if n < 0 { len + n } else { n };
                    if i < 0 || i >= len {
                        nova_error(eval_line(), &format!("string index {} out of bounds", n))
                    } else {
                        Value::Str(chars[i as usize].to_string())
                    }
                }
                (Value::HashMap(map), key) => {
                    // missing key returns Nil so ?? can provide a default: map[k] ?? fallback
                    map.lock().unwrap()
                        .iter()
                        .find(|(k, _)| values_equal(k, &key))
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Nil)
                }
                _ => nova_error(eval_line(), "cannot index into this value"),
            }
        }

        Expr::IndexAssign { name, index, value } => {
            let key = eval(index, env);
            let val = eval(value, env);
            match env.get_mut(name) {
                Some(Value::Array(arr)) => {
                    let idx = match &key {
                        Value::Int(n)   => {
                            let len = arr.lock().unwrap().len() as i64;
                            let i = if *n < 0 { len + n } else { *n };
                            if i < 0 || i >= len {
                                nova_error(eval_line(), &format!("array index {} out of bounds", n));
                            }
                            i as usize
                        }
                        Value::Float(n) => *n as usize,
                        _ => nova_error(eval_line(), "array index must be a number"),
                    };
                    // copy-on-write: if this array is shared (Arc count > 1), clone before mutating
                    if Arc::strong_count(arr) > 1 {
                        let cloned = arr.lock().unwrap().clone();
                        *arr = Arc::new(Mutex::new(cloned));
                    }
                    let len = arr.lock().unwrap().len();
                    if idx < len {
                        arr.lock().unwrap()[idx] = val;
                    } else {
                        nova_error(eval_line(), &format!("array index {} out of bounds", idx));
                    }
                }
                Some(Value::HashMap(map)) => {
                    // copy-on-write for hashmaps
                    if Arc::strong_count(map) > 1 {
                        let cloned = map.lock().unwrap().clone();
                        *map = Arc::new(Mutex::new(cloned));
                    }
                    let mut m = map.lock().unwrap();
                    if let Some(pair) = m.iter_mut().find(|(k, _)| values_equal(k, &key)) {
                        pair.1 = val; // update existing key
                    } else {
                        m.push((key, val)); // insert new key
                    }
                }
                _ => nova_error(eval_line(), &format!("'{}' is not an array or hashmap", name)),
            }
            Value::Nil
        }

        // for a, b in arr — two behaviours depending on what the array contains:
        //   if elements are 2-item arrays (e.g. from zip), unpack: a=pair[0], b=pair[1]
        //   otherwise enumerate: a=index, b=value
        Expr::ForEnumerate { index_var, item_var, iter, body } => {
            match eval(iter, env) {
                Value::Array(arr) => {
                    let items = arr.lock().unwrap().clone(); // clone inner vec so the borrow is released before the loop body runs
                    for (i, val) in items.into_iter().enumerate() {
                        match &val {
                            Value::Array(pair) if pair.lock().unwrap().len() == 2 => {
                                // zip-style: unpack the pair directly into both variables
                                env.insert(index_var.clone(), pair.lock().unwrap()[0].clone());
                                env.insert(item_var.clone(),  pair.lock().unwrap()[1].clone());
                            }
                            _ => {
                                // enumerate-style: index + value
                                env.insert(index_var.clone(), Value::Int(i as i64));
                                env.insert(item_var.clone(),  val.clone());
                            }
                        }
                        match eval_block(body, env) {
                            Value::Break         => break,
                            Value::Continue      => continue,
                            r @ Value::Return(_) => return r,
                            _ => {}
                        }
                    }
                }
                _ => nova_error(eval_line(), "enumerate loop requires an array"),
            }
            Value::Nil
        }

        Expr::ForDestructure { vars, iter, body } => {
            match eval(iter, env) {
                Value::Array(arr) => {
                    let items = arr.lock().unwrap().clone();
                    for val in items {
                        match val {
                            Value::Array(pair) => {
                                let elems = pair.lock().unwrap().clone();
                                if elems.len() != vars.len() {
                                    nova_error(eval_line(), &format!(
                                        "destructure pattern has {} variables but element has {} items",
                                        vars.len(), elems.len()
                                    ));
                                }
                                for (v, e) in vars.iter().zip(elems.into_iter()) {
                                    env.insert(v.clone(), e);
                                }
                            }
                            _ => nova_error(eval_line(), "destructure loop requires each element to be an array"),
                        }
                        match eval_block(body, env) {
                            Value::Break         => break,
                            Value::Continue      => continue,
                            r @ Value::Return(_) => return r,
                            _ => {}
                        }
                    }
                }
                _ => nova_error(eval_line(), "destructure loop requires an array"),
            }
            Value::Nil
        }

        Expr::For { var, iter, body } => {
            match eval(iter, env) {
                Value::Array(arr) => {
                    let items = arr.lock().unwrap().clone(); // clone inner vec to release the borrow before eval_block
                    for val in items {
                        env.insert(var.clone(), val);
                        match eval_block(body, env) {
                            Value::Break         => break,
                            Value::Continue      => continue,
                            r @ Value::Return(_) => return r,
                            _ => {}
                        }
                    }
                }
                Value::Range(start, end) => {
                    let mut i = start;
                    while i < end {
                        env.insert(var.clone(), Value::Int(i));
                        let sig = eval_block(body, env);
                        i += 1;
                        match sig {
                            Value::Break         => break,
                            Value::Continue      => continue,
                            r @ Value::Return(_) => return r,
                            _ => {}
                        }
                    }
                }
                Value::Str(s) => {
                    for ch in s.chars() {
                        env.insert(var.clone(), Value::Str(ch.to_string()));
                        match eval_block(body, env) {
                            Value::Break         => break,
                            Value::Continue      => continue,
                            r @ Value::Return(_) => return r,
                            _ => {}
                        }
                    }
                }
                _ => nova_error(eval_line(), "can only iterate over arrays, ranges, and strings"),
            }
            Value::Nil
        }

        // match x { pattern => body, _ => fallback }
        Expr::Match { value, arms } => {
            let val = eval(value, env);
            for (pattern, body) in arms {
                match pattern {
                    None => {
                        return eval_block(body, env);
                    }
                    Some(Expr::EnumPattern { enum_name, variant, bindings }) => {
                        if let Value::EnumVariant { enum_name: en, variant: vn, payload } = &val {
                            if en == enum_name && vn == variant {
                                for (b, pval) in bindings.iter().zip(payload.iter()) {
                                    env.insert(b.clone(), pval.clone());
                                }
                                return eval_block(body, env);
                            }
                        }
                    }
                    Some(p) => {
                        let pval = eval(p, env);
                        let matched = match (&pval, &val) {
                            (Value::Range(start, end), Value::Int(n)) => *n >= *start && *n < *end,
                            (Value::Range(start, end), Value::Float(n)) => *n >= *start as f64 && *n < *end as f64,
                            _ => values_equal(&pval, &val),
                        };
                        if matched {
                            return eval_block(body, env);
                        }
                    }
                }
            }
            Value::Nil
        }

        Expr::Range { start, end } => {
            let s = match eval(start, env) { Value::Int(n) => n, Value::Float(n) => n as i64, _ => nova_error(eval_line(), "range start must be a number") };
            let e = match eval(end, env)   { Value::Int(n) => n, Value::Float(n) => n as i64, _ => nova_error(eval_line(), "range end must be a number") };
            Value::Range(s, e)
        }

        Expr::EnumPattern { .. } => nova_error(eval_line(), "enum pattern cannot appear outside a match arm"),
    }
}

// Maps a Value to its Nova type name string for error messages.
fn runtime_type_of(v: &Value) -> &'static str {
    match v {
        Value::Int(_)     => "int",
        Value::Float(_)   => "float",
        Value::Str(_)     => "string",
        Value::Bool(_)    => "bool",
        Value::Array(_)   => "array",
        Value::HashMap(_) => "hashmap",
        Value::Nil        => "nil",
        _                 => "unknown",
    }
}

// Returns true if a value satisfies a declared type annotation.
// int widens to float. All other mismatches are errors.
fn runtime_type_matches(annotation: &str, v: &Value) -> bool {
    match (annotation, v) {
        ("int",     Value::Int(_))     => true,
        ("float",   Value::Float(_))   => true,
        ("float",   Value::Int(_))     => true, // int widens to float
        ("string",  Value::Str(_))     => true,
        ("bool",    Value::Bool(_))    => true,
        ("array",   Value::Array(_))   => true,
        ("hashmap", Value::HashMap(_)) => true,
        ("nil",     Value::Nil)        => true,
        // function-type annotation e.g. "(T) -> U" — check it's callable, not the inner types
        (ann, Value::Function { .. } | Value::VmFunc(_)) if ann.starts_with('(') && ann.contains("->") => true,
        _ => false,
    }
}

// Runs a list of expressions in order and returns the value of the last one.
// Immediately returns Break, Continue, or Return sentinels so they bubble up to the right handler.
fn eval_block(exprs: &[Expr], env: &mut Env) -> Value {
    let mut result = Value::Nil;
    for expr in exprs {
        result = eval(expr, env);
        if matches!(result, Value::Break | Value::Continue | Value::Return(_)) { return result; } // bubble signal upward
    }
    result
}

// Helper used by map, filter, reduce, find, any, all, count — calls a Value::Function with given args.
fn call_function(func: Value, args: Vec<Value>, env: &mut Env) -> Value {
    match func {
        Value::Function { params, param_types: _, defaults, body, variadic, captured_env } => {
            let captured_arc = captured_env.clone();
            let is_closure = captured_arc.is_some();
            let outer_keys: Vec<String> = if !is_closure { env.keys().cloned().collect() } else { vec![] };
            let mut fn_env = match captured_arc.as_ref() {
                Some(arc) => arc.lock().unwrap().clone(),
                None => env.clone(),
            };
            if variadic {
                let regular = params.len() - 1;
                for (param, val) in params[..regular].iter().zip(args.iter()) {
                    fn_env.insert(param.clone(), val.clone());
                }
                fn_env.insert(params[regular].clone(), make_array(args[regular..].to_vec()));
            } else {
                for (i, param) in params.iter().enumerate() {
                    let val = args.get(i).cloned()
                        .or_else(|| defaults.get(i).and_then(|d| d.clone()))
                        .unwrap_or(Value::Nil);
                    fn_env.insert(param.clone(), val);
                }
            }
            if !is_closure { push_local_frame(); }
            let result = match eval_block(&body, &mut fn_env) {
                Value::Return(v) => *v,
                other => other,
            };
            let local_set = if !is_closure { pop_local_frame() } else { HashSet::new() };
            if let Some(arc) = &captured_arc {
                let mut guard = arc.lock().unwrap();
                for k in guard.keys().cloned().collect::<Vec<_>>() {
                    if let Some(v) = fn_env.get(&k) { guard.insert(k, v.clone()); }
                }
            }
            if !is_closure {
                let param_set: std::collections::HashSet<&str> =
                    params.iter().map(|p| p.as_str()).collect();
                for k in &outer_keys {
                    if !param_set.contains(k.as_str()) && !local_set.contains(k) {
                        if let Some(v) = fn_env.get(k) {
                            env.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            result
        }
        _ => nova_error(eval_line(), "expected a function"),
    }
}


// Uses a 2D grid (dynamic programming) where each cell stores the edit distance
// between the first i characters of a and the first j characters of b.
// Damerau-Levenshtein distance (OSA variant) — counts inserts, deletes, replaces,
// and transpositions (swapping two adjacent characters) each as a single edit.
// Example: "naem" → "name" is 1 transposition, so distance = 1
// Regular Levenshtein would count that as 2 edits (delete + insert).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 0..=m { dp[i][0] = i; } // deleting all chars of a costs i edits
    for j in 0..=n { dp[0][j] = j; } // inserting all chars of b costs j edits

    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1]; // characters match — no edit needed
            } else {
                dp[i][j] = 1 + dp[i-1][j-1]  // replace
                    .min(dp[i-1][j])           // delete from a
                    .min(dp[i][j-1]);          // insert into a
            }
            // transposition: check if swapping two adjacent chars would be cheaper
            if i > 1 && j > 1 && a[i-1] == b[j-2] && a[i-2] == b[j-1] {
                dp[i][j] = dp[i][j].min(dp[i-2][j-2] + 1);
            }
        }
    }
    dp[m][n]
}

// Finds the closest match to `name` among `candidates`.
// Returns Some("suggestion") if a match within distance 2 is found, otherwise None.
fn did_you_mean(name: &str, candidates: &[&str]) -> Option<String> {
    candidates.iter()
        .map(|c| (*c, levenshtein(name, c)))
        .filter(|(_, dist)| *dist <= 2 && *dist > 0) // dist > 0 excludes exact matches
        .min_by_key(|(_, dist)| *dist)
        .map(|(c, _)| c.to_string())
}

// Checks whether two Values are equal — used by contains(), hasKey(), match arms.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x),   Value::Int(y))   => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x),   Value::Float(y)) => (*x as f64) == *y,
        (Value::Float(x), Value::Int(y))   => *x == (*y as f64),
        (Value::Str(x),   Value::Str(y))   => x == y,
        (Value::Bool(x),  Value::Bool(y))  => x == y,
        (Value::Nil,      Value::Nil)      => true,
        (Value::Array(a), Value::Array(b)) => {
            let a = a.lock().unwrap();
            let b = b.lock().unwrap();
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| values_equal(x, y))
        }
        (Value::HashMap(a), Value::HashMap(b)) => {
            let a = a.lock().unwrap();
            let b = b.lock().unwrap();
            a.len() == b.len() && a.iter().all(|(k, v)| {
                b.iter().any(|(k2, v2)| values_equal(k, k2) && values_equal(v, v2))
            })
        }
        (Value::EnumVariant { enum_name: en1, variant: v1, payload: p1 },
         Value::EnumVariant { enum_name: en2, variant: v2, payload: p2 }) => {
            en1 == en2 && v1 == v2 && p1.len() == p2.len()
                && p1.iter().zip(p2.iter()).all(|(x, y)| values_equal(x, y))
        }
        (Value::Struct { fields: fa, .. }, Value::Struct { fields: fb, .. }) => Arc::ptr_eq(fa, fb),
        _ => false,
    }
}

// Converts a Value to a human-readable string for printing.
// pub so the REPL can use it for auto-printing expression results.
pub fn format_value(v: &Value) -> String {
    match v {
        Value::Int(n)   => n.to_string(),
        Value::Float(n) => {
            if n.is_nan()          { return "NaN".to_string(); }
            if n.is_infinite()     { return if *n > 0.0 { "inf".to_string() } else { "-inf".to_string() }; }
            let s = format!("{}", n);
            if s.contains('.') || s.contains('e') { s } else { format!("{}.0", s) }
        }
        Value::Str(s)        => s.clone(),
        Value::Bool(b)       => b.to_string(),
        Value::Nil           => "nil".to_string(),
        Value::Array(arr)    => {
            let parts: Vec<String> = arr.lock().unwrap().iter().map(|v| format_value(v)).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::HashMap(map)  => {
            let parts: Vec<String> = map.lock().unwrap().iter()
                .map(|(k, v)| format!("{}: {}", format_value(k), format_value(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Range(s, e)   => format!("{}..{}", s, e),
        Value::Function {..} => "<function>".to_string(),
        Value::VmFunc(_)     => "<function>".to_string(),
        Value::Break         => "<break>".to_string(),
        Value::Continue      => "<continue>".to_string(),
        Value::Return(_)     => "<return>".to_string(),
        Value::StructDef { name, field_names } => format!("<struct {} {{ {} }}>", name, field_names.join(", ")),
        Value::Struct { name, fields } => {
            let parts: Vec<String> = fields.lock().unwrap().iter()
                .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                .collect();
            format!("{} {{ {} }}", name, parts.join(", "))
        }
        Value::EnumDef { name, .. }  => format!("<enum {}>", name),
        Value::EnumVariant { enum_name, variant, payload } => {
            if payload.is_empty() {
                format!("{}.{}", enum_name, variant)
            } else {
                let parts: Vec<String> = payload.iter().map(|v| format_value(v)).collect();
                format!("{}.{}({})", enum_name, variant, parts.join(", "))
            }
        }
        Value::EnumConstructor { .. } => "<enum constructor>".to_string(),
        Value::Channel(_) => "<channel>".to_string(),
        Value::Mutex(_)   => "<mutex>".to_string(),
        Value::Task(_) => "<task>".to_string(),
    }
}
