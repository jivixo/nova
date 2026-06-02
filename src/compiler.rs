// compiler.rs — AST-to-bytecode compiler. The first half of the VM backend.
//
// compile(stmts) walks the Vec<Expr> produced by the parser and emits a flat list of
// Instructions into a Chunk. The VM then executes that list.
//
// Key concepts:
//
//   Backpatching — forward jumps are a problem: when you emit JumpIfFalse for an if-statement,
//   you don't yet know where the else/end is. Solution: emit a placeholder JumpIfFalse(0),
//   record its position, compile the body, then overwrite the placeholder with the real target
//   via chunk.patch(pos, Instruction::JumpIfFalse(real_target)).
//
//   LoopFrame — when compiling a while/for loop, break/continue need to jump to the exit/top.
//   Those targets aren't known while compiling the body. LoopFrame collects the positions of
//   all break/continue placeholders; they're all backpatched at the end of the loop.
//   loop_stack is a Vec so nested loops have their own independent LoopFrame.
//
//   compile_body vs compile_stmt vs compile_expr:
//     compile_expr — compiles an expression, always leaves a value on the stack
//     compile_stmt — compiles a statement, always emits Pop at the end (discards result)
//     compile_body — compiles a function/lambda body: all-but-last as stmts, last as expr
//                    (this is what makes "the last expression is the return value" work)
//
//   is_value_expr — determines whether the tail of a function body should be treated as an
//   expression (leaves value on stack) or a statement (result discarded, nil returned).
//   Getting this wrong causes silent nil returns from functions.
//
//   Local vs Global variables — compile_body flips in_function = true so all var ops inside
//   a function emit LoadLocal/DefineLocal/StoreLocal instead of the global equivalents.
//   This gives each function call its own fresh local variable space.
use std::sync::Arc;
use std::cell::RefCell;
use crate::evaluator::Value;
use crate::lexer::{Lexer, StringPart, Token};
use crate::parser::{Expr, Parser};

thread_local! {
    static IMPORTING: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new());
}

#[derive(Debug, Clone)]
pub enum Instruction {
    // constants & globals
    LoadConst(Value),
    LoadGlobal(String),
    DefineGlobal(String),
    StoreGlobal(String),

    // locals (inside function bodies)
    LoadLocal(String),
    DefineLocal(String),
    StoreLocal(String),

    // arithmetic
    Add, Sub, Mul, Div, Mod,

    // bitwise
    BitAnd, BitOr, BitXor, Shl, Shr,

    // comparisons
    Equal, NotEqual, Less, LessEq, Greater, GreaterEq,

    // logical
    And, Or, Not,

    // control flow
    JumpIfFalse(usize),
    JumpIfNotNil(usize), // peek TOS (no pop); jump if TOS != nil — used for ??
    Jump(usize),

    // collections
    MakeArray(usize),
    MakeHashMap(usize),
    GetIndex,
    GetIndexOrNil, // like GetIndex but returns nil instead of throwing for array out-of-bounds
    SetIndex(String),

    // named functions
    DefineFunc(String, Vec<String>, Arc<Chunk>, bool), // name, params, body, variadic
    Call(String, usize),
    Return,

    // closures / lambdas
    MakeFunc(Vec<String>, Arc<Chunk>), // captures locals at runtime, pushes VmFunc(id)
    DynCall(usize),                    // pop VmFunc from stack, call with N args

    // built-in functions
    CallBuiltin(String, usize),

    // exceptions
    EnterTry(usize), // push TryHandler with catch_ip
    ExitTry,         // pop TryHandler (try block completed normally)
    Throw,           // pop value, unwind to nearest handler

    // I/O
    Print,
    Printn,

    // structs
    MakeStruct(String, Vec<String>), // pop N values (in field order), push Struct
    GetField(String),                // pop struct/enumdef, push field value or variant
    SetField(String),                // pop value, pop struct, set field, push nil
    DefineMethod(String, String, Vec<String>, Arc<Chunk>), // type_name, method_name, params, body
    CallMethod(String, usize),       // method_name, argc; pops argc args + object (self)

    // enums
    CheckEnumVariant(String, String), // (enum_name, variant): pop value, push bool
    GetEnumPayload(usize),            // pop EnumVariant, push payload[n]

    // stack management
    Pop,

    // concurrency
    Defer(Arc<Chunk>), // push sub-chunk onto the current frame's deferred list; runs LIFO on return
    Spawn(Arc<Chunk>), // compile inner expr to sub-chunk, run it on a new thread, push Task
    SpawnAll,          // pop (array, func) from stack, spawn func(elem) for each elem, push results
    // select: pop N channel values, block (or try) until one is ready, push received value, jump to winning arm.
    // arm bodies compiled inline; each ends with Jump(end). default_start is Some(ip) for non-blocking.
    SelectJump(usize, Vec<usize>, Option<usize>), // (n_channels, arm_start_ips, default_start)

    // debug / error reporting
    SetLine(usize), // update the VM's current line number for error messages
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub code: Vec<Instruction>,
}

impl Chunk {
    pub fn new() -> Self { Chunk { code: Vec::new() } }

    pub fn emit(&mut self, instr: Instruction) -> usize {
        let pos = self.code.len();
        self.code.push(instr);
        pos
    }

    pub fn patch(&mut self, pos: usize, instr: Instruction) {
        self.code[pos] = instr;
    }
}

// LoopFrame collects positions of break/continue jump placeholders inside one loop body.
// All positions are backpatched at the end of the loop when the real jump targets are known.
struct LoopFrame {
    break_patches:    Vec<usize>, // positions of Jump(0) placeholders emitted for 'break'
    continue_patches: Vec<usize>, // positions of Jump(0) placeholders emitted for 'continue'
}

// Compiler carries the mutable state needed while walking the AST.
struct Compiler {
    loop_stack:   Vec<LoopFrame>, // stack of active loops — innermost is last; handles nesting
    loop_counter: usize,          // unique suffix for hidden loop-state globals (__end_N, etc.)
    anon_counter: usize,          // unique suffix for anonymous function names (__anon_N)
    in_function:  bool,           // true while compiling a function/lambda body — selects local vs global ops
}

