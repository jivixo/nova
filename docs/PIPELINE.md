# Nova Compiler Pipeline (Technical Trace)

This document traces the same Nova program through all three execution backends:
the tree-walking interpreter, the bytecode VM, and the LLVM native code generator.

## Example Program

```nova
let total = 0
for i in 0..3 {
    for j in 0..3 {
        total = total + 1
    }
}
print total
```

Expected output: `9`

---

## Stage 1: Front End (shared by all backends)

### 1.1 Lexer (`lexer.rs`)

The lexer converts raw source text into a `Vec<(Token, usize)>`, where each token is paired with its source line number. Source is stored as `Vec<char>` for O(1) character access.

Key tokens produced (abbreviated):

```
(Let,      1)  (Ident("total"), 1)  (Equals, 1)  (IntLit(0),  1)
(For,      2)  (Ident("i"),     2)  (In,     2)  (IntLit(0),  2)  (DotDot, 2)  (IntLit(3), 2)  (LBrace, 2)
(For,      3)  (Ident("j"),     3)  (In,     3)  (IntLit(0),  3)  (DotDot, 3)  (IntLit(3), 3)  (LBrace, 3)
(Ident("total"), 4)  (Equals, 4)  (Ident("total"), 4)  (Plus, 4)  (IntLit(1), 4)
(RBrace,   5)  (RBrace, 6)
(Print,    7)  (Ident("total"), 7)
(EOF,      8)
```

### 1.2 Parser (`parser.rs`)

The parser is a hand-written recursive descent parser. It produces a `Vec<Expr>` (a tree of AST nodes). Every statement is wrapped in `Expr::Line(line, Box<Expr>)` to carry source line numbers.

AST produced:

```
Line(1, Let { name: "total", value: IntLit(0) })

Line(2, For {
    var: "i",
    iter: Range { start: IntLit(0), end: IntLit(3) },
    body: [
        Line(3, For {
            var: "j",
            iter: Range { start: IntLit(0), end: IntLit(3) },
            body: [
                Line(4, Assign {
                    name: "total",
                    value: BinaryOp {
                        left:  Ident("total"),
                        op:    Plus,
                        right: IntLit(1)
                    }
                })
            ]
        })
    ]
})

Line(7, Print(Ident("total")))
```

The range `0..3` is parsed into `Expr::Range { start, end }` rather than a `Range` value. It is only meaningful inside a `for` loop header and is handled directly by the loop compilation/evaluation logic.

### 1.3 Warnings (`warnings.rs`)

Two-pass walk over the AST:
- Pass 1: collect every name that was declared (`let`, `for` loop vars, `fn` params)
- Pass 2: collect every name that was read (`Ident` in expression position)

Declarations not in the used set → unused-variable warning to stderr. This program produces no warnings: `total`, `i`, `j` are all read.

### 1.4 Type Checker (`typechecker.rs`)

Three-pass static analysis. No annotations in this program so all three passes produce no errors:
- Pass 1 (signature collection): no `fn` declarations, nothing to collect
- Pass 2 (return type inference): nothing to infer
- Pass 3 (call-site checking): no calls with typed parameters

---

## Stage 2: Tree-Walking Interpreter (`evaluator.rs`)

The tree-walker calls `eval(expr, env)` recursively. `env` is a `HashMap<String, Value>`.

### Eval trace

**Statement 1:** `let total = 0`
```
eval(Line(1, Let { name: "total", value: IntLit(0) }), env)
  → set_eval_line(1)
  → eval(Let { name: "total", value: IntLit(0) }, env)
      → eval(IntLit(0), env)  ==>  Value::Int(0)
      → env.insert("total", Int(0))
      ==> Nil
```

**Statement 2:** outer `for i in 0..3 { ... }`

The evaluator matches `Expr::For` and sees `Expr::Range`. It iterates i over 0, 1, 2:

```
eval(For { var: "i", iter: Range(0,3), body: [...] }, env)

  iteration i=0:
    env.insert("i", Int(0))
    eval(For { var: "j", iter: Range(0,3), body: [...] }, env)

      iteration j=0:
        env.insert("j", Int(0))
        eval(Assign { name: "total", value: BinaryOp(total+1) }, env)
          eval(BinaryOp { left: Ident("total"), op: Plus, right: IntLit(1) }, env)
            eval(Ident("total"), env)  ==>  Int(0)
            eval(IntLit(1), env)       ==>  Int(1)
            Int(0) + Int(1)            ==>  Int(1)
          env.insert("total", Int(1))
        ==> Nil

      iteration j=1:
        env["j"] = Int(1)
        total + 1 = Int(1) + Int(1) = Int(2)  → env["total"] = Int(2)

      iteration j=2:
        env["j"] = Int(2)
        total + 1 = Int(2) + Int(1) = Int(3)  → env["total"] = Int(3)

  iteration i=1:
    env.insert("i", Int(1))
    inner loop j in 0..3:  total goes 3 → 4 → 5 → 6

  iteration i=2:
    env.insert("i", Int(2))
    inner loop j in 0..3:  total goes 6 → 7 → 8 → 9
```

