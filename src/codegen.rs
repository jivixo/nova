// codegen.rs — Nova AST to LLVM IR code generator
//
// How LLVM IR works (quick primer):
//   - LLVM IR is a text format that looks like assembly but is typed and portable.
//   - Every function is made of "basic blocks" — straight-line sequences of instructions
//     that always end with a terminator (br, ret). Control flow is explicit jumps between blocks.
//   - LLVM IR is in SSA (Static Single Assignment) form: every value is assigned exactly once.
//     Instead of mutating a variable, you create a new name. We use %t0, %t1, %t2, ... for temps.
//   - Memory (mutable variables) is handled with alloca (stack-allocate) + pointer passing.
//     We never use load/store directly — every Nova value is an alloca'd NovaValue on the stack,
//     and we pass pointers (ptr) to runtime functions that read or write through the pointer.
//
// How Nova values are represented:
//   - Every Nova value is a NovaValue = { i64 tag, i64 payload } — 16 bytes on the stack.
//   - tag encodes the type: 0=nil, 1=bool, 2=int, 3=float, 4=str
//   - payload is the actual data: int/bool stored directly, float stored as bit pattern, str as ptr
//   - All operations (add, compare, print, etc.) are C functions in nova_rt.c that take pointers.
//     This avoids struct-return ABI problems — we never return a NovaValue by value.
//
// How user-defined functions are compiled:
//   - Each Nova fn becomes a separate `define void @nova_fn_<name>(...)` in the .ll file.
//   - Parameters are passed as pointers (ptr %arg_X) — the function copies them to local alloca
//     slots so params are mutable without affecting the caller's values.
//   - The return value is passed via an extra output pointer (ptr %_ret) appended at the end
//     of the parameter list. The function copies its result into %_ret before returning.
//   - Nova's implicit return (last expression is the return value) is handled by compiling all
//     but the last body statement as statements, then the last as an expression copied to %_ret.
//   - Explicit `return val` compiles to: copy val to %_ret, branch to a shared exit label.
//
// Compile flow:
//   1. compile_program() first pre-scans all top-level Fn nodes and registers their names so
//      forward references and mutual recursion work correctly.
//   2. It then calls compile_stmt() for each statement to produce the @main body, which also
//      triggers function compilation (Fn nodes push complete `define` blocks into self.functions).
//   3. After the body is built, compile_program() assembles the full .ll text:
//      type def → string globals → runtime declares → function defines → define i32 @main() { body }

use crate::parser::Expr;
use crate::lexer::{Lexer, Token, StringPart};
use crate::parser::Parser;

// Static type used by the type-specialisation pass.
// Only covers types that unlock fast-path arithmetic; everything else is Unknown.
#[derive(Clone, PartialEq, Debug)]
enum StaticType { Int, Float, Bool, Str, Struct(String), Unknown }

pub struct Codegen {
    // accumulates the .ll text for the current compilation unit (either @main or a function body)
    out: String,

    // counter for generating unique SSA names: fresh() returns %t0, %t1, %t2, ...
    // also used for label suffixes so every basic block name is unique
    tmp: usize,

    // scope stack for variable lookup
    // each entry is a map from Nova variable name to its alloca pointer name in LLVM IR
    // e.g. "x" → "%local_x" means: the Nova variable x lives at the stack slot %local_x
    // we push a new scope on entering a block and pop it on exit
    // lookup walks from innermost to outermost (shadowing works automatically)
    locals: Vec<std::collections::HashMap<String, String>>,

    // string constants collected during codegen — emitted at the top of the .ll file as globals
    // stored as (global_name, raw_content), e.g. ("@str0", "hello world")
    strings: Vec<(String, String)>,

    // counter for generating unique string global names (@str0, @str1, ...)
    str_ctr: usize,

    // completed function definitions collected during compilation
    // each entry is a full `define void @nova_fn_X(...) { ... }` block as a string
    // emitted before @main so all user functions are visible when @main is assembled
    functions: Vec<String>,

    // name of the return-value pointer in the function currently being compiled
    // None when compiling @main (top-level code has no return slot)
    ret_ptr: Option<String>,

    // label name of the exit block in the function currently being compiled
    // explicit `return` statements branch here; the implicit return at the end also branches here
    fn_exit_label: Option<String>,

    // names of all user-defined functions encountered so far
    // used to distinguish user functions from builtins in Call nodes —
    // only user-defined functions get a real @nova_fn_X call; others emit nil
    defined_fns: std::collections::HashSet<String>,

    // enum definitions encountered so far: enum_name → list of (variant_name, arity)
    // used to detect enum constructor expressions: Direction.North or Shape.Circle(r)
    enum_defs: std::collections::HashMap<String, Vec<(String, usize)>>,

    // impl methods registered so far: (type_name, method_name)
    // used by MethodCall to emit a direct @nova_method_TypeName_method call
    defined_methods: std::collections::HashSet<(String, String)>,

    // static return type of each method: (type_name, method_name) → StaticType
    // populated during pre-scan so infer_type(MethodCall{...}) can propagate struct types
    method_ret_types: std::collections::HashMap<(String, String), StaticType>,

    // loop context stack for break/continue — each entry is (continue_label, break_label)
    // pushed on entering a for/while loop, popped on exit
    // break → br to break_label; continue → br to continue_label
    loop_ctx: Vec<(String, String)>,

    // try context stack — each entry is the catch label of the enclosing try block
    // pushed when compiling a try body, popped when done
    // throw inside a try body: br directly to the innermost catch label
    // throw outside any try (inside a function): br to fn_exit
    try_ctx: Vec<String>,

    // canonical paths of files currently being imported — prevents circular imports
    importing: std::collections::HashSet<String>,
    // canonical paths of files already fully imported — prevents duplicate compilation
    imported: std::collections::HashSet<String>,

    // type environment for the static type inference pass
    // parallel to locals: each scope maps variable name → inferred StaticType
    // lets BinaryOp emit direct LLVM arithmetic instead of calling nova_add/nova_mul etc.
    type_env: Vec<std::collections::HashMap<String, StaticType>>,
}

impl Codegen {
    pub fn new() -> Self {
        Codegen {
            out:          String::new(),
            tmp:          0,
            locals:       vec![std::collections::HashMap::new()],
            strings:      Vec::new(),
            str_ctr:      0,
            functions:    Vec::new(),
            ret_ptr:      None,
            fn_exit_label: None,
            defined_fns:  std::collections::HashSet::new(),
            enum_defs:       std::collections::HashMap::new(),
            defined_methods:  std::collections::HashSet::new(),
            method_ret_types: std::collections::HashMap::new(),
            loop_ctx:         Vec::new(),
            try_ctx:          Vec::new(),
            importing:        std::collections::HashSet::new(),
            imported:         std::collections::HashSet::new(),
            type_env:     vec![std::collections::HashMap::new()],
        }
    }

    // Returns the next unique SSA name: %t0, %t1, %t2, ...
    fn fresh(&mut self) -> String {
        let n = self.tmp;
        self.tmp += 1;
        format!("%t{}", n)
    }

    // Append one line of LLVM IR to the output buffer.
    fn emit(&mut self, line: &str) {
        self.out.push_str(line);
        self.out.push('\n');
    }

    // Record that variable `name` lives at alloca pointer `ptr` in the current scope.
    fn define_local(&mut self, name: &str, ptr: &str) {
        if let Some(scope) = self.locals.last_mut() {
            scope.insert(name.to_string(), ptr.to_string());
        }
    }

    // Search for variable `name` from innermost to outermost scope.
    fn lookup_local(&self, name: &str) -> Option<&str> {
        for scope in self.locals.iter().rev() {
            if let Some(ptr) = scope.get(name) {
                return Some(ptr.as_str());
            }
        }
        None
    }