impl Compiler {
    fn new() -> Self {
        Compiler { loop_stack: Vec::new(), loop_counter: 0, anon_counter: 0, in_function: false }
    }

    fn define_var(&self, name: &str) -> Instruction {
        if self.in_function { Instruction::DefineLocal(name.to_string()) }
        else                { Instruction::DefineGlobal(name.to_string()) }
    }
    fn store_var(&self, name: &str) -> Instruction {
        if self.in_function { Instruction::StoreLocal(name.to_string()) }
        else                { Instruction::StoreGlobal(name.to_string()) }
    }
    fn load_var(&self, name: &str) -> Instruction {
        if self.in_function { Instruction::LoadLocal(name.to_string()) }
        else                { Instruction::LoadGlobal(name.to_string()) }
    }

    // Returns true if the expression leaves a value on the stack (value-producing).
    // Used by compile_body to decide whether to call compile_expr (leaves value) or
    // compile_stmt (discards result) for the last statement of a function body.
    // Getting this wrong causes the function to return nil instead of its last expression.
    // Notable: Match IS a value expression (was removed from the false list so that
    // functions whose entire body is a match statement return the match result, not nil).
    fn is_value_expr(expr: &Expr) -> bool {
        match expr {
            Expr::Line(_, inner) => Self::is_value_expr(inner),
            Expr::Let { .. } | Expr::Assign { .. } | Expr::While { .. } |
            Expr::For { .. } | Expr::ForEnumerate { .. } | Expr::ForDestructure { .. } |
            Expr::If { else_block: None, .. } |
            Expr::Break | Expr::Continue | Expr::Return(_) | Expr::Throw(_) |
            Expr::Try { .. } | Expr::LetArrayDestructure { .. } |
            Expr::LetMapDestructure { .. } |
            Expr::Fn { .. } | Expr::Print(_) | Expr::Printn(_) |
            Expr::IndexAssign { .. } | Expr::StructDef { .. } | Expr::FieldAssign { .. } |
            Expr::EnumDef { .. } | Expr::ImplBlock { .. } | Expr::Select { .. } |
            Expr::Defer(_) => false,
            _ => true,
        }
    }

    // Compile a function/lambda body: all stmts as statements except the last,
    // which is compiled as an expression for implicit return (like the tree-walker).
    fn compile_body(&mut self, body: &[Expr], chunk: &mut Chunk) {
        if body.is_empty() {
            chunk.emit(Instruction::LoadConst(Value::Nil));
            return;
        }
        let last = body.len() - 1;
        for stmt in &body[..last] {
            self.compile_stmt(stmt, chunk);
        }
        let tail = &body[last];
        if Self::is_value_expr(tail) {
            self.compile_expr(tail, chunk); // leaves value on stack for Return
        } else {
            self.compile_stmt(tail, chunk);
            chunk.emit(Instruction::LoadConst(Value::Nil));
        }
    }

    fn compile_stmt(&mut self, expr: &Expr, chunk: &mut Chunk) {
        match expr {
            Expr::Line(n, inner) => {
                chunk.emit(Instruction::SetLine(*n));
                self.compile_stmt(inner, chunk);
            }

            Expr::Let { name, value } => {
                self.compile_expr(value, chunk);
                chunk.emit(self.define_var(name));
            }

            Expr::Assign { name, value } => {
                self.compile_expr(value, chunk);
                chunk.emit(self.store_var(name));
            }

            Expr::Print(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Print);
            }

            Expr::Printn(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Printn);
            }

            // if / else
            // Backpatching: emit JumpIfFalse(0) as a placeholder (0 = unknown target),
            // record its position in jf, compile the then-body, then overwrite the
            // placeholder with the real target index once we know where the else/end is.
            Expr::If { condition, then_block, else_block } => {
                self.compile_expr(condition, chunk);
                let jf = chunk.emit(Instruction::JumpIfFalse(0)); // placeholder

                for stmt in then_block { self.compile_stmt(stmt, chunk); }

                if let Some(else_stmts) = else_block {
                    let jo = chunk.emit(Instruction::Jump(0)); // skip else after then executes
                    chunk.patch(jf, Instruction::JumpIfFalse(chunk.code.len())); // backpatch: jump to else
                    for stmt in else_stmts { self.compile_stmt(stmt, chunk); }
                    chunk.patch(jo, Instruction::Jump(chunk.code.len())); // backpatch: jump past else
                } else {
                    chunk.patch(jf, Instruction::JumpIfFalse(chunk.code.len())); // backpatch: jump past then
                }
            }

            // while
            // loop_top = index of the condition instruction — continue jumps back here.
            // LoopFrame accumulates all break/continue placeholder positions during body compilation.
            // After the body, Jump(loop_top) closes the loop, then all placeholders are backpatched.
            Expr::While { condition, body } => {
                let loop_top = chunk.code.len(); // continue target
                self.compile_expr(condition, chunk);
                let exit_jump = chunk.emit(Instruction::JumpIfFalse(0)); // break/exit target (unknown yet)

                self.loop_stack.push(LoopFrame { break_patches: vec![], continue_patches: vec![] });
                for stmt in body { self.compile_stmt(stmt, chunk); }
                let frame = self.loop_stack.pop().unwrap();

                chunk.emit(Instruction::Jump(loop_top)); // go back to condition check
                let exit = chunk.code.len();             // first instruction AFTER the loop
                chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
                for pos in frame.break_patches    { chunk.patch(pos, Instruction::Jump(exit)); }
                for pos in frame.continue_patches { chunk.patch(pos, Instruction::Jump(loop_top)); }
            }