**Statement 3:** `print total`
```
eval(Print(Ident("total")), env)
  → eval(Ident("total"), env)  ==>  Int(9)
  → println!("9")
```

**Output:** `9`

**Memory:** After each top-level statement, `collect_cycles(env)` runs a mark-and-sweep over the `ARRAY_HEAP` and `MAP_HEAP` registries. This program allocates no arrays or hashmaps, so no GC work is done.

---

## Stage 3: Bytecode VM (`compiler.rs` + `vm.rs`)

### 3.1 Compilation

The compiler makes a single pass over the AST, emitting a flat `Vec<Instruction>` called a `Chunk`. Forward jumps are handled by backpatching: a placeholder `JumpIfFalse(0)` is emitted and overwritten once the target index is known.

For-range loops are compiled with hidden sentinel variables (`__end_N`) to hold the loop bound, and with the loop counter initialised before the loop header. The compiler tracks nested loops via a `loop_stack` of `LoopFrame`s, each collecting placeholder positions for `break`/`continue`.

Full bytecode for the example program:

```
ip  0   LoadConst(Int(0))         -- let total = 0
ip  1   DefineGlobal("total")

ip  2   LoadConst(Int(0))         -- for i in 0..3 : init counter and bound
ip  3   DefineGlobal("i")
ip  4   LoadConst(Int(3))
ip  5   DefineGlobal("__end_0")

         -- OUTER LOOP TOP (loop_top = 6)
ip  6   LoadGlobal("i")
ip  7   LoadGlobal("__end_0")
ip  8   Less                      -- i < 3 ?
ip  9   JumpIfFalse(32)           -- exit outer loop

ip 10   LoadConst(Int(0))         -- for j in 0..3 : init counter and bound
ip 11   DefineGlobal("j")
ip 12   LoadConst(Int(3))
ip 13   DefineGlobal("__end_1")

         -- INNER LOOP TOP (loop_top = 14)
ip 14   LoadGlobal("j")
ip 15   LoadGlobal("__end_1")
ip 16   Less                      -- j < 3 ?
ip 17   JumpIfFalse(27)           -- exit inner loop

ip 18   LoadGlobal("total")       -- total = total + 1
ip 19   LoadConst(Int(1))
ip 20   Add
ip 21   StoreGlobal("total")

         -- INNER LOOP CONTINUE (continue_target = 22)
ip 22   LoadGlobal("j")
ip 23   LoadConst(Int(1))
ip 24   Add
ip 25   StoreGlobal("j")
ip 26   Jump(14)                  -- back to inner loop top

         -- INNER LOOP EXIT (exit = 27)
         -- OUTER LOOP CONTINUE (continue_target = 27)
ip 27   LoadGlobal("i")
ip 28   LoadConst(Int(1))
ip 29   Add
ip 30   StoreGlobal("i")
ip 31   Jump(6)                   -- back to outer loop top

         -- OUTER LOOP EXIT (exit = 32)
ip 32   LoadGlobal("total")       -- print total
ip 33   Print
```

Key compiler mechanics:
- `JumpIfFalse(0)` at ip 9 is emitted first with target `0`, then `chunk.patch(9, JumpIfFalse(32))` is called once ip 32 is known
- Same for ip 17 → target 27
- `loop_counter` is incremented per loop so `__end_0` and `__end_1` never collide in nested loops
- Hidden globals (`__end_0`, `__end_1`, `j`, `i`) are real entries in the globals `HashMap`; they shadow nothing because their names cannot be written in Nova source

### 3.2 VM Execution

The VM maintains:
- `stack: Vec<Value>`: operand stack
- `globals: HashMap<String, Value>`: shared across all frames
- `frames: Vec<CallFrame>`: call stack; each frame has its own `ip` and `locals`

For this top-level program there is one frame throughout. Abbreviated execution trace:

```
ip= 0  LoadConst(Int(0))          stack: [0]
ip= 1  DefineGlobal("total")      stack: []    globals: {total:0}
ip= 2  LoadConst(Int(0))          stack: [0]
ip= 3  DefineGlobal("i")          stack: []    globals: {total:0, i:0}
ip= 4  LoadConst(Int(3))          stack: [3]
ip= 5  DefineGlobal("__end_0")    stack: []    globals: {total:0, i:0, __end_0:3}
ip= 6  LoadGlobal("i")            stack: [0]
ip= 7  LoadGlobal("__end_0")      stack: [0, 3]
ip= 8  Less                       stack: [true]   (0 < 3)
ip= 9  JumpIfFalse(32)            → condition true, fall through
ip=10  LoadConst(Int(0))          stack: [0]
ip=11  DefineGlobal("j")          stack: []    globals: {..., j:0}
ip=12  LoadConst(Int(3))          stack: [3]
ip=13  DefineGlobal("__end_1")    stack: []    globals: {..., __end_1:3}
ip=14  LoadGlobal("j")            stack: [0]
ip=15  LoadGlobal("__end_1")      stack: [0, 3]
ip=16  Less                       stack: [true]   (0 < 3)
ip=17  JumpIfFalse(27)            → fall through
ip=18  LoadGlobal("total")        stack: [0]
ip=19  LoadConst(Int(1))          stack: [0, 1]
ip=20  Add                        stack: [1]
ip=21  StoreGlobal("total")       stack: []    globals: {total:1}
ip=22  LoadGlobal("j")            stack: [0]
ip=23  LoadConst(Int(1))          stack: [0, 1]
ip=24  Add                        stack: [1]
ip=25  StoreGlobal("j")           stack: []    globals: {j:1}
ip=26  Jump(14)                   → ip = 14

         ... (j=1: total=2, j=2: total=3, then JumpIfFalse(27) fires) ...

ip=27  LoadGlobal("i")            stack: [0]
ip=28  LoadConst(Int(1))          stack: [0, 1]
ip=29  Add                        stack: [1]
ip=30  StoreGlobal("i")           stack: []    globals: {i:1}
ip=31  Jump(6)                    → ip = 6

         ... (i=1: inner loop runs, total→6; i=2: inner loop runs, total→9) ...

ip= 9  JumpIfFalse(32)            → i=3, 3 < 3 is false → ip = 32
ip=32  LoadGlobal("total")        stack: [9]
ip=33  Print                      → prints "9", stack: []
```

**Output:** `9`

---

## Stage 4: LLVM Native Code Generator (`codegen.rs` + `nova_rt.c`)

### 4.1 Value Representation

Every Nova value in the LLVM backend is a `NovaValue`, a 16-byte tagged struct:

```c
typedef struct { uint64_t tag; uint64_t payload; } NovaValue;
```

Tags: `0`=nil, `1`=bool, `2`=int, `3`=float, `4`=str, `5`=array, `6`=map, `7`=closure, `8`=enum, `9`=task, `10`=chan.

Every Nova variable is an `alloca %NovaValue`, a 16-byte slot on the LLVM stack frame. All runtime functions take and return values through pointers to these slots (never by value, to avoid struct-return ABI issues between platforms).

### 4.2 LLVM IR: Preamble

The code generator emits a full `.ll` file. The preamble defines the `%NovaValue` type and declares all runtime function signatures:

```llvm
%NovaValue = type { i64, i64 }

declare void @nova_make_int(i64, ptr)
declare void @nova_add(ptr, ptr, ptr)
declare void @nova_cmp_lt(ptr, ptr, ptr)
declare void @nova_print(ptr)
declare i1   @nova_truthy(ptr)
```

### 4.3 LLVM IR: Variable Allocation (alloca hoisting)

Each Nova variable corresponds to an `alloca %NovaValue` in the LLVM entry block. The code generator initially emits allocas at the point of first use. A post-processing pass (`hoist_allocas`) moves all `alloca %NovaValue, align 8` instructions to the function entry block. This is required for LLVM's `mem2reg` promotion pass and prevents stack growth inside loops (each loop iteration would otherwise execute a `sub rsp, 16` at the machine level).

```llvm
define i32 @main() {
entry:
  %local_total  = alloca %NovaValue, align 8
  %local_i      = alloca %NovaValue, align 8
  %local_end_0  = alloca %NovaValue, align 8
  %local_j      = alloca %NovaValue, align 8
  %local_end_1  = alloca %NovaValue, align 8
  %t0 = alloca %NovaValue, align 8    ; scratch temporaries
  %t1 = alloca %NovaValue, align 8
  %t2 = alloca %NovaValue, align 8
  ...
```

### 4.4 LLVM IR: Initialisation