    fn push_scope(&mut self) {
        self.locals.push(std::collections::HashMap::new());
        self.type_env.push(std::collections::HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.locals.pop();
        self.type_env.pop();
    }

    fn push_loop(&mut self, cont_lbl: &str, break_lbl: &str) {
        self.loop_ctx.push((cont_lbl.to_string(), break_lbl.to_string()));
    }
    fn pop_loop(&mut self) { self.loop_ctx.pop(); }

    // Record the static type of a newly-defined variable in the innermost scope.
    fn record_type(&mut self, name: &str, ty: StaticType) {
        if let Some(scope) = self.type_env.last_mut() {
            scope.insert(name.to_string(), ty);
        }
    }

    // Update the static type of an existing variable (for Assign statements).
    // Searches from innermost to outermost, updates the first match.
    fn update_type(&mut self, name: &str, ty: StaticType) {
        for scope in self.type_env.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), ty);
                return;
            }
        }
    }

    // Pure AST walk — infers the static type of an expression without emitting any IR.
    fn infer_type(&self, expr: &Expr) -> StaticType {
        match expr {
            Expr::Line(_, inner) => self.infer_type(inner),
            Expr::IntLit(_)      => StaticType::Int,
            Expr::FloatLit(_)    => StaticType::Float,
            Expr::BoolLit(_)     => StaticType::Bool,
            Expr::StrLit(_)      => StaticType::Str,
            Expr::Ident(name) => {
                for scope in self.type_env.iter().rev() {
                    if let Some(ty) = scope.get(name) { return ty.clone(); }
                }
                StaticType::Unknown
            }
            Expr::BinaryOp { left, op, right } => {
                let lt = self.infer_type(left);
                let rt = self.infer_type(right);
                match op {
                    Token::EqualsEquals | Token::BangEquals |
                    Token::Less | Token::LessEquals |
                    Token::Greater | Token::GreaterEquals |
                    Token::And | Token::Or => StaticType::Bool,
                    _ => if lt == rt && (lt == StaticType::Int || lt == StaticType::Float) { lt }
                         else { StaticType::Unknown },
                }
            }
            Expr::StructLit { name, .. } => StaticType::Struct(name.clone()),
            Expr::MethodCall { object, method, .. } => {
                if let StaticType::Struct(type_name) = self.infer_type(object) {
                    self.method_ret_types
                        .get(&(type_name, method.clone()))
                        .cloned()
                        .unwrap_or(StaticType::Unknown)
                } else { StaticType::Unknown }
            }
            Expr::Not(_) => StaticType::Bool,
            _ => StaticType::Unknown,
        }
    }

    // Extract the i64 payload from a known-Int NovaValue pointer.
    fn extract_int(&mut self, ptr: &str) -> String {
        let gep = self.fresh();
        let val = self.fresh();
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 1", gep, ptr));
        self.emit(&format!("    {} = load i64, ptr {}, align 8", val, gep));
        val
    }

    // Extract the double payload from a known-Float NovaValue pointer.
    fn extract_float(&mut self, ptr: &str) -> String {
        let gep = self.fresh();
        let raw = self.fresh();
        let val = self.fresh();
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 1", gep, ptr));
        self.emit(&format!("    {} = load i64, ptr {}, align 8", raw, gep));
        self.emit(&format!("    {} = bitcast i64 {} to double", val, raw));
        val
    }

    // Pack an i64 into an already-alloca'd NovaValue slot as tag=2 (Int).
    fn store_int(&mut self, slot: &str, val: &str) {
        let tgep = self.fresh();
        let pgep = self.fresh();
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 0", tgep, slot));
        self.emit(&format!("    store i64 2, ptr {}, align 8", tgep));
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 1", pgep, slot));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", val, pgep));
    }

    // Pack a double into an already-alloca'd NovaValue slot as tag=3 (Float).
    fn store_float(&mut self, slot: &str, val: &str) {
        let tgep  = self.fresh();
        let pgep  = self.fresh();
        let as_i64 = self.fresh();
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 0", tgep, slot));
        self.emit(&format!("    store i64 3, ptr {}, align 8", tgep));
        self.emit(&format!("    {} = bitcast double {} to i64", as_i64, val));
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 1", pgep, slot));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", as_i64, pgep));
    }

    // Pack an i64 (0 or 1) into an already-alloca'd NovaValue slot as tag=1 (Bool).
    fn store_bool(&mut self, slot: &str, val: &str) {
        let tgep = self.fresh();
        let pgep = self.fresh();
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 0", tgep, slot));
        self.emit(&format!("    store i64 1, ptr {}, align 8", tgep));
        self.emit(&format!("    {} = getelementptr %NovaValue, ptr {}, i64 0, i32 1", pgep, slot));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", val, pgep));
    }

    // Emit direct LLVM arithmetic for statically-known operand types.
    // Returns true if a fast path was emitted, false to fall back to the runtime call.
    fn try_emit_specialised_binop(
        &mut self,
        op: &Token,
        lt: &StaticType,
        rt: &StaticType,
        lptr: &str,
        rptr: &str,
        out: &str,
    ) -> bool {
        match (lt, rt) {
            (StaticType::Int, StaticType::Int) => {
                let lv = self.extract_int(lptr);
                let rv = self.extract_int(rptr);
                match op {
                    Token::Plus => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = add i64 {}, {}", r, lv, rv));
                        self.store_int(out, &r);
                    }
                    Token::Minus => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = sub i64 {}, {}", r, lv, rv));
                        self.store_int(out, &r);
                    }
                    Token::Star => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = mul i64 {}, {}", r, lv, rv));
                        self.store_int(out, &r);
                    }
                    Token::Slash => {
                        // Fall back to nova_div: it produces float (Nova design) and checks zero.
                        let _ = (lv, rv);
                        return false;
                    }
                    Token::Percent => {
                        // Fall back to nova_mod: it checks for division by zero.
                        let _ = (lv, rv);
                        return false;
                    }
                    Token::EqualsEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp eq i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::BangEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp ne i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::Less => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp slt i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::LessEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp sle i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::Greater => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp sgt i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::GreaterEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = icmp sge i64 {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    _ => return false,
                }
                true
            }
            (StaticType::Float, StaticType::Float) => {
                let lv = self.extract_float(lptr);
                let rv = self.extract_float(rptr);
                match op {
                    Token::Plus => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = fadd double {}, {}", r, lv, rv));
                        self.store_float(out, &r);
                    }
                    Token::Minus => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = fsub double {}, {}", r, lv, rv));
                        self.store_float(out, &r);
                    }
                    Token::Star => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = fmul double {}, {}", r, lv, rv));
                        self.store_float(out, &r);
                    }
                    Token::Slash => {
                        let r = self.fresh();
                        self.emit(&format!("    {} = fdiv double {}, {}", r, lv, rv));
                        self.store_float(out, &r);
                    }
                    Token::EqualsEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp oeq double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::BangEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp une double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::Less => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp olt double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::LessEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp ole double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::Greater => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp ogt double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    Token::GreaterEquals => {
                        let c = self.fresh(); let e = self.fresh();
                        self.emit(&format!("    {} = fcmp oge double {}, {}", c, lv, rv));
                        self.emit(&format!("    {} = zext i1 {} to i64", e, c));
                        self.store_bool(out, &e);
                    }
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    // Register a string literal as a global constant and return its name and byte length.
    // LLVM string globals: @str0 = private constant [6 x i8] c"hello\00"
    fn intern_string(&mut self, s: &str) -> (String, usize) {
        let name = format!("@str{}", self.str_ctr);
        self.str_ctr += 1;
        let len = s.len() + 1; // +1 for null terminator
        self.strings.push((name.clone(), s.to_string()));
        (name, len)
    }

    // Encode a Rust string as a valid LLVM c"..." byte sequence.
    // All non-printable bytes and special chars (\, ", \n, \t, etc.) become \XY hex escapes.
    fn escape_for_llvm(s: &str) -> String {
        let mut out = String::new();
        for byte in s.bytes() {
            match byte {
                b'\\' => out.push_str("\\5C"),
                b'"'  => out.push_str("\\22"),
                b'\n' => out.push_str("\\0A"),
                b'\r' => out.push_str("\\0D"),
                b'\t' => out.push_str("\\09"),
                b'\0' => out.push_str("\\00"),
                32..=126 => out.push(byte as char),
                _ => out.push_str(&format!("\\{:02X}", byte)),
            }
        }
        out
    }

    // Collect all Ident names referenced anywhere inside an expression tree.
    // Used by find_captures to determine what a lambda closes over.
    fn collect_idents(expr: &Expr, out: &mut std::collections::HashSet<String>) {
        match expr {
            Expr::Line(_, inner)       => Self::collect_idents(inner, out),
            Expr::Ident(n)             => { out.insert(n.clone()); }
            Expr::BinaryOp { left, right, .. } => {
                Self::collect_idents(left, out);
                Self::collect_idents(right, out);
            }
            Expr::Let { value, .. }    => Self::collect_idents(value, out),
            Expr::Assign { value, .. } => Self::collect_idents(value, out),
            Expr::Print(e)             => Self::collect_idents(e, out),
            Expr::Not(e)               => Self::collect_idents(e, out),
            Expr::Return(e)            => Self::collect_idents(e, out),
            Expr::If { condition, then_block, else_block } => {
                Self::collect_idents(condition, out);
                for s in then_block { Self::collect_idents(s, out); }
                if let Some(eb) = else_block { for s in eb { Self::collect_idents(s, out); } }
            }
            Expr::While { condition, body } => {
                Self::collect_idents(condition, out);
                for s in body { Self::collect_idents(s, out); }
            }
            Expr::Call { name, args } => {
                // The function name itself may be a local variable holding a closure — collect it
                // so find_captures can detect it as a free variable and include it in the env.
                out.insert(name.clone());
                for a in args { Self::collect_idents(a, out); }
            }
            Expr::DynCall { callee, args } => {
                Self::collect_idents(callee, out);
                for a in args { Self::collect_idents(a, out); }
            }
            Expr::Array(items) => { for i in items { Self::collect_idents(i, out); } }
            Expr::HashMap(pairs) => {
                for (k, v) in pairs {
                    Self::collect_idents(k, out);
                    Self::collect_idents(v, out);
                }
            }
            Expr::Index { object, index } => {
                Self::collect_idents(object, out);
                Self::collect_idents(index, out);
            }
            Expr::IndexAssign { index, value, .. } => {
                Self::collect_idents(index, out);
                Self::collect_idents(value, out);
            }
            Expr::Spawn(inner) => Self::collect_idents(inner, out),
            // Nested lambda: propagate references that escape the inner lambda's own params
            Expr::Lambda { params, body } => {
                let mut inner = std::collections::HashSet::new();
                for s in body { Self::collect_idents(s, &mut inner); }
                let ps: std::collections::HashSet<_> = params.iter().cloned().collect();
                for n in inner { if !ps.contains(&n) { out.insert(n); } }
            }
            _ => {}
        }
    }

    // Determine which outer-scope variables a lambda with the given params and body captures.
    // Returns names sorted for deterministic env slot ordering.
    fn find_captures(&self, params: &[String], body: &[Expr]) -> Vec<String> {
        let mut referenced = std::collections::HashSet::new();
        for stmt in body { Self::collect_idents(stmt, &mut referenced); }
        let param_set: std::collections::HashSet<_> = params.iter().cloned().collect();
        let mut caps: Vec<String> = referenced.into_iter()
            .filter(|n| !param_set.contains(n))
            .filter(|n| self.lookup_local(n).is_some())
            .collect();
        caps.sort();
        caps
    }

    // Emit an indirect closure call: pack arg_ptrs into a stack array, call nova_invoke_closure.
    fn emit_closure_call(&mut self, closure_ptr: &str, arg_ptrs: &[String], result: &str) {
        let n = arg_ptrs.len();
        if n == 0 {
            // nova_invoke_closure still needs a valid pointer for the args param even if nargs=0
            let dummy = self.fresh();
            self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
            self.emit(&format!("    call void @nova_invoke_closure(ptr {}, ptr {}, i64 0, ptr {})",
                closure_ptr, dummy, result));
        } else {
            // pack all arguments into a flat stack-allocated NovaValue array
            let args_array = self.fresh();
            self.emit(&format!("    {} = alloca [{} x %NovaValue], align 8", args_array, n));
            for (i, ap) in arg_ptrs.iter().enumerate() {
                let gep = self.fresh();
                self.emit(&format!(
                    "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                    gep, n, args_array, i
                ));
                self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", ap, gep));
            }
            self.emit(&format!("    call void @nova_invoke_closure(ptr {}, ptr {}, i64 {}, ptr {})",
                closure_ptr, args_array, n, result));
        }
    }

    // Compile a match arm body into `out`.
    // All items except the last are compiled as statements; the last is compiled as an expression
    // and its value is copied into `out`. Empty body copies nil.
    fn compile_arm_body(&mut self, body: &[Expr], out: &str) {
        let n = body.len();
        if n > 0 {
            for stmt in body.iter().take(n - 1) {
                self.compile_stmt(stmt);
            }
            let last_val = self.compile_expr(&body[n - 1]);
            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", last_val, out));
        } else {
            let nil = self.fresh();
            self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
            self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", nil, out));
        }
    }

    // Compile `spawn expr`: wraps `inner` in a synthetic zero-argument closure (capturing all
    // referenced outer variables), then calls nova_spawn to run it on a new OS thread.
    // Returns a TAG_TASK value that can be passed to wait() to retrieve the result.
    fn compile_spawn(&mut self, inner: &Expr) -> String {
        let captures = self.find_captures(&[], std::slice::from_ref(inner));
        let wrap_idx = self.tmp;
        self.tmp += 1;
        let wrap_name = format!("spawn_wrap{}", wrap_idx);

        // Save context and enter the wrapper function's scope
        let saved_out      = std::mem::take(&mut self.out);
        let saved_locals   = std::mem::take(&mut self.locals);
        let saved_type_env = std::mem::take(&mut self.type_env);
        let saved_loop_ctx = std::mem::take(&mut self.loop_ctx);
        let saved_try_ctx  = std::mem::take(&mut self.try_ctx);
        let saved_ret_ptr  = self.ret_ptr.take();
        let saved_fn_exit  = self.fn_exit_label.take();

        self.locals   = vec![std::collections::HashMap::new()];
        self.type_env = vec![std::collections::HashMap::new()];
        self.loop_ctx = Vec::new();
        self.try_ctx  = Vec::new();
        let ret_ptr_name = "%_ret".to_string();
        let fn_exit_lbl  = format!("sw_exit{}", self.tmp);
        self.tmp += 1;
        self.ret_ptr       = Some(ret_ptr_name.clone());
        self.fn_exit_label = Some(fn_exit_lbl.clone());

        self.emit(&format!(
            "define void @{}(ptr %env, ptr %args, ptr {}) {{",
            wrap_name, ret_ptr_name
        ));
        self.emit("entry:");

        // Load each captured variable from the env array
        for (i, cap) in captures.iter().enumerate() {
            let gep   = self.fresh();
            let local = format!("%local_{}", cap);
            self.emit(&format!(
                "    {} = getelementptr %NovaValue, ptr %env, i64 {}", gep, i
            ));
            self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
            self.define_local(cap, &local);
            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", gep, local));
        }

        // Compile the inner expression and copy its value to the return slot
        let inner_val = self.compile_expr(inner);
        self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", inner_val, ret_ptr_name));

        self.emit(&format!("    br label %{}", fn_exit_lbl));
        self.emit(&format!("{}:", fn_exit_lbl));
        self.emit("    ret void");
        self.emit("}");
        self.emit("");

        let fn_def = std::mem::take(&mut self.out);
        self.functions.push(fn_def);

        // Restore outer context
        self.out           = saved_out;
        self.locals        = saved_locals;
        self.type_env      = saved_type_env;
        self.loop_ctx      = saved_loop_ctx;
        self.try_ctx       = saved_try_ctx;
        self.ret_ptr       = saved_ret_ptr;
        self.fn_exit_label = saved_fn_exit;

        // Build the env array from captured values in the restored outer scope
        let env_size = captures.len();
        let closure_slot = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", closure_slot));

        if env_size == 0 {
            let dummy = self.fresh();
            self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
            self.emit(&format!(
                "    call void @nova_make_closure(ptr @{}, ptr {}, i64 0, ptr {})",
                wrap_name, dummy, closure_slot
            ));
        } else {
            let mut cap_ptrs: Vec<String> = Vec::new();
            for cap in &captures {
                let ptr = self.lookup_local(cap).map(|s| s.to_string()).unwrap_or_default();
                cap_ptrs.push(ptr);
            }
            let env_arr = self.fresh();
            self.emit(&format!("    {} = alloca [{} x %NovaValue], align 8", env_arr, env_size));
            for (i, cp) in cap_ptrs.iter().enumerate() {
                let slot = self.fresh();
                self.emit(&format!(
                    "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                    slot, env_size, env_arr, i
                ));
                self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", cp, slot));
            }
            self.emit(&format!(
                "    call void @nova_make_closure(ptr @{}, ptr {}, i64 {}, ptr {})",
                wrap_name, env_arr, env_size, closure_slot
            ));
        }

        let result = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
        self.emit(&format!("    call void @nova_spawn(ptr {}, ptr {})", closure_slot, result));
        result
    }

    // Compile a statement (result value is discarded).
    fn compile_stmt(&mut self, expr: &Expr) {
        match expr {
            Expr::Line(_, inner) => self.compile_stmt(inner),

            // `let name = value`
            // Allocate a NovaValue slot on the stack, compile the RHS, copy into the slot.
            // The alloca name includes a counter suffix so two `let x = ...` in different
            // scopes of the same function get distinct LLVM SSA names.
            Expr::Let { name, value, .. } => {
                let ty = self.infer_type(value);
                // Compile the RHS first so that `let arr = sort(arr)` reads the OLD arr,
                // not a freshly-created empty slot.
                let src = self.compile_expr(value);
                if let Some(existing) = self.lookup_local(name).map(|s| s.to_string()) {
                    // Name already in scope — reuse the existing alloca so that rebindings
                    // like `let arr = push(arr, x)` inside a loop correctly chain.
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", src, existing));
                    self.update_type(name, ty);
                } else {
                    let uid = self.tmp; self.tmp += 1;
                    let ptr = format!("%local_{}_{}", name, uid);
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                    self.define_local(name, &ptr);
                    self.record_type(name, ty);
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", src, ptr));
                }
            }

            // `name = value` — write a new value into an existing variable's slot
            Expr::Assign { name, value } => {
                let ty = self.infer_type(value);
                let src = self.compile_expr(value);
                if let Some(ptr) = self.lookup_local(name).map(|s| s.to_string()) {
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", src, ptr));
                    self.update_type(name, ty);
                }
            }

            Expr::Print(inner) => {
                let ptr = self.compile_expr(inner);
                self.emit(&format!("    call void @nova_print(ptr {})", ptr));
            }

            Expr::Printn(inner) => {
                let ptr = self.compile_expr(inner);
                let out = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", out));
                self.emit(&format!("    call void @nova_printn(ptr {}, ptr {})", ptr, out));
            }

            Expr::If { condition, then_block, else_block } => {
                self.compile_if(condition, then_block, else_block.as_deref());
            }

            Expr::While { condition, body } => {
                self.compile_while(condition, body);
            }

            // for var in 0..10  OR  for var in array
            Expr::For { var, iter, body } => {
                match iter.as_ref() {
                    Expr::Range { start, end } => {
                        self.compile_for_range(var, start, end, body);
                    }
                    other => {
                        // clone to avoid borrow conflict (compile_for_array takes &mut self)
                        let arr_expr = other.clone();
                        self.compile_for_array(var, &arr_expr, body);
                    }
                }
            }

            Expr::ForEnumerate { index_var, item_var, iter, body } => {
                let arr_expr = iter.as_ref().clone();
                self.compile_for_enumerate(index_var, item_var, &arr_expr, body);
            }

            Expr::ForDestructure { vars, iter, body } => {
                let arr_expr = iter.as_ref().clone();
                let vars_clone = vars.clone();
                self.compile_for_destructure(&vars_clone, &arr_expr, body);
            }

            // break — jump to the innermost loop's end label, then continue in a dead block
            Expr::Break => {
                if let Some((_, break_lbl)) = self.loop_ctx.last().cloned() {
                    self.emit(&format!("    br label %{}", break_lbl));
                    let dead = format!("dead{}", self.tmp);
                    self.tmp += 1;
                    self.emit(&format!("{}:", dead));
                }
            }

            // continue — jump to the innermost loop's continue label, then continue in a dead block
            Expr::Continue => {
                if let Some((cont_lbl, _)) = self.loop_ctx.last().cloned() {
                    self.emit(&format!("    br label %{}", cont_lbl));
                    let dead = format!("dead{}", self.tmp);
                    self.tmp += 1;
                    self.emit(&format!("{}:", dead));
                }
            }

            // import "path.nova" — read, parse, prescan, and inline the file's statements.
            // Circular imports are detected and produce a compile-time error.
            Expr::Import(path) => {
                let canonical = std::fs::canonicalize(path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.clone());
                if self.imported.contains(&canonical) { return; }  // already done
                if self.importing.contains(&canonical) {
                    eprintln!("error: circular import detected: '{}'", path);
                    std::process::exit(1);
                }
                let source = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(_) => {
                        eprintln!("error: cannot import '{}': file not found", path);
                        std::process::exit(1);
                    }
                };
                self.importing.insert(canonical.clone());
                let mut lex = Lexer::new(&source);
                let tokens = lex.tokenize();
                let mut parser = Parser::new(tokens);
                let mut stmts: Vec<Expr> = Vec::new();
                while !matches!(parser.current_token(), Token::EOF) {
                    stmts.push(parser.parse_statement());
                    parser.skip_optional_semicolon();
                }
                self.prescan_stmts(&stmts);
                for stmt in stmts { self.compile_stmt(&stmt); }
                self.importing.remove(&canonical);
                self.imported.insert(canonical);
            }

            // try { body } catch name { handler }
            // Compiles each body stmt; after every stmt checks nova_is_thrown().
            // Throw inside the body jumps directly to catch_lbl (via try_ctx).
            // Throw propagated from a called function is caught by the per-stmt flag check.
            Expr::Try { body, catch_var, catch_body } => {
                let try_id    = self.tmp; self.tmp += 1;
                let catch_lbl = format!("catch{}", try_id);
                let end_lbl   = format!("try_end{}", try_id);

                let catch_ptr = format!("%catch_var_{}", try_id);
                self.emit(&format!("    {} = alloca %NovaValue, align 8", catch_ptr));

                self.try_ctx.push(catch_lbl.clone());
                for (i, stmt) in body.iter().enumerate() {
                    self.compile_stmt(stmt);
                    // check for a throw that propagated up from a called function
                    let raw   = self.fresh();
                    let cond  = self.fresh();
                    let next  = format!("try_next{}_{}", i, try_id);
                    self.emit(&format!("    {} = call i32 @nova_is_thrown()", raw));
                    self.emit(&format!("    {} = icmp ne i32 {}, 0", cond, raw));
                    self.emit(&format!("    br i1 {}, label %{}, label %{}", cond, catch_lbl, next));
                    self.emit(&format!("{}:", next));
                }
                self.try_ctx.pop();
                self.emit(&format!("    br label %{}", end_lbl));

                self.emit(&format!("{}:", catch_lbl));
                self.emit(&format!("    call void @nova_get_thrown(ptr {})", catch_ptr));
                self.push_scope();
                self.define_local(catch_var, &catch_ptr);
                for stmt in catch_body { self.compile_stmt(stmt); }
                self.pop_scope();
                self.emit(&format!("    br label %{}", end_lbl));

                self.emit(&format!("{}:", end_lbl));
            }

            // `fn name(params) { body }` — compile to a separate LLVM function definition
            // and store it in self.functions; no IR is emitted into the current body
            Expr::Fn { name, params, body, .. } => {
                // Register the name NOW so the function body can call itself recursively
                self.defined_fns.insert(name.clone());

                // Extract just the param names (ignore type annotations and defaults for now)
                let param_names: Vec<String> = params.iter().map(|(n, _, _)| n.clone()).collect();

                // Save the current compilation context (we're nesting into a new function)
                let saved_out       = std::mem::take(&mut self.out);
                let saved_locals    = std::mem::take(&mut self.locals);
                let saved_type_env  = std::mem::take(&mut self.type_env);
                let saved_loop_ctx  = std::mem::take(&mut self.loop_ctx);
                let saved_try_ctx   = std::mem::take(&mut self.try_ctx);
                let saved_ret_ptr   = self.ret_ptr.take();
                let saved_fn_exit   = self.fn_exit_label.take();

                // Set up function context
                self.locals   = vec![std::collections::HashMap::new()];
                self.type_env = vec![std::collections::HashMap::new()];
                self.loop_ctx = Vec::new();
                self.try_ctx  = Vec::new();
                let ret_ptr_name = "%_ret".to_string();
                let fn_exit_lbl  = format!("fn_exit{}", self.tmp);
                self.tmp += 1;
                self.ret_ptr       = Some(ret_ptr_name.clone());
                self.fn_exit_label = Some(fn_exit_lbl.clone());

                // Build the LLVM parameter list: one ptr per param + one ptr for the return value
                // Calling convention: caller allocates the result slot and passes its address last
                let mut sig: Vec<String> = param_names.iter()
                    .map(|p| format!("ptr %arg_{}", p))
                    .collect();
                sig.push(format!("ptr {}", ret_ptr_name));

                self.emit(&format!("define void @nova_fn_{}({}) {{", name, sig.join(", ")));
                self.emit("entry:");

                // Copy each argument into a local alloca slot so params are independently mutable
                for p in &param_names {
                    let local = format!("%local_{}", p);
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
                    self.define_local(p, &local);
                    self.emit(&format!("    call void @nova_copy(ptr %arg_{}, ptr {})", p, local));
                }

                // Compile body:
                // All statements except the last are compiled as statements.
                // The last is compiled as an expression — its value is the implicit return.
                // (Explicit `return` stmts in the body branch directly to fn_exit via compile_stmt.)
                let n = body.len();
                if n > 0 {
                    for stmt in body.iter().take(n - 1) {
                        self.compile_stmt(stmt);
                    }
                    let last_val = self.compile_expr(&body[n - 1]);
                    self.emit(&format!(
                        "    call void @nova_copy(ptr {}, ptr {})", last_val, ret_ptr_name
                    ));
                } else {
                    // empty body — return nil
                    let nil = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
                    self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", nil, ret_ptr_name));
                }

                // Branch to the unified exit block (all paths — implicit and explicit — land here)
                self.emit(&format!("    br label %{}", fn_exit_lbl));
                self.emit(&format!("{}:", fn_exit_lbl));
                self.emit("    ret void");
                self.emit("}");
                self.emit(""); // blank line between function definitions

                // Save the completed function definition and restore the outer context
                let fn_def = std::mem::take(&mut self.out);
                self.functions.push(fn_def);

                self.out           = saved_out;
                self.locals        = saved_locals;
                self.type_env      = saved_type_env;
                self.loop_ctx      = saved_loop_ctx;
                self.try_ctx       = saved_try_ctx;
                self.ret_ptr       = saved_ret_ptr;
                self.fn_exit_label = saved_fn_exit;
            }

            // `impl TypeName { fn method(self, ...) { body } }`
            // Each method is compiled as @nova_method_{TypeName}_{method_name} with the same
            // calling convention as regular functions: (ptr self, ptr arg1, ..., ptr ret_slot)
            Expr::ImplBlock { type_name, methods } => {
                for method in methods {
                    let mfunc = match method {
                        Expr::Line(_, inner) => inner.as_ref(),
                        other => other,
                    };
                    if let Expr::Fn { name, params, body, .. } = mfunc {
                        let fn_llvm_name = format!("nova_method_{}_{}", type_name, name);
                        self.defined_fns.insert(fn_llvm_name.clone());
                        self.defined_methods.insert((type_name.clone(), name.clone()));

                        let param_names: Vec<String> = params.iter().map(|(n, _, _)| n.clone()).collect();

                        let saved_out      = std::mem::take(&mut self.out);
                        let saved_locals   = std::mem::take(&mut self.locals);
                        let saved_type_env = std::mem::take(&mut self.type_env);
                        let saved_loop_ctx = std::mem::take(&mut self.loop_ctx);
                        let saved_try_ctx  = std::mem::take(&mut self.try_ctx);
                        let saved_ret_ptr  = self.ret_ptr.take();
                        let saved_fn_exit  = self.fn_exit_label.take();

                        self.locals   = vec![std::collections::HashMap::new()];
                        self.type_env = vec![std::collections::HashMap::new()];
                        self.loop_ctx = Vec::new();
                        self.try_ctx  = Vec::new();
                        let ret_ptr_name = "%_ret".to_string();
                        let fn_exit_lbl  = format!("fn_exit{}", self.tmp);
                        self.tmp += 1;
                        self.ret_ptr       = Some(ret_ptr_name.clone());
                        self.fn_exit_label = Some(fn_exit_lbl.clone());

                        let mut sig: Vec<String> = param_names.iter()
                            .map(|p| format!("ptr %arg_{}", p))
                            .collect();
                        sig.push(format!("ptr {}", ret_ptr_name));

                        self.emit(&format!("define void @{}({}) {{", fn_llvm_name, sig.join(", ")));
                        self.emit("entry:");

                        for p in &param_names {
                            let local = format!("%local_{}", p);
                            self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
                            self.define_local(p, &local);
                            self.emit(&format!("    call void @nova_copy(ptr %arg_{}, ptr {})", p, local));
                        }

                        let n = body.len();
                        if n > 0 {
                            for stmt in body.iter().take(n - 1) {
                                self.compile_stmt(stmt);
                            }
                            let last_val = self.compile_expr(&body[n - 1]);
                            self.emit(&format!(
                                "    call void @nova_copy(ptr {}, ptr {})", last_val, ret_ptr_name
                            ));
                        } else {
                            let nil = self.fresh();
                            self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
                            self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
                            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", nil, ret_ptr_name));
                        }

                        self.emit(&format!("    br label %{}", fn_exit_lbl));
                        self.emit(&format!("{}:", fn_exit_lbl));
                        self.emit("    ret void");
                        self.emit("}");
                        self.emit("");

                        let fn_def = std::mem::take(&mut self.out);
                        self.functions.push(fn_def);

                        self.out           = saved_out;
                        self.locals        = saved_locals;
                        self.type_env      = saved_type_env;
                        self.loop_ctx      = saved_loop_ctx;
                        self.try_ctx       = saved_try_ctx;
                        self.ret_ptr       = saved_ret_ptr;
                        self.fn_exit_label = saved_fn_exit;
                    }
                }
            }

            // `return expr` — copy the value to the function's return slot and jump to its exit
            // After the branch we emit a "dead" block label so any subsequent instructions are
            // in a valid (but unreachable) basic block — LLVM requires every instruction to live
            // inside a block, and a block can only end with one terminator.
            Expr::Return(val) => {
                if let (Some(ret), Some(exit)) =
                    (self.ret_ptr.clone(), self.fn_exit_label.clone())
                {
                    let v = self.compile_expr(val);
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", v, ret));
                    self.emit(&format!("    br label %{}", exit));
                    // dead block absorbs any code that follows this return (e.g. dead stmts)
                    let dead = format!("dead{}", self.tmp);
                    self.tmp += 1;
                    self.emit(&format!("{}:", dead));
                }
                // return at top level (outside any fn) is a no-op
            }

            // struct.field = val — mutate a struct field in place via nova_index_set
            // Structs are backed by TAG_MAP so field access is string-keyed map access.
            Expr::FieldAssign { object, field, value } => {
                let obj_ptr = self.compile_expr(object);
                let (gname, _) = self.intern_string(field);
                let kptr = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", kptr));
                self.emit(&format!("    call void @nova_make_str(ptr {}, ptr {})", gname, kptr));
                let val_ptr = self.compile_expr(value);
                self.emit(&format!("    call void @nova_index_set(ptr {}, ptr {}, ptr {})", obj_ptr, kptr, val_ptr));
            }

            // arr[i] = val or map["key"] = val — mutate in place via nova_index_set
            Expr::IndexAssign { name, index, value } => {
                if let Some(ptr) = self.lookup_local(name).map(|s| s.to_string()) {
                    let idx_ptr = self.compile_expr(index);
                    let val_ptr = self.compile_expr(value);
                    self.emit(&format!("    call void @nova_index_set(ptr {}, ptr {}, ptr {})", ptr, idx_ptr, val_ptr));
                }
            }

            // any other expression used as a statement — compile and discard the result
            other => { self.compile_expr(other); }
        }
    }

    // Compile an expression and return the SSA name of a pointer to a stack-allocated NovaValue.
    fn compile_expr(&mut self, expr: &Expr) -> String {
        match expr {
            Expr::Line(_, inner) => self.compile_expr(inner),

            // Integer literal — tag=2, payload=n
            Expr::IntLit(n) => {
                let ptr = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", n, ptr));
                ptr
            }

            // Float literal — tag=3, payload=bit-cast of f64
            // LLVM requires a decimal point in double constants (e.g. 16.0, not 16).
            Expr::FloatLit(f) => {
                let ptr = self.fresh();
                let mut fs = format!("{}", f);
                if !fs.contains('.') && !fs.contains('e') { fs.push_str(".0"); }
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_float(double {}, ptr {})", fs, ptr));
                ptr
            }

            // Bool literal — tag=1, payload=1 or 0
            Expr::BoolLit(b) => {
                let ptr = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_bool(i64 {}, ptr {})", if *b { 1 } else { 0 }, ptr));
                ptr
            }

            // nil — tag=0, payload=0
            Expr::NilLit => {
                let ptr = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_nil(ptr {})", ptr));
                ptr
            }

            // String literal — tag=4, payload=pointer to null-terminated char array
            // The char data lives in a global constant; we pass its address to nova_make_str.
            Expr::StrLit(s) => {
                let ptr = self.fresh();
                let (gname, _) = self.intern_string(s);
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_str(ptr {}, ptr {})", gname, ptr));
                ptr
            }

            // Interpolated string: "hello {name}, you have {n} items"
            // Each part is compiled to a NovaValue slot; all slots are passed to nova_str_build.
            Expr::StrInterp(parts) => {
                let n = parts.len();
                let arr = self.fresh();
                self.emit(&format!("    {} = alloca [{} x %NovaValue], align 8", arr, n));
                for (i, part) in parts.iter().enumerate() {
                    let slot = self.fresh();
                    self.emit(&format!(
                        "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                        slot, n, arr, i
                    ));
                    match part {
                        StringPart::Literal(s) => {
                            let (gname, _) = self.intern_string(s);
                            self.emit(&format!("    call void @nova_make_str(ptr {}, ptr {})", gname, slot));
                        }
                        StringPart::Interp(expr_text) => {
                            if expr_text.is_empty() {
                                self.emit(&format!("    call void @nova_make_nil(ptr {})", slot));
                            } else {
                                // Re-parse so complex expressions like {len(arr)} compile
                                // correctly, not just bare variable names.
                                let mut lex = crate::lexer::Lexer::new(expr_text);
                                let tokens = lex.tokenize();
                                let mut parser = crate::parser::Parser::new(tokens);
                                let expr = parser.parse_null_coalesce();
                                let vptr = self.compile_expr(&expr);
                                self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", vptr, slot));
                            }
                        }
                    }
                }
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_str_build(ptr {}, i64 {}, ptr {})", arr, n, result));
                result
            }

            // Logical not: !expr
            Expr::Not(inner) => {
                let v = self.compile_expr(inner);
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_not(ptr {}, ptr {})", v, result));
                result
            }

            // Variable reference — return the alloca pointer directly (no copy needed for reads)
            Expr::Ident(name) => {
                if let Some(local_ptr) = self.lookup_local(name).map(|s| s.to_string()) {
                    local_ptr
                } else {
                    // undeclared variable — produce nil rather than crashing the codegen
                    let ptr = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                    self.emit(&format!("    call void @nova_make_nil(ptr {})", ptr));
                    ptr
                }
            }

            // Binary operation — fast path if both operand types are statically known,
            // otherwise fall through to the runtime function (handles boxing, type coercion, etc.)
            Expr::BinaryOp { left, op, right } => {
                let lt = self.infer_type(left);
                let rt = self.infer_type(right);
                let lptr = self.compile_expr(left);
                let rptr = self.compile_expr(right);
                let out  = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", out));
                if !self.try_emit_specialised_binop(op, &lt, &rt, &lptr, &rptr, &out) {
                    let rt_fn = match op {
                        Token::Plus          => "nova_add",
                        Token::Minus         => "nova_sub",
                        Token::Star          => "nova_mul",
                        Token::Slash         => "nova_div",
                        Token::Percent       => "nova_mod",
                        Token::EqualsEquals  => "nova_eq",
                        Token::BangEquals    => "nova_neq",
                        Token::Less          => "nova_lt",
                        Token::LessEquals    => "nova_lte",
                        Token::Greater       => "nova_gt",
                        Token::GreaterEquals => "nova_gte",
                        Token::And           => "nova_and",
                        Token::Or            => "nova_or",
                        _ => "nova_add",
                    };
                    self.emit(&format!("    call void @{}(ptr {}, ptr {}, ptr {})", rt_fn, lptr, rptr, out));
                }
                out
            }

            // if-else as an expression (produces a value)
            Expr::If { condition, then_block, else_block } => {
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.compile_if_expr(condition, then_block, else_block.as_deref(), &result);
                result
            }

            // Array literal: [1, 2, 3]
            // Create an empty array then append each element in place.
            Expr::Array(items) => {
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_make_array(ptr {})", result));
                for item in items {
                    let elem = self.compile_expr(item);
                    self.emit(&format!("    call void @nova_array_append(ptr {}, ptr {})", result, elem));
                }
                result
            }

            // Hashmap literal: {"key": val, ...}
            // Create an empty map then insert each key/value pair in place.
            Expr::HashMap(pairs) => {
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_make_map(ptr {})", result));
                for (key, val) in pairs {
                    let kptr = self.compile_expr(key);
                    let vptr = self.compile_expr(val);
                    self.emit(&format!("    call void @nova_map_insert(ptr {}, ptr {}, ptr {})", result, kptr, vptr));
                }
                result
            }

            // Index read: arr[i] or map["key"]
            // nova_index_get dispatches at runtime based on the value's tag.
            Expr::Index { object, index } => {
                let obj_ptr = self.compile_expr(object);
                let idx_ptr = self.compile_expr(index);
                let result  = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_index_get(ptr {}, ptr {}, ptr {})", obj_ptr, idx_ptr, result));
                result
            }

            // Named function call — handles builtins, user-defined functions, and local closures.
            // Order: builtins first, then user-defined fns, then local-variable closures, then nil.
            Expr::Call { name, args } => {
                let arg_ptrs: Vec<String> = args.iter()
                    .map(|a| self.compile_expr(a))
                    .collect();

                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));

                // Check for a local variable with this name before the match so we can use it
                // in the guard without holding a borrow of self inside the match arm.
                let local_closure = self.lookup_local(name).map(|s| s.to_string());

                match name.as_str() {
                    // len(arr/map/str) → int
                    "len" if arg_ptrs.len() == 1 => {
                        self.emit(&format!("    call void @nova_len(ptr {}, ptr {})", arg_ptrs[0], result));
                    }
                    // push(arr, elem) → new array with elem appended
                    "push" if arg_ptrs.len() == 2 => {
                        self.emit(&format!("    call void @nova_array_push(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result));
                    }
                    // wait(task) → block until the spawned thread finishes, return its result
                    "wait" if arg_ptrs.len() == 1 => {
                        self.emit(&format!("    call void @nova_wait(ptr {}, ptr {})", arg_ptrs[0], result));
                    }
                    // chan() → create an empty rendezvous channel
                    "chan" if arg_ptrs.len() == 0 => {
                        self.emit(&format!("    call void @nova_make_chan(ptr {})", result));
                    }
                    // send(ch, val) → send a value into a channel (blocking)
                    "send" if arg_ptrs.len() == 2 => {
                        self.emit(&format!("    call void @nova_send(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result));
                    }
                    // recv(ch) → receive a value from a channel (blocking)
                    "recv" if arg_ptrs.len() == 1 => {
                        self.emit(&format!("    call void @nova_recv(ptr {}, ptr {})", arg_ptrs[0], result));
                    }
                    // clock() → int milliseconds since Unix epoch
                    "clock" if arg_ptrs.is_empty() => {
                        self.emit(&format!("    call void @nova_clock(ptr {})", result));
                    }
                    // make_array(n, default) → pre-sized array filled with default
                    "make_array" if arg_ptrs.len() == 2 => {
                        self.emit(&format!("    call void @nova_make_array_n(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result));
                    }

                    // ── Type conversion ─────────────────────────────────────
                    "str"   if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_to_str  (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "int"   if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_to_int  (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "float" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_to_float(ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "type"  if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_type_of (ptr {}, ptr {})", arg_ptrs[0], result)); }

                    // ── Math ────────────────────────────────────────────────
                    "abs"   if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_abs  (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "sqrt"  if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_sqrt (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "floor" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_floor(ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "ceil"  if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_ceil (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "round" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_round(ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "min"   if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_min  (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "max"   if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_max  (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }

                    // ── String ──────────────────────────────────────────────
                    "upper"       if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_upper      (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "lower"       if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_lower      (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "trim"        if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_trim       (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "contains"    if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_contains   (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "starts_with" if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_starts_with(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "ends_with"   if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_ends_with  (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "replace"     if arg_ptrs.len() == 3 => { self.emit(&format!("    call void @nova_replace    (ptr {}, ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], arg_ptrs[2], result)); }
                    "split"       if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_split      (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "join"        if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_join       (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }

                    // ── Array ───────────────────────────────────────────────
                    "sort"    if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_sort   (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "reverse" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_reverse(ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "pop"     if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_pop    (ptr {}, ptr {})", arg_ptrs[0], result)); }

                    // ── Array HOFs ──────────────────────────────────────────
                    "map"    if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_hof_map   (ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "filter" if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_hof_filter(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "sum"    if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_sum        (ptr {}, ptr {})",         arg_ptrs[0], result)); }

                    // ── Map ─────────────────────────────────────────────────
                    "keys"   if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_keys   (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "values" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_values (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "setKey" if arg_ptrs.len() == 3 => { self.emit(&format!("    call void @nova_set_key(ptr {}, ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], arg_ptrs[2], result)); }

                    // ── Char ────────────────────────────────────────────────
                    "ord" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_ord(ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "chr" if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_chr(ptr {}, ptr {})", arg_ptrs[0], result)); }

                    // ── I/O ─────────────────────────────────────────────────
                    "println"   if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_println   (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "printn"    if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_printn    (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "readFile"  if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_read_file (ptr {}, ptr {})", arg_ptrs[0], result)); }
                    "writeFile" if arg_ptrs.len() == 2 => { self.emit(&format!("    call void @nova_write_file(ptr {}, ptr {}, ptr {})", arg_ptrs[0], arg_ptrs[1], result)); }
                    "input"     if arg_ptrs.len() == 1 => { self.emit(&format!("    call void @nova_input     (ptr {}, ptr {})", arg_ptrs[0], result)); }

                    _ if self.defined_fns.contains(name) => {
                        // User-defined function: pass all value ptrs + result ptr as last arg
                        let mut call_args: Vec<String> = arg_ptrs.iter()
                            .map(|p| format!("ptr {}", p))
                            .collect();
                        call_args.push(format!("ptr {}", result));
                        self.emit(&format!(
                            "    call void @nova_fn_{}({})",
                            name,
                            call_args.join(", ")
                        ));
                    }
                    _ if local_closure.is_some() => {
                        // Local variable holding a closure — dispatch through nova_invoke_closure
                        let closure_ptr = local_closure.unwrap();
                        self.emit_closure_call(&closure_ptr, &arg_ptrs, &result);
                    }
                    _ => {
                        eprintln!("error: unknown function '{}' in LLVM backend", name);
                        std::process::exit(1);
                    }
                }
                result
            }

            // Anonymous function: (params) -> body or (params) -> { block }
            // Compiles the body to a named LLVM function (@lambdaN) and wraps it with
            // nova_make_closure so the value can be stored, passed, and called later.
            Expr::Lambda { params, body } => {
                // Determine which outer variables this lambda captures
                let captures = self.find_captures(params, body);
                let lambda_idx = self.tmp;
                self.tmp += 1;
                let lambda_name = format!("lambda{}", lambda_idx);

                // Save the current compilation context before entering the lambda body
                let saved_out      = std::mem::take(&mut self.out);
                let saved_locals   = std::mem::take(&mut self.locals);
                let saved_type_env = std::mem::take(&mut self.type_env);
                let saved_loop_ctx = std::mem::take(&mut self.loop_ctx);
                let saved_try_ctx  = std::mem::take(&mut self.try_ctx);
                let saved_ret_ptr  = self.ret_ptr.take();
                let saved_fn_exit  = self.fn_exit_label.take();

                self.locals   = vec![std::collections::HashMap::new()];
                self.type_env = vec![std::collections::HashMap::new()];
                self.loop_ctx = Vec::new();
                self.try_ctx  = Vec::new();
                let ret_ptr_name = "%_ret".to_string();
                let fn_exit_lbl  = format!("lam_exit{}", self.tmp);
                self.tmp += 1;
                self.ret_ptr       = Some(ret_ptr_name.clone());
                self.fn_exit_label = Some(fn_exit_lbl.clone());

                // All lambdas share the same dispatch signature regardless of arity:
                //   void @lambdaN(ptr %env, ptr %args, ptr %_ret)
                // Captures arrive via %env, positional params via %args.
                self.emit(&format!(
                    "define void @{}(ptr %env, ptr %args, ptr {}) {{",
                    lambda_name, ret_ptr_name
                ));
                self.emit("entry:");

                // Load each captured variable from its slot in the env array
                for (i, cap) in captures.iter().enumerate() {
                    let gep   = self.fresh();
                    let local = format!("%local_{}", cap);
                    self.emit(&format!(
                        "    {} = getelementptr %NovaValue, ptr %env, i64 {}",
                        gep, i
                    ));
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
                    self.define_local(cap, &local);
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", gep, local));
                }

                // Load each parameter from its slot in the args array
                for (i, param) in params.iter().enumerate() {
                    let gep   = self.fresh();
                    let local = format!("%local_{}", param);
                    self.emit(&format!(
                        "    {} = getelementptr %NovaValue, ptr %args, i64 {}",
                        gep, i
                    ));
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
                    self.define_local(param, &local);
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", gep, local));
                }

                // Compile body — same pattern as regular functions
                let n = body.len();
                if n > 0 {
                    for stmt in body.iter().take(n - 1) {
                        self.compile_stmt(stmt);
                    }
                    let last_val = self.compile_expr(&body[n - 1]);
                    self.emit(&format!(
                        "    call void @nova_copy(ptr {}, ptr {})", last_val, ret_ptr_name
                    ));
                } else {
                    let nil = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
                    self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
                    self.emit(&format!(
                        "    call void @nova_copy(ptr {}, ptr {})", nil, ret_ptr_name
                    ));
                }

                self.emit(&format!("    br label %{}", fn_exit_lbl));
                self.emit(&format!("{}:", fn_exit_lbl));
                self.emit("    ret void");
                self.emit("}");
                self.emit("");

                let fn_def = std::mem::take(&mut self.out);
                self.functions.push(fn_def);

                // Restore outer context
                self.out           = saved_out;
                self.locals        = saved_locals;
                self.type_env      = saved_type_env;
                self.loop_ctx      = saved_loop_ctx;
                self.try_ctx       = saved_try_ctx;
                self.ret_ptr       = saved_ret_ptr;
                self.fn_exit_label = saved_fn_exit;

                // At the call site: build the env array and call nova_make_closure
                let env_size = captures.len();
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));

                if env_size == 0 {
                    // No captures — pass a dummy slot; nova_make_closure won't dereference it
                    let dummy = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
                    self.emit(&format!(
                        "    call void @nova_make_closure(ptr @{}, ptr {}, i64 0, ptr {})",
                        lambda_name, dummy, result
                    ));
                } else {
                    // Collect capture pointers from the restored outer scope
                    let mut cap_ptrs: Vec<String> = Vec::new();
                    for cap in &captures {
                        let ptr = self.lookup_local(cap).map(|s| s.to_string()).unwrap_or_default();
                        cap_ptrs.push(ptr);
                    }

                    // Pack captured values into a stack-allocated env array
                    let env_arr = self.fresh();
                    self.emit(&format!(
                        "    {} = alloca [{} x %NovaValue], align 8", env_arr, env_size
                    ));
                    for (i, cp) in cap_ptrs.iter().enumerate() {
                        let slot = self.fresh();
                        self.emit(&format!(
                            "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                            slot, env_size, env_arr, i
                        ));
                        self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", cp, slot));
                    }
                    self.emit(&format!(
                        "    call void @nova_make_closure(ptr @{}, ptr {}, i64 {}, ptr {})",
                        lambda_name, env_arr, env_size, result
                    ));
                }

                result
            }

            // Dynamic call: expr(args) — may be a closure call or an enum constructor call.
            // Enum constructor: Shape.Circle(5) parses as DynCall { callee: FieldAccess { Ident("Shape"), "Circle" }, args: [5] }
            // If the callee is a field access on a known enum, build a NovaEnum instead of invoking a closure.
            Expr::DynCall { callee, args } => {
                // Pre-check for enum constructor before compiling callee (avoids borrow conflicts)
                let enum_ctor = if let Expr::FieldAccess { object, field } = callee.as_ref() {
                    if let Expr::Ident(ename) = object.as_ref() {
                        if self.enum_defs.contains_key(ename) {
                            Some((ename.clone(), field.clone()))
                        } else { None }
                    } else { None }
                } else { None };

                if let Some((_, variant_name)) = enum_ctor {
                    // Enum constructor call: pack args as payload, call nova_make_enum
                    let arg_ptrs: Vec<String> = args.iter()
                        .map(|a| self.compile_expr(a))
                        .collect();
                    let result = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                    let (vgname, _) = self.intern_string(&variant_name);
                    let n = arg_ptrs.len();
                    if n == 0 {
                        let dummy = self.fresh();
                        self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
                        self.emit(&format!(
                            "    call void @nova_make_enum(ptr {}, ptr {}, i64 0, ptr {})",
                            vgname, dummy, result
                        ));
                    } else {
                        let payload = self.fresh();
                        self.emit(&format!("    {} = alloca [{} x %NovaValue], align 8", payload, n));
                        for (i, ap) in arg_ptrs.iter().enumerate() {
                            let slot = self.fresh();
                            self.emit(&format!(
                                "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                                slot, n, payload, i
                            ));
                            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", ap, slot));
                        }
                        self.emit(&format!(
                            "    call void @nova_make_enum(ptr {}, ptr {}, i64 {}, ptr {})",
                            vgname, payload, n, result
                        ));
                    }
                    result
                } else {
                    // Regular closure dispatch
                    let closure_ptr = self.compile_expr(callee);
                    let arg_ptrs: Vec<String> = args.iter()
                        .map(|a| self.compile_expr(a))
                        .collect();
                    let result = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                    self.emit_closure_call(&closure_ptr, &arg_ptrs, &result);
                    result
                }
            }

            // Struct literal: Point { x: 1, y: 2 }
            // Structs are backed by TAG_MAP — the struct name is just a constructor tag
            // (not stored at runtime); field names become string keys in the map.
            Expr::StructLit { fields, .. } => {
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_make_map(ptr {})", result));
                for (fname, fval) in fields {
                    let (gname, _) = self.intern_string(fname);
                    let kptr = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", kptr));
                    self.emit(&format!("    call void @nova_make_str(ptr {}, ptr {})", gname, kptr));
                    let vptr = self.compile_expr(fval);
                    self.emit(&format!("    call void @nova_map_insert(ptr {}, ptr {}, ptr {})", result, kptr, vptr));
                }
                result
            }

            // Field read: p.x
            // If object is a known enum name: create an arity-0 enum variant (e.g. Color.Red).
            // Otherwise: string-keyed map lookup (struct field read).
            Expr::FieldAccess { object, field } => {
                // Check if object is a known enum (arity-0 constructor)
                let is_enum_ctor0 = if let Expr::Ident(ename) = object.as_ref() {
                    self.enum_defs.get(ename)
                        .and_then(|vs| vs.iter().find(|(v, _)| v == field))
                        .map(|(_, arity)| *arity == 0)
                        .unwrap_or(false)
                } else { false };

                let variant_name = field.clone();
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));

                if is_enum_ctor0 {
                    let (vgname, _) = self.intern_string(&variant_name);
                    let dummy = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
                    self.emit(&format!(
                        "    call void @nova_make_enum(ptr {}, ptr {}, i64 0, ptr {})",
                        vgname, dummy, result
                    ));
                } else {
                    let obj_ptr = self.compile_expr(object);
                    let (gname, _) = self.intern_string(field);
                    let kptr = self.fresh();
                    self.emit(&format!("    {} = alloca %NovaValue, align 8", kptr));
                    self.emit(&format!("    call void @nova_make_str(ptr {}, ptr {})", gname, kptr));
                    self.emit(&format!(
                        "    call void @nova_index_get(ptr {}, ptr {}, ptr {})", obj_ptr, kptr, result
                    ));
                }
                result
            }

            // match value { pattern -> body ... }
            // Compiles to a chain of conditional blocks — each arm checks its pattern and
            // either executes its body or falls through to the next arm.
            Expr::Match { value, arms } => {
                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));
                self.emit(&format!("    call void @nova_make_nil(ptr {})", result));

                let match_ptr = self.compile_expr(value);
                let match_id = self.tmp;
                self.tmp += 1;
                let end_lbl = format!("match_end{}", match_id);

                for (arm_idx, (pattern, body)) in arms.iter().enumerate() {
                    let is_last = arm_idx == arms.len() - 1;
                    let arm_id  = self.tmp;
                    self.tmp += 1;

                    match pattern {
                        // Wildcard arm — no condition, body executes unconditionally
                        None => {
                            self.compile_arm_body(body, &result);
                            self.emit(&format!("    br label %{}", end_lbl));
                        }

                        // Enum pattern: Color.Red or Shape.Circle(r)
                        Some(Expr::EnumPattern { variant, bindings, .. }) => {
                            let body_lbl = format!("arm_body{}", arm_id);
                            let skip_lbl = if is_last {
                                end_lbl.clone()
                            } else {
                                format!("arm_skip{}", arm_id)
                            };

                            // Check the variant name matches
                            let (vgname, _) = self.intern_string(variant);
                            let check = self.fresh();
                            self.emit(&format!("    {} = alloca %NovaValue, align 8", check));
                            self.emit(&format!(
                                "    call void @nova_check_enum(ptr {}, ptr {}, ptr {})",
                                match_ptr, vgname, check
                            ));
                            let cond = self.fresh();
                            self.emit(&format!("    {} = call i1 @nova_truthy(ptr {})", cond, check));
                            self.emit(&format!(
                                "    br i1 {}, label %{}, label %{}", cond, body_lbl, skip_lbl
                            ));

                            self.emit(&format!("{}:", body_lbl));
                            self.push_scope();
                            // Bind payload variables
                            for (j, binding) in bindings.iter().enumerate() {
                                let local = format!("%local_{}", binding);
                                self.emit(&format!("    {} = alloca %NovaValue, align 8", local));
                                self.define_local(binding, &local);
                                self.emit(&format!(
                                    "    call void @nova_get_enum_payload(ptr {}, i64 {}, ptr {})",
                                    match_ptr, j, local
                                ));
                            }
                            self.compile_arm_body(body, &result);
                            self.pop_scope();
                            self.emit(&format!("    br label %{}", end_lbl));

                            if !is_last {
                                self.emit(&format!("{}:", skip_lbl));
                            }
                        }

                        // Literal / expression pattern: 1, "red", true, ...
                        Some(pat_expr) => {
                            let body_lbl = format!("arm_body{}", arm_id);
                            let skip_lbl = if is_last {
                                end_lbl.clone()
                            } else {
                                format!("arm_skip{}", arm_id)
                            };

                            let pat_ptr = self.compile_expr(pat_expr);
                            let eq_out  = self.fresh();
                            self.emit(&format!("    {} = alloca %NovaValue, align 8", eq_out));
                            self.emit(&format!(
                                "    call void @nova_eq(ptr {}, ptr {}, ptr {})",
                                match_ptr, pat_ptr, eq_out
                            ));
                            let cond = self.fresh();
                            self.emit(&format!("    {} = call i1 @nova_truthy(ptr {})", cond, eq_out));
                            self.emit(&format!(
                                "    br i1 {}, label %{}, label %{}", cond, body_lbl, skip_lbl
                            ));

                            self.emit(&format!("{}:", body_lbl));
                            self.push_scope();
                            self.compile_arm_body(body, &result);
                            self.pop_scope();
                            self.emit(&format!("    br label %{}", end_lbl));

                            if !is_last {
                                self.emit(&format!("{}:", skip_lbl));
                            }
                        }
                    }
                }

                self.emit(&format!("{}:", end_lbl));
                result
            }

            // `return expr` compiled as an expression (happens when return is the last
            // statement in a function body — we compile the last body item via compile_expr)
            // Copy the value to the return slot, branch to the exit, then fall into a dead
            // block for any instructions that follow (they are unreachable but must be valid IR)
            Expr::Return(val) => {
                if let (Some(ret), Some(exit)) =
                    (self.ret_ptr.clone(), self.fn_exit_label.clone())
                {
                    let v = self.compile_expr(val);
                    self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", v, ret));
                    self.emit(&format!("    br label %{}", exit));
                    let dead = format!("dead{}", self.tmp);
                    self.tmp += 1;
                    self.emit(&format!("{}:", dead));
                }
                // placeholder nil — this is in the dead block and never used
                let ptr = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", ptr));
                self.emit(&format!("    call void @nova_make_nil(ptr {})", ptr));
                ptr
            }

            // spawn expr — compile inner as a zero-arg wrapper closure, run on a new thread
            Expr::Spawn(inner) => self.compile_spawn(inner),

            // Method call: obj.method(args)
            // If obj is a known enum name, this is an enum constructor: Shape.Circle(5).
            // Otherwise method calls are not yet implemented — produce nil.
            Expr::MethodCall { object, method, args } => {
                let enum_ctor = if let Expr::Ident(ename) = object.as_ref() {
                    if self.enum_defs.contains_key(ename) {
                        Some(method.clone())
                    } else { None }
                } else { None };

                let result = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", result));

                // Determine what kind of method call this is
                let struct_dispatch: Option<String> = if enum_ctor.is_none() {
                    let obj_ty = self.infer_type(object);
                    if let StaticType::Struct(type_name) = obj_ty {
                        if self.defined_methods.contains(&(type_name.clone(), method.clone())) {
                            Some(type_name)
                        } else { None }
                    } else { None }
                } else { None };

                if let Some(variant_name) = enum_ctor {
                    let arg_ptrs: Vec<String> = args.iter()
                        .map(|a| self.compile_expr(a))
                        .collect();
                    let (vgname, _) = self.intern_string(&variant_name);
                    let n = arg_ptrs.len();
                    if n == 0 {
                        let dummy = self.fresh();
                        self.emit(&format!("    {} = alloca %NovaValue, align 8", dummy));
                        self.emit(&format!(
                            "    call void @nova_make_enum(ptr {}, ptr {}, i64 0, ptr {})",
                            vgname, dummy, result
                        ));
                    } else {
                        let payload = self.fresh();
                        self.emit(&format!("    {} = alloca [{} x %NovaValue], align 8", payload, n));
                        for (i, ap) in arg_ptrs.iter().enumerate() {
                            let slot = self.fresh();
                            self.emit(&format!(
                                "    {} = getelementptr [{} x %NovaValue], ptr {}, i64 0, i64 {}",
                                slot, n, payload, i
                            ));
                            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", ap, slot));
                        }
                        self.emit(&format!(
                            "    call void @nova_make_enum(ptr {}, ptr {}, i64 {}, ptr {})",
                            vgname, payload, n, result
                        ));
                    }
                } else if let Some(type_name) = struct_dispatch {
                    // Direct call: obj.method(args) → @nova_method_{TypeName}_{method}(ptr obj, ptr args..., ptr result)
                    let obj_ptr = self.compile_expr(object);
                    let arg_ptrs: Vec<String> = args.iter()
                        .map(|a| self.compile_expr(a))
                        .collect();
                    let fn_name = format!("nova_method_{}_{}", type_name, method);
                    let mut call_args = vec![format!("ptr {}", obj_ptr)];
                    for ap in &arg_ptrs { call_args.push(format!("ptr {}", ap)); }
                    call_args.push(format!("ptr {}", result));
                    self.emit(&format!("    call void @{}({})", fn_name, call_args.join(", ")));
                } else {
                    self.emit(&format!("    call void @nova_make_nil(ptr {})", result));
                }
                result
            }

            // throw expr — sets the global throw flag and jumps to the nearest catch or fn_exit.
            // Can appear in expression position (e.g. as the last expr in a fn body), returning nil.
            Expr::Throw(val) => {
                let vptr = self.compile_expr(val);
                self.emit(&format!("    call void @nova_throw(ptr {})", vptr));
                // Jump to: innermost try's catch label > function exit > dead block
                let dead = format!("throw_dead{}", self.tmp); self.tmp += 1;
                if let Some(catch_lbl) = self.try_ctx.last().cloned() {
                    self.emit(&format!("    br label %{}", catch_lbl));
                } else if let Some(exit_lbl) = self.fn_exit_label.clone() {
                    if let Some(ret) = self.ret_ptr.clone() {
                        let nil = self.fresh();
                        self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
                        self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
                        self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", nil, ret));
                    }
                    self.emit(&format!("    br label %{}", exit_lbl));
                } else {
                    self.emit(&format!("    br label %{}", dead));
                }
                self.emit(&format!("{}:", dead));
                let nil = self.fresh();
                self.emit(&format!("    {} = alloca %NovaValue, align 8", nil));
                self.emit(&format!("    call void @nova_make_nil(ptr {})", nil));
                nil
            }

            // Unsupported node — hard error so bugs are never silently nil at runtime.
            other => {
                let tag = format!("{:?}", other);
                let short = tag.split(|c| c == ' ' || c == '{' || c == '(').next().unwrap_or("?");
                eprintln!("error: unsupported expression in LLVM backend: {}", short);
                std::process::exit(1);
            }
        }
    }

    // If/else as a statement (result not needed).
    //
    // LLVM requires explicit branches between basic blocks:
    //   eval cond → br to thenN or elseN/endN
    //   thenN: body → br to endN
    //   elseN: body → br to endN  (only if else branch exists)
    //   endN:  (execution continues)
    fn compile_if(&mut self, cond: &Expr, then_body: &[Expr], else_body: Option<&[Expr]>) {
        let cond_ptr = self.compile_expr(cond);
        let cond_i1  = self.fresh();
        let lbl_then = format!("then{}", self.tmp);
        let lbl_else = format!("else{}", self.tmp + 1);
        let lbl_end  = format!("end{}", self.tmp + 2);
        self.tmp += 3;

        self.emit(&format!("    {} = call i1 @nova_truthy(ptr {})", cond_i1, cond_ptr));
        if else_body.is_some() {
            self.emit(&format!("    br i1 {}, label %{}, label %{}", cond_i1, lbl_then, lbl_else));
        } else {
            self.emit(&format!("    br i1 {}, label %{}, label %{}", cond_i1, lbl_then, lbl_end));
        }

        self.emit(&format!("{}:", lbl_then));
        self.push_scope();
        for stmt in then_body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", lbl_end));

        if let Some(else_stmts) = else_body {
            self.emit(&format!("{}:", lbl_else));
            self.push_scope();
            for stmt in else_stmts { self.compile_stmt(stmt); }
            self.pop_scope();
            self.emit(&format!("    br label %{}", lbl_end));
        }

        self.emit(&format!("{}:", lbl_end));
    }

    // If/else as an expression (produces a value written into `out`).
    // Both branches must write a value — the else path emits nil if no else block is present.
    fn compile_if_expr(&mut self, cond: &Expr, then_body: &[Expr], else_body: Option<&[Expr]>, out: &str) {
        let cond_ptr = self.compile_expr(cond);
        let cond_i1  = self.fresh();
        let lbl_then = format!("then{}", self.tmp);
        let lbl_else = format!("else{}", self.tmp + 1);
        let lbl_end  = format!("end{}", self.tmp + 2);
        self.tmp += 3;

        self.emit(&format!("    {} = call i1 @nova_truthy(ptr {})", cond_i1, cond_ptr));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond_i1, lbl_then, lbl_else));

        self.emit(&format!("{}:", lbl_then));
        self.push_scope();
        let then_val = then_body.last().map(|e| self.compile_expr(e));
        if let Some(v) = then_val {
            self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", v, out));
        }
        self.pop_scope();
        self.emit(&format!("    br label %{}", lbl_end));

        self.emit(&format!("{}:", lbl_else));
        self.push_scope();
        if let Some(else_stmts) = else_body {
            let else_val = else_stmts.last().map(|e| self.compile_expr(e));
            if let Some(v) = else_val {
                self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", v, out));
            }
        }
        self.pop_scope();
        self.emit(&format!("    br label %{}", lbl_end));

        self.emit(&format!("{}:", lbl_end));
    }

    // While loop — compiled into three basic blocks:
    //   wcheckN: eval cond → br to wbodyN or wendN
    //   wbodyN:  body      → br back to wcheckN  (back-edge)
    //   wendN:   (loop exits here)
    // for var in start..end — range-based integer loop
    // Uses direct LLVM i64 arithmetic for the counter (fast path always applies here)
    fn compile_for_range(&mut self, var: &str, start: &Expr, end: &Expr, body: &[Expr]) {
        let loop_id   = self.tmp; self.tmp += 1;
        let check_lbl = format!("for_check{}", loop_id);
        let body_lbl  = format!("for_body{}", loop_id);
        let inc_lbl   = format!("for_inc{}", loop_id);   // continue lands here
        let end_lbl   = format!("for_end{}", loop_id);   // break lands here

        let var_ptr = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", var_ptr));
        let start_val = self.compile_expr(start);
        self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", start_val, var_ptr));
        self.define_local(var, &var_ptr);
        self.record_type(var, StaticType::Int);

        let end_slot = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", end_slot));
        let end_val = self.compile_expr(end);
        self.emit(&format!("    call void @nova_copy(ptr {}, ptr {})", end_val, end_slot));

        // continue → inc_lbl (so the increment always runs before re-checking)
        self.push_loop(&inc_lbl, &end_lbl);
        self.emit(&format!("    br label %{}", check_lbl));
        self.emit(&format!("{}:", check_lbl));

        let iv   = self.extract_int(&var_ptr);
        let ev   = self.extract_int(&end_slot);
        let cond = self.fresh();
        self.emit(&format!("    {} = icmp slt i64 {}, {}", cond, iv, ev));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond, body_lbl, end_lbl));

        self.emit(&format!("{}:", body_lbl));
        self.push_scope();
        for stmt in body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", inc_lbl));

        // increment block — continue jumps here, then loops back to check
        self.emit(&format!("{}:", inc_lbl));
        let iv2 = self.extract_int(&var_ptr);
        let inc = self.fresh();
        self.emit(&format!("    {} = add i64 {}, 1", inc, iv2));
        self.store_int(&var_ptr, &inc);
        self.emit(&format!("    br label %{}", check_lbl));

        self.emit(&format!("{}:", end_lbl));
        self.pop_loop();
    }

    // for var in array — iterate over every element of an array
    fn compile_for_array(&mut self, var: &str, arr_expr: &Expr, body: &[Expr]) {
        let loop_id   = self.tmp; self.tmp += 1;
        let check_lbl = format!("for_check{}", loop_id);
        let body_lbl  = format!("for_body{}", loop_id);
        let inc_lbl   = format!("for_inc{}", loop_id);
        let end_lbl   = format!("for_end{}", loop_id);

        let arr_ptr = self.compile_expr(arr_expr);

        // len — only computed once, before the loop
        let len_slot = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", len_slot));
        self.emit(&format!("    call void @nova_len(ptr {}, ptr {})", arr_ptr, len_slot));

        // raw i64 index counter (not exposed as a Nova variable)
        let idx = self.fresh();
        self.emit(&format!("    {} = alloca i64, align 8", idx));
        self.emit(&format!("    store i64 0, ptr {}, align 8", idx));

        // use fresh() so multiple loops with the same var name don't collide in LLVM IR
        let var_ptr  = self.fresh();
        let idx_nova = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", var_ptr));
        self.emit(&format!("    {} = alloca %NovaValue, align 8", idx_nova));
        self.define_local(var, &var_ptr);

        self.push_loop(&inc_lbl, &end_lbl);
        self.emit(&format!("    br label %{}", check_lbl));
        self.emit(&format!("{}:", check_lbl));

        let idx_val = self.fresh();
        let len_val = self.extract_int(&len_slot);
        let cond    = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val, idx));
        self.emit(&format!("    {} = icmp slt i64 {}, {}", cond, idx_val, len_val));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond, body_lbl, end_lbl));

        self.emit(&format!("{}:", body_lbl));
        self.push_scope();

        let idx_val2 = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val2, idx));
        self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", idx_val2, idx_nova));
        self.emit(&format!("    call void @nova_index_get(ptr {}, ptr {}, ptr {})", arr_ptr, idx_nova, var_ptr));

        for stmt in body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", inc_lbl));

        self.emit(&format!("{}:", inc_lbl));
        let idx_val3 = self.fresh();
        let idx_inc  = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val3, idx));
        self.emit(&format!("    {} = add i64 {}, 1", idx_inc, idx_val3));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", idx_inc, idx));
        self.emit(&format!("    br label %{}", check_lbl));

        self.emit(&format!("{}:", end_lbl));
        self.pop_loop();
    }

    // for i, item in array — yields (index, element) each iteration
    fn compile_for_enumerate(&mut self, idx_var: &str, item_var: &str, arr_expr: &Expr, body: &[Expr]) {
        let loop_id   = self.tmp; self.tmp += 1;
        let check_lbl = format!("for_check{}", loop_id);
        let body_lbl  = format!("for_body{}", loop_id);
        let inc_lbl   = format!("for_inc{}", loop_id);
        let end_lbl   = format!("for_end{}", loop_id);

        let arr_ptr = self.compile_expr(arr_expr);

        let len_slot = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", len_slot));
        self.emit(&format!("    call void @nova_len(ptr {}, ptr {})", arr_ptr, len_slot));

        let idx = self.fresh();
        self.emit(&format!("    {} = alloca i64, align 8", idx));
        self.emit(&format!("    store i64 0, ptr {}, align 8", idx));

        // use fresh() so multiple loops with the same var names don't collide in LLVM IR
        let idx_var_ptr  = self.fresh();
        let item_var_ptr = self.fresh();
        let idx_nova     = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", idx_var_ptr));
        self.emit(&format!("    {} = alloca %NovaValue, align 8", item_var_ptr));
        self.emit(&format!("    {} = alloca %NovaValue, align 8", idx_nova));
        self.define_local(idx_var,  &idx_var_ptr);
        self.define_local(item_var, &item_var_ptr);
        self.record_type(idx_var, StaticType::Int);

        self.push_loop(&inc_lbl, &end_lbl);
        self.emit(&format!("    br label %{}", check_lbl));
        self.emit(&format!("{}:", check_lbl));

        let idx_val = self.fresh();
        let len_val = self.extract_int(&len_slot);
        let cond    = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val, idx));
        self.emit(&format!("    {} = icmp slt i64 {}, {}", cond, idx_val, len_val));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond, body_lbl, end_lbl));

        self.emit(&format!("{}:", body_lbl));
        self.push_scope();

        let idx_val2 = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val2, idx));
        self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", idx_val2, idx_var_ptr));
        self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", idx_val2, idx_nova));
        self.emit(&format!("    call void @nova_index_get(ptr {}, ptr {}, ptr {})", arr_ptr, idx_nova, item_var_ptr));

        for stmt in body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", inc_lbl));

        self.emit(&format!("{}:", inc_lbl));
        let idx_val3 = self.fresh();
        let idx_inc  = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val3, idx));
        self.emit(&format!("    {} = add i64 {}, 1", idx_inc, idx_val3));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", idx_inc, idx));
        self.emit(&format!("    br label %{}", check_lbl));

        self.emit(&format!("{}:", end_lbl));
        self.pop_loop();
    }

    // for [a, b, ...] in array — destructures each element into named variables
    fn compile_for_destructure(&mut self, vars: &[String], arr_expr: &Expr, body: &[Expr]) {
        let loop_id   = self.tmp; self.tmp += 1;
        let check_lbl = format!("for_check{}", loop_id);
        let body_lbl  = format!("for_body{}", loop_id);
        let inc_lbl   = format!("for_inc{}", loop_id);
        let end_lbl   = format!("for_end{}", loop_id);

        let arr_ptr = self.compile_expr(arr_expr);

        let len_slot = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", len_slot));
        self.emit(&format!("    call void @nova_len(ptr {}, ptr {})", arr_ptr, len_slot));

        let idx = self.fresh();
        self.emit(&format!("    {} = alloca i64, align 8", idx));
        self.emit(&format!("    store i64 0, ptr {}, align 8", idx));

        // alloca a slot for the current element (sub-array being destructured)
        let elem_ptr = self.fresh();
        let idx_nova = self.fresh();
        self.emit(&format!("    {} = alloca %NovaValue, align 8", elem_ptr));
        self.emit(&format!("    {} = alloca %NovaValue, align 8", idx_nova));

        // use fresh() so multiple loops with the same var names don't collide in LLVM IR
        let var_ptrs: Vec<String> = vars.iter().map(|_| format!("%t{}", {
            let n = self.tmp; self.tmp += 1; n
        })).collect();
        for (v, vp) in vars.iter().zip(var_ptrs.iter()) {
            self.emit(&format!("    {} = alloca %NovaValue, align 8", vp));
            self.define_local(v, vp);
        }

        self.push_loop(&inc_lbl, &end_lbl);
        self.emit(&format!("    br label %{}", check_lbl));
        self.emit(&format!("{}:", check_lbl));

        let idx_val = self.fresh();
        let len_val = self.extract_int(&len_slot);
        let cond    = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val, idx));
        self.emit(&format!("    {} = icmp slt i64 {}, {}", cond, idx_val, len_val));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond, body_lbl, end_lbl));

        self.emit(&format!("{}:", body_lbl));
        self.push_scope();

        let idx_val2 = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val2, idx));
        self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", idx_val2, idx_nova));
        self.emit(&format!("    call void @nova_index_get(ptr {}, ptr {}, ptr {})", arr_ptr, idx_nova, elem_ptr));

        for (i, vp) in var_ptrs.iter().enumerate() {
            let sub_idx = self.fresh();
            self.emit(&format!("    {} = alloca %NovaValue, align 8", sub_idx));
            self.emit(&format!("    call void @nova_make_int(i64 {}, ptr {})", i, sub_idx));
            self.emit(&format!("    call void @nova_index_get(ptr {}, ptr {}, ptr {})", elem_ptr, sub_idx, vp));
        }

        for stmt in body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", inc_lbl));

        self.emit(&format!("{}:", inc_lbl));
        let idx_val3 = self.fresh();
        let idx_inc  = self.fresh();
        self.emit(&format!("    {} = load i64, ptr {}, align 8", idx_val3, idx));
        self.emit(&format!("    {} = add i64 {}, 1", idx_inc, idx_val3));
        self.emit(&format!("    store i64 {}, ptr {}, align 8", idx_inc, idx));
        self.emit(&format!("    br label %{}", check_lbl));

        self.emit(&format!("{}:", end_lbl));
        self.pop_loop();
    }

    fn compile_while(&mut self, cond: &Expr, body: &[Expr]) {
        let lbl_check = format!("wcheck{}", self.tmp);
        let lbl_body  = format!("wbody{}", self.tmp + 1);
        let lbl_end   = format!("wend{}", self.tmp + 2);
        self.tmp += 3;

        self.push_loop(&lbl_check, &lbl_end);

        // Explicit branch into the check block (LLVM requires every block to be entered
        // via a terminator — we can't just "fall into" a label from the previous block)
        self.emit(&format!("    br label %{}", lbl_check));
        self.emit(&format!("{}:", lbl_check));
        let cond_ptr = self.compile_expr(cond);
        let cond_i1  = self.fresh();
        self.emit(&format!("    {} = call i1 @nova_truthy(ptr {})", cond_i1, cond_ptr));
        self.emit(&format!("    br i1 {}, label %{}, label %{}", cond_i1, lbl_body, lbl_end));

        self.emit(&format!("{}:", lbl_body));
        self.push_scope();
        for stmt in body { self.compile_stmt(stmt); }
        self.pop_scope();
        self.emit(&format!("    br label %{}", lbl_check)); // back-edge

        self.emit(&format!("{}:", lbl_end));
        self.pop_loop();
    }

    // Assemble the complete .ll file.
    //
    // Structure:
    //   1. Type definition for %NovaValue
    //   2. Global string constants
    //   3. External runtime function declarations (nova_rt.c / nova_rt.o)
    //   4. User-defined function definitions (compiled first so @main can call them)
    //   5. define i32 @main() { ... }
    // Pre-register all fn/enum/impl definitions in `stmts` so forward references work.
    // Called both by compile_program (for the entry file) and by Expr::Import (for each import).
    fn prescan_stmts(&mut self, stmts: &[Expr]) {
        for stmt in stmts {
            let inner = match stmt {
                Expr::Line(_, inner) => inner.as_ref(),
                other => other,
            };
            match inner {
                Expr::Fn { name, .. } => { self.defined_fns.insert(name.clone()); }
                Expr::EnumDef { name, variants } => {
                    self.enum_defs.insert(name.clone(), variants.clone());
                }
                Expr::ImplBlock { type_name, methods } => {
                    for method in methods {
                        let mfunc = match method {
                            Expr::Line(_, inner) => inner.as_ref(),
                            other => other,
                        };
                        if let Expr::Fn { name, body, .. } = mfunc {
                            self.defined_methods.insert((type_name.clone(), name.clone()));
                            if let Some(last) = body.last() {
                                let ret_ty = self.infer_type(last);
                                if ret_ty != StaticType::Unknown {
                                    self.method_ret_types.insert(
                                        (type_name.clone(), name.clone()), ret_ty
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn compile_program(&mut self, stmts: &[Expr]) -> String {
        self.prescan_stmts(stmts);

        // Compile all top-level statements into the @main body.
        // Fn nodes encountered here push to self.functions instead of emitting inline.
        for stmt in stmts {
            self.compile_stmt(stmt);
        }

        let body_code = std::mem::take(&mut self.out);

        // Assemble the full .ll file
        let mut ll = String::new();

        // NovaValue tagged union type: { tag: i64, payload: i64 }
        ll.push_str("%NovaValue = type { i64, i64 }\n\n");

        // String literal globals — each is a null-terminated byte array constant
        for (gname, content) in &self.strings {
            let len = content.len() + 1;
            let escaped = Self::escape_for_llvm(content);
            ll.push_str(&format!(
                "{} = private constant [{} x i8] c\"{}\\00\"\n",
                gname, len, escaped
            ));
        }
        if !self.strings.is_empty() { ll.push('\n'); }

        // External runtime declarations — all defined in nova_rt.c, linked as nova_rt.o
        ll.push_str("declare void @nova_make_int  (i64, ptr)\n");
        ll.push_str("declare void @nova_make_float(double, ptr)\n");
        ll.push_str("declare void @nova_make_bool (i64, ptr)\n");
        ll.push_str("declare void @nova_make_nil  (ptr)\n");
        ll.push_str("declare void @nova_make_str  (ptr, ptr)\n");
        ll.push_str("declare void @nova_print     (ptr)\n");
        ll.push_str("declare void @nova_copy      (ptr, ptr)\n");
        ll.push_str("declare i1   @nova_truthy    (ptr)\n");
        ll.push_str("declare void @nova_add (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_sub (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_mul (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_div (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_mod (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_eq  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_neq (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_lt  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_lte (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_gt  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_gte (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_and       (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_or        (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_not       (ptr, ptr)\n");
        ll.push_str("declare void @nova_str_build (ptr, i64, ptr)\n");
        ll.push_str("declare void @nova_to_str    (ptr, ptr)\n");
        ll.push_str("declare void @nova_to_int    (ptr, ptr)\n");
        ll.push_str("declare void @nova_to_float  (ptr, ptr)\n");
        ll.push_str("declare void @nova_type_of   (ptr, ptr)\n");
        ll.push_str("declare void @nova_abs        (ptr, ptr)\n");
        ll.push_str("declare void @nova_sqrt       (ptr, ptr)\n");
        ll.push_str("declare void @nova_floor      (ptr, ptr)\n");
        ll.push_str("declare void @nova_ceil       (ptr, ptr)\n");
        ll.push_str("declare void @nova_round      (ptr, ptr)\n");
        ll.push_str("declare void @nova_min        (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_max        (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_upper      (ptr, ptr)\n");
        ll.push_str("declare void @nova_lower      (ptr, ptr)\n");
        ll.push_str("declare void @nova_trim       (ptr, ptr)\n");
        ll.push_str("declare void @nova_contains   (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_starts_with(ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_ends_with  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_replace    (ptr, ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_split      (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_join       (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_sort       (ptr, ptr)\n");
        ll.push_str("declare void @nova_reverse    (ptr, ptr)\n");
        ll.push_str("declare void @nova_pop        (ptr, ptr)\n");
        ll.push_str("declare void @nova_keys       (ptr, ptr)\n");
        ll.push_str("declare void @nova_values     (ptr, ptr)\n");
        ll.push_str("declare void @nova_set_key    (ptr, ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_sum        (ptr, ptr)\n");
        ll.push_str("declare void @nova_hof_map    (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_hof_filter (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_ord        (ptr, ptr)\n");
        ll.push_str("declare void @nova_chr        (ptr, ptr)\n");
        ll.push_str("declare void @nova_println    (ptr, ptr)\n");
        ll.push_str("declare void @nova_printn     (ptr, ptr)\n");
        ll.push_str("declare void @nova_read_file  (ptr, ptr)\n");
        ll.push_str("declare void @nova_write_file (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_input      (ptr, ptr)\n");
        ll.push_str("declare void @nova_throw      (ptr)\n");
        ll.push_str("declare i32  @nova_is_thrown  ()\n");
        ll.push_str("declare void @nova_get_thrown (ptr)\n");
        ll.push_str("declare void @nova_make_array  (ptr)\n");
        ll.push_str("declare void @nova_array_append(ptr, ptr)\n");
        ll.push_str("declare void @nova_array_push  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_make_map    (ptr)\n");
        ll.push_str("declare void @nova_map_insert  (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_index_get   (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_index_set   (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_len           (ptr, ptr)\n");
        ll.push_str("declare void @nova_make_closure   (ptr, ptr, i64, ptr)\n");
        ll.push_str("declare void @nova_invoke_closure (ptr, ptr, i64, ptr)\n");
        ll.push_str("declare void @nova_make_enum       (ptr, ptr, i64, ptr)\n");
        ll.push_str("declare void @nova_check_enum      (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_get_enum_payload(ptr, i64, ptr)\n");
        ll.push_str("declare void @nova_spawn    (ptr, ptr)\n");
        ll.push_str("declare void @nova_wait     (ptr, ptr)\n");
        ll.push_str("declare void @nova_make_chan(ptr)\n");
        ll.push_str("declare void @nova_send     (ptr, ptr, ptr)\n");
        ll.push_str("declare void @nova_recv     (ptr, ptr)\n");
        ll.push_str("declare void @nova_gc          ()\n");
        ll.push_str("declare void @nova_clock        (ptr)\n");
        ll.push_str("declare void @nova_make_array_n (ptr, ptr, ptr)\n");
        ll.push('\n');

        // User-defined function definitions (emitted before @main so calls resolve correctly)
        for fn_def in &self.functions {
            ll.push_str(fn_def);
        }

        // Main function: entry point for the compiled Nova program
        ll.push_str("define i32 @main() {\nentry:\n");
        ll.push_str(&body_code);
        ll.push_str("    call void @nova_gc()\n    ret i32 0\n}\n");

        hoist_allocas(ll)
    }
}

// Move all alloca instructions that landed in non-entry basic blocks back into the
// entry block so that LLVM's allocas execute only once per call, not once per loop
// iteration.  Nova's codegen emits temporaries with `alloca` at the point of first
// use; LLVM only lowers `mem2reg` for entry-block allocas.  Without this, each
// inner-loop iteration does `sub rsp, 16` per alloca — a 512×512 loop with ~20
// allocas per body blows the stack immediately.
fn hoist_allocas(ll: String) -> String {
    let mut out = String::with_capacity(ll.len() + 2048);
    let mut func_lines: Vec<String> = Vec::new();
    let mut in_func = false;

    for line in ll.lines() {
        if !in_func {
            if line.trim().starts_with("define ") {
                in_func = true;
                func_lines.clear();
                func_lines.push(line.to_string());
            } else {
                out.push_str(line);
                out.push('\n');
            }
        } else {
            func_lines.push(line.to_string());
            if line.trim() == "}" {
                out.push_str(&hoist_fn_allocas(&func_lines));
                out.push('\n');
                in_func = false;
            }
        }
    }
    out
}

fn hoist_fn_allocas(lines: &[String]) -> String {
    let mut hoisted: Vec<String> = Vec::new();
    let mut result: Vec<String> = Vec::new();
    let mut in_entry = false;
    let mut entry_pos: Option<usize> = None;

    for line in lines {
        let trimmed = line.trim();
        // Detect basic-block label transitions: a label is a non-empty line whose
        // trimmed form ends with ':' and contains no spaces.
        if !trimmed.is_empty() && trimmed.ends_with(':') && !trimmed.contains(' ') {
            if trimmed == "entry:" {
                in_entry = true;
                entry_pos = Some(result.len());
            } else {
                in_entry = false;
            }
        }
        // Pull allocas out of non-entry blocks; keep everything else in place.
        if !in_entry && trimmed.contains("= alloca %NovaValue, align 8") {
            hoisted.push(line.to_string());
        } else {
            result.push(line.to_string());
        }
    }

    // Re-insert hoisted allocas immediately after the entry: label.
    if let Some(pos) = entry_pos {
        for (i, alloca) in hoisted.into_iter().enumerate() {
            result.insert(pos + 1 + i, alloca);
        }
    }

    result.join("\n")
}