            // for
            Expr::For { var, iter, body } => {
                let mut raw_iter = iter.as_ref();
                while let Expr::Line(_, inner) = raw_iter { raw_iter = inner.as_ref(); }

                match raw_iter {
                    Expr::Range { start, end } => {
                        let n = self.loop_counter;
                        self.loop_counter += 1;
                        let end_var = format!("__end_{}", n);

                        self.compile_expr(start, chunk);
                        chunk.emit(self.define_var(var));
                        self.compile_expr(end, chunk);
                        chunk.emit(self.define_var(&end_var));

                        let loop_top = chunk.code.len();
                        chunk.emit(self.load_var(var));
                        chunk.emit(self.load_var(&end_var));
                        chunk.emit(Instruction::Less);
                        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

                        self.loop_stack.push(LoopFrame { break_patches: vec![], continue_patches: vec![] });
                        for stmt in body { self.compile_stmt(stmt, chunk); }
                        let frame = self.loop_stack.pop().unwrap();

                        let continue_target = chunk.code.len();
                        chunk.emit(self.load_var(var));
                        chunk.emit(Instruction::LoadConst(Value::Int(1)));
                        chunk.emit(Instruction::Add);
                        chunk.emit(self.store_var(var));
                        chunk.emit(Instruction::Jump(loop_top));

                        let exit = chunk.code.len();
                        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
                        for pos in frame.break_patches    { chunk.patch(pos, Instruction::Jump(exit)); }
                        for pos in frame.continue_patches { chunk.patch(pos, Instruction::Jump(continue_target)); }
                    }
                    _ => {
                        let n = self.loop_counter;
                        self.loop_counter += 1;
                        let iter_var = format!("__iter_{}", n);
                        let idx_var  = format!("__idx_{}", n);
                        let len_var  = format!("__len_{}", n);

                        self.compile_expr(raw_iter, chunk);
                        chunk.emit(self.define_var(&iter_var));
                        chunk.emit(self.load_var(&iter_var));
                        chunk.emit(Instruction::CallBuiltin("len".to_string(), 1));
                        chunk.emit(self.define_var(&len_var));
                        chunk.emit(Instruction::LoadConst(Value::Int(0)));
                        chunk.emit(self.define_var(&idx_var));

                        let loop_top = chunk.code.len();
                        chunk.emit(self.load_var(&idx_var));
                        chunk.emit(self.load_var(&len_var));
                        chunk.emit(Instruction::Less);
                        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

                        chunk.emit(self.load_var(&iter_var));
                        chunk.emit(self.load_var(&idx_var));
                        chunk.emit(Instruction::GetIndex);
                        chunk.emit(self.define_var(var));

                        self.loop_stack.push(LoopFrame { break_patches: vec![], continue_patches: vec![] });
                        for stmt in body { self.compile_stmt(stmt, chunk); }
                        let frame = self.loop_stack.pop().unwrap();

                        let continue_target = chunk.code.len();
                        chunk.emit(self.load_var(&idx_var));
                        chunk.emit(Instruction::LoadConst(Value::Int(1)));
                        chunk.emit(Instruction::Add);
                        chunk.emit(self.store_var(&idx_var));
                        chunk.emit(Instruction::Jump(loop_top));

                        let exit = chunk.code.len();
                        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
                        for pos in frame.break_patches    { chunk.patch(pos, Instruction::Jump(exit)); }
                        for pos in frame.continue_patches { chunk.patch(pos, Instruction::Jump(continue_target)); }
                    }
                }
            }

            // for i, item in arr
            Expr::ForEnumerate { index_var, item_var, iter, body } => {
                let n = self.loop_counter;
                self.loop_counter += 1;
                let iter_var = format!("__iter_{}", n);
                let idx_var  = format!("__idx_{}", n);
                let len_var  = format!("__len_{}", n);

                let mut raw_iter = iter.as_ref();
                while let Expr::Line(_, inner) = raw_iter { raw_iter = inner.as_ref(); }

                self.compile_expr(raw_iter, chunk);
                chunk.emit(self.define_var(&iter_var));
                chunk.emit(self.load_var(&iter_var));
                chunk.emit(Instruction::CallBuiltin("len".to_string(), 1));
                chunk.emit(self.define_var(&len_var));
                chunk.emit(Instruction::LoadConst(Value::Int(0)));
                chunk.emit(self.define_var(&idx_var));

                let loop_top = chunk.code.len();
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(self.load_var(&len_var));
                chunk.emit(Instruction::Less);
                let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

                // item = iter[idx], index_var = idx
                chunk.emit(self.load_var(&iter_var));
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(Instruction::GetIndex);
                chunk.emit(self.define_var(item_var));
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(self.define_var(index_var));

                self.loop_stack.push(LoopFrame { break_patches: vec![], continue_patches: vec![] });
                for stmt in body { self.compile_stmt(stmt, chunk); }
                let lf = self.loop_stack.pop().unwrap();

                let continue_target = chunk.code.len();
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(Instruction::LoadConst(Value::Int(1)));
                chunk.emit(Instruction::Add);
                chunk.emit(self.store_var(&idx_var));
                chunk.emit(Instruction::Jump(loop_top));

                let exit = chunk.code.len();
                chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
                for pos in lf.break_patches    { chunk.patch(pos, Instruction::Jump(exit)); }
                for pos in lf.continue_patches { chunk.patch(pos, Instruction::Jump(continue_target)); }
            }

            // for [a, b] in arr
            Expr::ForDestructure { vars, iter, body } => {
                let n = self.loop_counter;
                self.loop_counter += 1;
                let iter_var = format!("__iter_{}", n);
                let idx_var  = format!("__idx_{}", n);
                let len_var  = format!("__len_{}", n);
                let elem_var = format!("__elem_{}", n);

                let mut raw_iter = iter.as_ref();
                while let Expr::Line(_, inner) = raw_iter { raw_iter = inner.as_ref(); }

                self.compile_expr(raw_iter, chunk);
                chunk.emit(self.define_var(&iter_var));
                chunk.emit(self.load_var(&iter_var));
                chunk.emit(Instruction::CallBuiltin("len".to_string(), 1));
                chunk.emit(self.define_var(&len_var));
                chunk.emit(Instruction::LoadConst(Value::Int(0)));
                chunk.emit(self.define_var(&idx_var));

                let loop_top = chunk.code.len();
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(self.load_var(&len_var));
                chunk.emit(Instruction::Less);
                let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

                // elem = iter[idx]
                chunk.emit(self.load_var(&iter_var));
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(Instruction::GetIndex);
                chunk.emit(self.define_var(&elem_var));

                // bind each var: vars[i] = elem[i]
                for (i, v) in vars.iter().enumerate() {
                    chunk.emit(self.load_var(&elem_var));
                    chunk.emit(Instruction::LoadConst(Value::Int(i as i64)));
                    chunk.emit(Instruction::GetIndex);
                    chunk.emit(self.define_var(v));
                }

                self.loop_stack.push(LoopFrame { break_patches: vec![], continue_patches: vec![] });
                for stmt in body { self.compile_stmt(stmt, chunk); }
                let lf = self.loop_stack.pop().unwrap();

                let continue_target = chunk.code.len();
                chunk.emit(self.load_var(&idx_var));
                chunk.emit(Instruction::LoadConst(Value::Int(1)));
                chunk.emit(Instruction::Add);
                chunk.emit(self.store_var(&idx_var));
                chunk.emit(Instruction::Jump(loop_top));

                let exit = chunk.code.len();
                chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
                for pos in lf.break_patches    { chunk.patch(pos, Instruction::Jump(exit)); }
                for pos in lf.continue_patches { chunk.patch(pos, Instruction::Jump(continue_target)); }
            }