```llvm
  ; let total = 0
  call void @nova_make_int(i64 0, ptr %local_total)

  ; for i in 0..3 — init i and __end_0
  call void @nova_make_int(i64 0, ptr %local_i)
  call void @nova_make_int(i64 3, ptr %local_end_0)
  br label %outer_top
```

### 4.5 LLVM IR: Outer Loop

```llvm
outer_top:
  ; i < __end_0 ?
  call void @nova_cmp_lt(ptr %local_i, ptr %local_end_0, ptr %t0)
  %cond_outer = call i1 @nova_truthy(ptr %t0)
  br i1 %cond_outer, label %outer_body, label %outer_exit

outer_body:
  ; for j in 0..3 — init j and __end_1
  call void @nova_make_int(i64 0, ptr %local_j)
  call void @nova_make_int(i64 3, ptr %local_end_1)
  br label %inner_top
```

### 4.6 LLVM IR: Inner Loop Body (with type specialisation)

The type specialisation pass detects that `total` was assigned `Int(0)` and `1` is a literal integer, so `total + 1` is statically known to be `int + int`. Instead of calling `@nova_add`, the code generator emits direct LLVM integer arithmetic:

```llvm
inner_top:
  ; j < __end_1 ?
  call void @nova_cmp_lt(ptr %local_j, ptr %local_end_1, ptr %t1)
  %cond_inner = call i1 @nova_truthy(ptr %t1)
  br i1 %cond_inner, label %inner_body, label %inner_exit

inner_body:
  ; total = total + 1  (TYPE-SPECIALISED: both operands are statically int)
  %raw_total_ptr = getelementptr %NovaValue, ptr %local_total, i64 0, i32 1
  %val_total     = load i64, ptr %raw_total_ptr, align 8
  %new_total     = add i64 %val_total, 1
  ; store back: tag = 2 (int), payload = new_total
  %tag_ptr  = getelementptr %NovaValue, ptr %local_total, i64 0, i32 0
  %pay_ptr  = getelementptr %NovaValue, ptr %local_total, i64 0, i32 1
  store i64 2,         ptr %tag_ptr,  align 8
  store i64 %new_total, ptr %pay_ptr, align 8

  ; j += 1  (also specialised)
  %raw_j_ptr = getelementptr %NovaValue, ptr %local_j, i64 0, i32 1
  %val_j     = load i64, ptr %raw_j_ptr, align 8
  %new_j     = add i64 %val_j, 1
  %jtag_ptr  = getelementptr %NovaValue, ptr %local_j, i64 0, i32 0
  %jpay_ptr  = getelementptr %NovaValue, ptr %local_j, i64 0, i32 1
  store i64 2,     ptr %jtag_ptr, align 8
  store i64 %new_j, ptr %jpay_ptr, align 8
  br label %inner_top

inner_exit:
  ; i += 1  (specialised)
  %raw_i_ptr = getelementptr %NovaValue, ptr %local_i, i64 0, i32 1
  %val_i     = load i64, ptr %raw_i_ptr, align 8
  %new_i     = add i64 %val_i, 1
  %itag_ptr  = getelementptr %NovaValue, ptr %local_i, i64 0, i32 0
  %ipay_ptr  = getelementptr %NovaValue, ptr %local_i, i64 0, i32 1
  store i64 2,     ptr %itag_ptr, align 8
  store i64 %new_i, ptr %ipay_ptr, align 8
  br label %outer_top

outer_exit:
  ; print total
  call void @nova_print(ptr %local_total)
  ret i32 0
}
```

Without type specialisation the inner body would instead call `@nova_add`:
```llvm
  ; unspecialised fallback (used when types are unknown)
  call void @nova_add(ptr %local_total, ptr %t_const_1, ptr %t2)
  call void @nova_copy(ptr %t2, ptr %local_total)
```

### 4.7 Build Steps

```
1. codegen.rs walks AST → writes <temp>/nova_XXXX.ll
2. nova_rt.o  (compiled once from nova_rt.c, auto-rebuilt when nova_rt.c changes)
3. clang <temp>/nova_XXXX.ll nova_rt.o -o <temp>/nova_XXXX.exe
4. <temp>/nova_XXXX.exe runs → prints 9
5. Both temp files deleted automatically
```
`<temp>` is the system temp directory (`%TEMP%` on Windows, `/tmp` on Linux/macOS).

`nova build` replaces step 4–5 with keeping `file.ll` and `file.exe` next to the source.

### 4.8 Output

```
9
```

Identical to both interpreter and VM. The three backends always produce the same output for the same program. Differential testing against the tree-walker was used throughout development to verify correctness of each new backend.