            // index assign
            Expr::IndexAssign { name, index, value } => {
                self.compile_expr(value, chunk);
                self.compile_expr(index, chunk);
                chunk.emit(Instruction::SetIndex(name.clone()));
            }

            // break / continue
            Expr::Break => {
                let pos = chunk.emit(Instruction::Jump(0));
                self.loop_stack.last_mut().expect("break outside loop").break_patches.push(pos);
            }

            Expr::Continue => {
                let pos = chunk.emit(Instruction::Jump(0));
                self.loop_stack.last_mut().expect("continue outside loop").continue_patches.push(pos);
            }

            // named function definition
            Expr::Fn { name, params, body, variadic, .. } => {
                let param_names: Vec<String> = params.iter().map(|(n, _, _)| n.clone()).collect();

                let mut sub = Chunk::new();
                let mut inner = Compiler::new();
                inner.in_function = true;

                // Default parameter handling: if param == nil { param = default }
                for (pname, _, pdefault) in params {
                    if let Some(default_expr) = pdefault {
                        sub.emit(Instruction::LoadLocal(pname.clone()));
                        sub.emit(Instruction::LoadConst(Value::Nil));
                        sub.emit(Instruction::Equal);
                        let skip = sub.emit(Instruction::JumpIfFalse(0));
                        inner.compile_expr(default_expr, &mut sub);
                        sub.emit(Instruction::StoreLocal(pname.clone()));
                        sub.patch(skip, Instruction::JumpIfFalse(sub.code.len()));
                    }
                }

                inner.compile_body(body, &mut sub);
                sub.emit(Instruction::Return);

                chunk.emit(Instruction::DefineFunc(name.clone(), param_names, Arc::new(sub), *variadic));
            }

            // return
            Expr::Return(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Return);
            }

            // struct definition
            Expr::StructDef { name, fields } => {
                chunk.emit(Instruction::LoadConst(Value::StructDef {
                    name: name.clone(),
                    field_names: fields.clone(),
                }));
                chunk.emit(self.define_var(name));
            }

            // enum definition
            Expr::EnumDef { name, variants } => {
                chunk.emit(Instruction::LoadConst(Value::EnumDef {
                    name: name.clone(),
                    variants: variants.clone(),
                }));
                chunk.emit(self.define_var(name));
            }

            // field assignment
            Expr::FieldAssign { object, field, value } => {
                self.compile_expr(object, chunk);
                self.compile_expr(value, chunk);
                chunk.emit(Instruction::SetField(field.clone()));
            }

            // impl block
            Expr::ImplBlock { type_name, methods } => {
                for method in methods {
                    let func = match method {
                        Expr::Line(_, inner) => inner.as_ref(),
                        other => other,
                    };
                    if let Expr::Fn { name, params, body, .. } = func {
                        let mut sub = Chunk::new();
                        let param_names: Vec<String> = params.iter().map(|(p, _, _)| p.clone()).collect();
                        // use a fresh inner compiler with in_function=true so variables
                        // inside the method body compile as LoadLocal/StoreLocal, not globals
                        let mut inner = Compiler::new();
                        inner.in_function = true;
                        inner.compile_body(body, &mut sub);
                        sub.emit(Instruction::Return);
                        chunk.emit(Instruction::DefineMethod(
                            type_name.clone(),
                            name.clone(),
                            param_names,
                            Arc::new(sub),
                        ));
                    }
                }
            }

            // import
            Expr::Import(path) => {
                let canonical = std::fs::canonicalize(path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.clone());
                let already = IMPORTING.with(|s| s.borrow().contains(&canonical));
                if already {
                    panic!("circular import detected: '{}'", path);
                }
                let source = std::fs::read_to_string(path)
                    .unwrap_or_else(|_| panic!("cannot import '{}': file not found", path));
                IMPORTING.with(|s| s.borrow_mut().insert(canonical.clone()));
                let mut lex = Lexer::new(&source);
                let tokens = lex.tokenize();
                let mut parser = Parser::new(tokens);
                let mut stmts = Vec::new();
                while !matches!(parser.current_token(), Token::EOF) {
                    stmts.push(parser.parse_statement());
                }
                for stmt in stmts {
                    self.compile_stmt(&stmt, chunk);
                }
                IMPORTING.with(|s| s.borrow_mut().remove(&canonical));
            }

            // throw
            Expr::Throw(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Throw);
            }

            // try / catch
            Expr::Try { body, catch_var, catch_body } => {
                let enter = chunk.emit(Instruction::EnterTry(0));
                for stmt in body { self.compile_stmt(stmt, chunk); }
                chunk.emit(Instruction::ExitTry);
                let skip_catch = chunk.emit(Instruction::Jump(0));
                chunk.patch(enter, Instruction::EnterTry(chunk.code.len()));
                chunk.emit(self.define_var(catch_var));
                for stmt in catch_body { self.compile_stmt(stmt, chunk); }
                chunk.patch(skip_catch, Instruction::Jump(chunk.code.len()));
            }

            // let [a, b] = expr
            Expr::LetArrayDestructure { names, value } => {
                let n = self.anon_counter;
                self.anon_counter += 1;
                let dest = format!("__dest_{}", n);
                self.compile_expr(value, chunk);
                chunk.emit(self.define_var(&dest));
                for (i, name) in names.iter().enumerate() {
                    chunk.emit(self.load_var(&dest));
                    chunk.emit(Instruction::LoadConst(Value::Int(i as i64)));
                    chunk.emit(Instruction::GetIndexOrNil); // nil for missing elements
                    chunk.emit(self.define_var(name));
                }
            }

            // let {name, age} = expr
            Expr::LetMapDestructure { names, value } => {
                let n = self.anon_counter;
                self.anon_counter += 1;
                let dest = format!("__dest_{}", n);
                self.compile_expr(value, chunk);
                chunk.emit(self.define_var(&dest));
                for name in names {
                    chunk.emit(self.load_var(&dest));
                    chunk.emit(Instruction::LoadConst(Value::Str(name.clone())));
                    chunk.emit(Instruction::GetIndex);
                    chunk.emit(self.define_var(name));
                }
            }

            // match statement
            Expr::Match { value, arms } => {
                let n = self.anon_counter;
                self.anon_counter += 1;
                let match_var = format!("__match_{}", n);

                self.compile_expr(value, chunk);
                chunk.emit(self.define_var(&match_var));

                let mut end_patches: Vec<usize> = Vec::new();

                for (pattern, body) in arms {
                    let skip_jump = match pattern {
                        None => None,
                        Some(Expr::Range { start, end }) => {
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(start, chunk);
                            chunk.emit(Instruction::GreaterEq);
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(end, chunk);
                            chunk.emit(Instruction::Less);
                            chunk.emit(Instruction::And);
                            Some(chunk.emit(Instruction::JumpIfFalse(0)))
                        }
                        Some(Expr::EnumPattern { enum_name, variant, bindings }) => {
                            chunk.emit(self.load_var(&match_var));
                            chunk.emit(Instruction::CheckEnumVariant(enum_name.clone(), variant.clone()));
                            let skip = chunk.emit(Instruction::JumpIfFalse(0));
                            for (i, binding) in bindings.iter().enumerate() {
                                chunk.emit(self.load_var(&match_var));
                                chunk.emit(Instruction::GetEnumPayload(i));
                                chunk.emit(self.define_var(binding));
                            }
                            Some(skip)
                        }
                        Some(pat) => {
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(pat, chunk);
                            chunk.emit(Instruction::Equal);
                            Some(chunk.emit(Instruction::JumpIfFalse(0)))
                        }
                    };

                    for stmt in body { self.compile_stmt(stmt, chunk); }
                    end_patches.push(chunk.emit(Instruction::Jump(0)));

                    if let Some(skip) = skip_jump {
                        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));
                    }
                }

                let end = chunk.code.len();
                for pos in end_patches { chunk.patch(pos, Instruction::Jump(end)); }
            }

            // defer is a statement — Instruction::Defer pushes no value, so no Pop
            Expr::Defer(_) => {
                self.compile_expr(expr, chunk);
            }

            // select is a statement — SelectJump + inline arm bodies push no final value, so no Pop
            Expr::Select { .. } => {
                self.compile_expr(expr, chunk);
            }

            // expression as statement
            other => {
                self.compile_expr(other, chunk);
                chunk.emit(Instruction::Pop);
            }
        }
    }

    fn compile_expr(&mut self, expr: &Expr, chunk: &mut Chunk) {
        match expr {
            Expr::IntLit(n)   => { chunk.emit(Instruction::LoadConst(Value::Int(*n))); }
            Expr::FloatLit(f) => { chunk.emit(Instruction::LoadConst(Value::Float(*f))); }
            Expr::BoolLit(b)  => { chunk.emit(Instruction::LoadConst(Value::Bool(*b))); }
            Expr::NilLit      => { chunk.emit(Instruction::LoadConst(Value::Nil)); }
            Expr::StrLit(s)   => { chunk.emit(Instruction::LoadConst(Value::Str(s.clone()))); }

            Expr::Ident(name) => { chunk.emit(self.load_var(name)); }

            Expr::Not(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Not);
            }

            Expr::BinaryOp { left, op, right } => {
                if matches!(op, Token::QuestionQuestion) {
                    // Lazy null-coalesce: only evaluate right if left is nil.
                    self.compile_expr(left, chunk);
                    let jnn = chunk.emit(Instruction::JumpIfNotNil(0));
                    chunk.emit(Instruction::Pop);
                    self.compile_expr(right, chunk);
                    chunk.patch(jnn, Instruction::JumpIfNotNil(chunk.code.len()));
                } else {
                    self.compile_expr(left, chunk);
                    self.compile_expr(right, chunk);
                    let instr = match op {
                        Token::Plus          => Instruction::Add,
                        Token::Minus         => Instruction::Sub,
                        Token::Star          => Instruction::Mul,
                        Token::Slash         => Instruction::Div,
                        Token::Percent       => Instruction::Mod,
                        Token::EqualsEquals  => Instruction::Equal,
                        Token::BangEquals    => Instruction::NotEqual,
                        Token::Less          => Instruction::Less,
                        Token::LessEquals    => Instruction::LessEq,
                        Token::Greater       => Instruction::Greater,
                        Token::GreaterEquals => Instruction::GreaterEq,
                        Token::And           => Instruction::And,
                        Token::Or            => Instruction::Or,
                        Token::BitAnd        => Instruction::BitAnd,
                        Token::BitOr         => Instruction::BitOr,
                        Token::BitXor        => Instruction::BitXor,
                        Token::Shl           => Instruction::Shl,
                        Token::Shr           => Instruction::Shr,
                        op => panic!("unsupported binary operator {:?}", op),
                    };
                    chunk.emit(instr);
                }
            }

            Expr::Array(elems) => {
                for e in elems { self.compile_expr(e, chunk); }
                chunk.emit(Instruction::MakeArray(elems.len()));
            }

            Expr::HashMap(pairs) => {
                for (k, v) in pairs {
                    self.compile_expr(k, chunk);
                    self.compile_expr(v, chunk);
                }
                chunk.emit(Instruction::MakeHashMap(pairs.len()));
            }

            Expr::Index { object, index } => {
                self.compile_expr(object, chunk);
                self.compile_expr(index, chunk);
                chunk.emit(Instruction::GetIndex);
            }

            Expr::StrInterp(parts) => {
                if parts.is_empty() {
                    chunk.emit(Instruction::LoadConst(Value::Str(String::new())));
                    return;
                }
                // Compile each part then concatenate with Add — handles arbitrary expressions.
                let mut pushed = 0;
                for part in parts {
                    match part {
                        StringPart::Literal(s) => {
                            chunk.emit(Instruction::LoadConst(Value::Str(s.clone())));
                        }
                        StringPart::Interp(expr_text) => {
                            // Re-parse the interpolated expression at compile time.
                            let tokens = Lexer::new(expr_text).tokenize();
                            let inner = Parser::new(tokens).parse_null_coalesce();
                            self.compile_expr(&inner, chunk);
                            chunk.emit(Instruction::CallBuiltin("str".to_string(), 1));
                        }
                    }
                    pushed += 1;
                    if pushed > 1 {
                        chunk.emit(Instruction::Add);
                    }
                }
            }

            // lambda
            // if-else as expression (yields a value)
            Expr::If { condition, then_block, else_block: Some(else_stmts) } => {
                self.compile_expr(condition, chunk);
                let jf = chunk.emit(Instruction::JumpIfFalse(0));
                self.compile_body(then_block, chunk);
                let jo = chunk.emit(Instruction::Jump(0));
                chunk.patch(jf, Instruction::JumpIfFalse(chunk.code.len()));
                self.compile_body(else_stmts, chunk);
                chunk.patch(jo, Instruction::Jump(chunk.code.len()));
            }

            Expr::Lambda { params, body } => {
                let mut sub = Chunk::new();
                let mut inner = Compiler::new();
                inner.in_function = true;
                inner.compile_body(body, &mut sub);
                sub.emit(Instruction::Return);
                chunk.emit(Instruction::MakeFunc(params.clone(), Arc::new(sub)));
            }

            // dynamic call: expr(args)
            Expr::DynCall { callee, args } => {
                for arg in args { self.compile_expr(arg, chunk); }
                self.compile_expr(callee, chunk);
                chunk.emit(Instruction::DynCall(args.len()));
            }

            // named call / HOF expansion
            Expr::Call { name, args } => {
                match name.as_str() {
                    "map" if args.len() == 2 => {
                        self.compile_hof_map(&args[0], &args[1], chunk);
                    }
                    "filter" if args.len() == 2 => {
                        self.compile_hof_filter(&args[0], &args[1], chunk);
                    }
                    "reduce" if args.len() == 3 => {
                        self.compile_hof_reduce(&args[0], &args[1], &args[2], chunk);
                    }
                    "any" if args.len() == 2 => {
                        self.compile_hof_any(&args[0], &args[1], chunk);
                    }
                    "all" if args.len() == 2 => {
                        self.compile_hof_all(&args[0], &args[1], chunk);
                    }
                    "count" if args.len() == 2 => {
                        self.compile_hof_count(&args[0], &args[1], chunk);
                    }
                    "find" if args.len() == 2 => {
                        self.compile_hof_find(&args[0], &args[1], chunk);
                    }
                    "spawnAll" if args.len() == 2 => {
                        self.compile_expr(&args[0], chunk); // push array
                        self.compile_expr(&args[1], chunk); // push function
                        chunk.emit(Instruction::SpawnAll);
                    }
                    "withLock" if args.len() == 2 => {
                        self.compile_hof_with_lock(&args[0], &args[1], chunk);
                    }
                    _ => {
                        for arg in args { self.compile_expr(arg, chunk); }
                        if is_builtin(name) {
                            chunk.emit(Instruction::CallBuiltin(name.clone(), args.len()));
                        } else {
                            chunk.emit(Instruction::Call(name.clone(), args.len()));
                        }
                    }
                }
            }

            Expr::StructLit { name, fields } => {
                let field_names: Vec<String> = fields.iter().map(|(f, _)| f.clone()).collect();
                for (_, fexpr) in fields {
                    self.compile_expr(fexpr, chunk);
                }
                chunk.emit(Instruction::MakeStruct(name.clone(), field_names));
            }

            Expr::FieldAccess { object, field } => {
                self.compile_expr(object, chunk);
                chunk.emit(Instruction::GetField(field.clone()));
            }

            // match as expression: let x = match v { ... } — each arm yields a value via compile_body
            Expr::Match { value, arms } => {
                let n = self.anon_counter;
                self.anon_counter += 1;
                let match_var = format!("__match_{}", n);

                self.compile_expr(value, chunk);
                chunk.emit(self.define_var(&match_var));

                let mut end_patches: Vec<usize> = Vec::new();

                for (pattern, body) in arms {
                    let skip_jump = match pattern {
                        None => None,
                        Some(Expr::Range { start, end }) => {
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(start, chunk);
                            chunk.emit(Instruction::GreaterEq);
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(end, chunk);
                            chunk.emit(Instruction::Less);
                            chunk.emit(Instruction::And);
                            Some(chunk.emit(Instruction::JumpIfFalse(0)))
                        }
                        Some(Expr::EnumPattern { enum_name, variant, bindings }) => {
                            chunk.emit(self.load_var(&match_var));
                            chunk.emit(Instruction::CheckEnumVariant(enum_name.clone(), variant.clone()));
                            let skip = chunk.emit(Instruction::JumpIfFalse(0));
                            for (i, binding) in bindings.iter().enumerate() {
                                chunk.emit(self.load_var(&match_var));
                                chunk.emit(Instruction::GetEnumPayload(i));
                                chunk.emit(self.define_var(binding));
                            }
                            Some(skip)
                        }
                        Some(pat) => {
                            chunk.emit(self.load_var(&match_var));
                            self.compile_expr(pat, chunk);
                            chunk.emit(Instruction::Equal);
                            Some(chunk.emit(Instruction::JumpIfFalse(0)))
                        }
                    };

                    self.compile_body(body, chunk); // leaves last value on stack
                    end_patches.push(chunk.emit(Instruction::Jump(0)));

                    if let Some(skip) = skip_jump {
                        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));
                    }
                }

                // fallback: no arm matched → push nil
                chunk.emit(Instruction::LoadConst(Value::Nil));
                let end = chunk.code.len();
                for pos in end_patches { chunk.patch(pos, Instruction::Jump(end)); }
            }

            // method call
            Expr::MethodCall { object, method, args } => {
                self.compile_expr(object, chunk); // pushed as self (arg 0)
                for arg in args { self.compile_expr(arg, chunk); }
                chunk.emit(Instruction::CallMethod(method.clone(), args.len()));
            }

            Expr::Line(n, inner) => {
                chunk.emit(Instruction::SetLine(*n));
                self.compile_expr(inner, chunk);
            }

            Expr::Defer(inner) => {
                // compile the deferred expression as a sub-chunk; pushed to the current frame's
                // deferred list and executed LIFO when the function returns.
                let mut sub = Chunk::new();
                let mut sub_compiler = Compiler::new();
                sub_compiler.in_function = self.in_function; // inherit scope context
                sub_compiler.compile_stmt(inner, &mut sub); // stmt so nothing is left on stack
                chunk.emit(Instruction::Defer(Arc::new(sub)));
            }

            Expr::Spawn(inner) => {
                // compile the spawn body into a sub-chunk; the VM runs it on a new thread
                let mut sub = Chunk::new();
                let mut sub_compiler = Compiler::new();
                sub_compiler.compile_expr(inner, &mut sub);
                chunk.emit(Instruction::Spawn(Arc::new(sub)));
            }

            Expr::Throw(inner) => {
                self.compile_expr(inner, chunk);
                chunk.emit(Instruction::Throw);
            }

            Expr::Select { arms, default_body } => {
                // Push each channel expression onto the stack (one per arm, popped by SelectJump)
                for (ch_expr, _, _) in arms {
                    self.compile_expr(ch_expr, chunk);
                }
                // Emit SelectJump placeholder — arm start IPs not known yet
                let select_pos = chunk.emit(Instruction::SelectJump(arms.len(), vec![0; arms.len()], None));
                // Compile each case arm body inline
                let mut arm_starts: Vec<usize> = Vec::new();
                let mut end_patches: Vec<usize> = Vec::new();
                for (_, bind_var, body) in arms {
                    arm_starts.push(chunk.code.len());
                    // SelectJump pushed the received value; consume it into bind_var
                    chunk.emit(self.define_var(bind_var));
                    for stmt in body { self.compile_stmt(stmt, chunk); }
                    end_patches.push(chunk.emit(Instruction::Jump(0)));
                }
                // Compile the optional default arm (no bind_var — no value was received)
                let default_start = if let Some(default_stmts) = default_body {
                    let start = chunk.code.len();
                    for stmt in default_stmts { self.compile_stmt(stmt, chunk); }
                    end_patches.push(chunk.emit(Instruction::Jump(0)));
                    Some(start)
                } else {
                    None
                };
                let end = chunk.code.len();
                for pos in end_patches { chunk.patch(pos, Instruction::Jump(end)); }
                chunk.patch(select_pos, Instruction::SelectJump(arms.len(), arm_starts, default_start));
            }

            other => panic!("VM: unsupported expression {:?}", other),
        }
    }

    // HOF compile-time expansions

    fn hof_setup(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk)
        -> (String, String, String, String, String)
    {
        let n = self.anon_counter;
        self.anon_counter += 1;
        let arr_v  = format!("__hof_arr_{}", n);
        let fn_v   = format!("__hof_fn_{}", n);
        let idx_v  = format!("__hof_idx_{}", n);
        let len_v  = format!("__hof_len_{}", n);
        let item_v = format!("__hof_item_{}", n);

        self.compile_expr(arr_expr, chunk);
        chunk.emit(self.define_var(&arr_v));
        self.compile_expr(fn_expr, chunk);
        chunk.emit(self.define_var(&fn_v));
        chunk.emit(self.load_var(&arr_v));
        chunk.emit(Instruction::CallBuiltin("len".to_string(), 1));
        chunk.emit(self.define_var(&len_v));
        chunk.emit(Instruction::LoadConst(Value::Int(0)));
        chunk.emit(self.define_var(&idx_v));

        (arr_v, fn_v, idx_v, len_v, item_v)
    }

    fn compile_hof_map(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter; // capture before hof_setup increments it
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::MakeArray(0));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        // res = push(res, fn(item))
        chunk.emit(self.load_var(&res_v));
        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        chunk.emit(Instruction::CallBuiltin("push".to_string(), 2));
        chunk.emit(self.store_var(&res_v));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.emit(self.load_var(&res_v));
    }

    fn compile_hof_filter(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::MakeArray(0));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        // if fn(item): res = push(res, item)
        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        let skip = chunk.emit(Instruction::JumpIfFalse(0));
        chunk.emit(self.load_var(&res_v));
        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::CallBuiltin("push".to_string(), 2));
        chunk.emit(self.store_var(&res_v));
        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.emit(self.load_var(&res_v));
    }

    fn compile_hof_reduce(&mut self, arr_expr: &Expr, init_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        self.anon_counter += 1;
        let arr_v  = format!("__hof_arr_{}", n);
        let fn_v   = format!("__hof_fn_{}", n);
        let idx_v  = format!("__hof_idx_{}", n);
        let len_v  = format!("__hof_len_{}", n);
        let item_v = format!("__hof_item_{}", n);
        let acc_v  = format!("__hof_acc_{}", n);

        self.compile_expr(arr_expr, chunk);
        chunk.emit(self.define_var(&arr_v));
        self.compile_expr(init_expr, chunk);
        chunk.emit(self.define_var(&acc_v));
        self.compile_expr(fn_expr, chunk);
        chunk.emit(self.define_var(&fn_v));
        chunk.emit(self.load_var(&arr_v));
        chunk.emit(Instruction::CallBuiltin("len".to_string(), 1));
        chunk.emit(self.define_var(&len_v));
        chunk.emit(Instruction::LoadConst(Value::Int(0)));
        chunk.emit(self.define_var(&idx_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        // acc = fn(acc, item)
        chunk.emit(self.load_var(&acc_v));
        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 2));
        chunk.emit(self.store_var(&acc_v));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.emit(self.load_var(&acc_v));
    }

    fn compile_hof_any(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::LoadConst(Value::Bool(false)));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        let skip = chunk.emit(Instruction::JumpIfFalse(0));
        chunk.emit(Instruction::LoadConst(Value::Bool(true)));
        chunk.emit(self.store_var(&res_v));
        // break — early exit
        let brk = chunk.emit(Instruction::Jump(0));
        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.patch(brk, Instruction::Jump(exit));
        chunk.emit(self.load_var(&res_v));
    }

    fn compile_hof_all(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::LoadConst(Value::Bool(true)));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        chunk.emit(Instruction::Not);
        let skip = chunk.emit(Instruction::JumpIfFalse(0));
        chunk.emit(Instruction::LoadConst(Value::Bool(false)));
        chunk.emit(self.store_var(&res_v));
        let brk = chunk.emit(Instruction::Jump(0));
        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.patch(brk, Instruction::Jump(exit));
        chunk.emit(self.load_var(&res_v));
    }

    fn compile_hof_count(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::LoadConst(Value::Int(0)));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        let skip = chunk.emit(Instruction::JumpIfFalse(0));
        chunk.emit(self.load_var(&res_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&res_v));
        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.emit(self.load_var(&res_v));
    }

    fn compile_hof_find(&mut self, arr_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        let (arr_v, fn_v, idx_v, len_v, item_v) = self.hof_setup(arr_expr, fn_expr, chunk);
        let res_v = format!("__hof_res_{}", n);

        chunk.emit(Instruction::LoadConst(Value::Nil));
        chunk.emit(self.define_var(&res_v));

        let loop_top = chunk.code.len();
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(self.load_var(&len_v));
        chunk.emit(Instruction::Less);
        let exit_jump = chunk.emit(Instruction::JumpIfFalse(0));

        chunk.emit(self.load_var(&arr_v));
        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::GetIndex);
        chunk.emit(self.define_var(&item_v));

        chunk.emit(self.load_var(&item_v));
        chunk.emit(Instruction::Call(fn_v.clone(), 1));
        let skip = chunk.emit(Instruction::JumpIfFalse(0));
        chunk.emit(self.load_var(&item_v));
        chunk.emit(self.store_var(&res_v));
        let brk = chunk.emit(Instruction::Jump(0));
        chunk.patch(skip, Instruction::JumpIfFalse(chunk.code.len()));

        chunk.emit(self.load_var(&idx_v));
        chunk.emit(Instruction::LoadConst(Value::Int(1)));
        chunk.emit(Instruction::Add);
        chunk.emit(self.store_var(&idx_v));
        chunk.emit(Instruction::Jump(loop_top));

        let exit = chunk.code.len();
        chunk.patch(exit_jump, Instruction::JumpIfFalse(exit));
        chunk.patch(brk, Instruction::Jump(exit));
        chunk.emit(self.load_var(&res_v));
    }

    // withLock(m, fn) — acquire mutex, call fn(current_value), release with result, return result
    // Expands inline: no sub-frame, no closure — just lock/DynCall/unlock in sequence.
    fn compile_hof_with_lock(&mut self, mutex_expr: &Expr, fn_expr: &Expr, chunk: &mut Chunk) {
        let n = self.anon_counter;
        self.anon_counter += 1;
        let m_v   = format!("__wl_m_{}", n);
        let fn_v  = format!("__wl_fn_{}", n);
        let cur_v = format!("__wl_cur_{}", n);
        let res_v = format!("__wl_res_{}", n);

        self.compile_expr(mutex_expr, chunk);
        chunk.emit(self.define_var(&m_v));
        self.compile_expr(fn_expr, chunk);
        chunk.emit(self.define_var(&fn_v));

        // cur = lock(m)
        chunk.emit(self.load_var(&m_v));
        chunk.emit(Instruction::CallBuiltin("lock".to_string(), 1));
        chunk.emit(self.define_var(&cur_v));

        // res = fn(cur)  — DynCall: push arg first, then callee
        chunk.emit(self.load_var(&cur_v));
        chunk.emit(self.load_var(&fn_v));
        chunk.emit(Instruction::DynCall(1));
        chunk.emit(self.define_var(&res_v));

        // unlock(m, res)
        chunk.emit(self.load_var(&m_v));
        chunk.emit(self.load_var(&res_v));
        chunk.emit(Instruction::CallBuiltin("unlock".to_string(), 2));
        chunk.emit(Instruction::Pop); // discard nil return from unlock

        chunk.emit(self.load_var(&res_v));
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(name,
        "str" | "int" | "float" | "bool" | "type" |
        "upper" | "lower" | "len" | "contains" | "trim" |
        "startsWith" | "endsWith" | "ord" | "chr" | "substr" |
        "abs" | "max" | "min" | "sqrt" | "floor" | "ceil" | "round" |
        "pow" | "mod" | "random" | "input" |
        "push" | "pop" | "slice" | "concat" | "zip" | "reverse" | "sort" |
        "find" | "sum" | "product" |
        "split" | "join" | "replace" |
        "keys" | "values" | "hasKey" | "setKey" | "delete" | "mergeMap" |
        "isInt" | "isFloat" | "isString" | "isBool" | "isArray" | "isHashmap" | "isNil" |
        "print" | "println" | "printn" | "readFile" | "writeFile" |
        "channel" | "send" | "recv" | "tryRecv" | "close" |
        "ticker" | "timeout" |
        "mutex" | "lock" | "unlock" |
        "wait" |
        "count" | "any" | "all" |
        "clock" | "make_array"
    )
}

pub fn compile(stmts: &[Expr]) -> Chunk {
    let mut compiler = Compiler::new();
    let mut chunk = Chunk::new();
    for stmt in stmts {
        compiler.compile_stmt(stmt, &mut chunk);
    }
    chunk
}

// Like compile() but leaves the last value expression on the stack (no Pop) for REPL auto-print.
pub fn compile_repl(stmts: &[Expr]) -> Chunk {
    let mut compiler = Compiler::new();
    let mut chunk = Chunk::new();
    let n = stmts.len();
    for (i, stmt) in stmts.iter().enumerate() {
        if i == n - 1 && Compiler::is_value_expr(stmt) {
            compiler.compile_expr(stmt, &mut chunk);
        } else {
            compiler.compile_stmt(stmt, &mut chunk);
        }
    }
    chunk
}
