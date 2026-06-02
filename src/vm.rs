// vm.rs — bytecode virtual machine. The second half of the VM backend.
//
// The VM executes the flat Instruction list produced by compiler.rs.
// It maintains two core data structures:
//
//   Value stack — operands and results. Instructions push/pop values here.
//     ADD pops two values, pushes their sum. LoadConst(42) pushes 42. etc.
//
//   Call stack (frames: Vec<CallFrame>) — one frame per active function call.
//     Each frame has its own instruction pointer (ip) and local variables.
//     Calling a function pushes a new frame. Return pops it and resumes the caller.
//
// Key types:
//
//   CallFrame — represents one level of the call stack:
//     chunk: the bytecode of this function (Rc so multiple frames can share it)
//     ip: index of the NEXT instruction to execute in chunk.code
//     locals: this call's local variables (params + let bindings inside the function)
//
//   TryHandler — saved context for the nearest active try block:
//     frame_depth: how many frames were on the call stack when we entered the try
//     stack_depth: how many values were on the value stack
//     catch_ip: instruction index to jump to in the current chunk on throw
//     On throw, frames and stack are truncated back to these depths, then execution
//     resumes at catch_ip with the thrown value on the stack.
//
//   pending_throw — runtime errors (bad index, wrong type, etc.) set this instead of
//     panicking, when a try handler is active. Checked at the TOP of the main loop so
//     the handler takes effect before any more instructions execute.
//
//   functions map — stores named functions by name for Call(name, N) lookup
//   closures map — stores lambdas (and named functions re-registered as first-class values)
//     keyed by u64 id; holds (params, chunk, captured_env)
//     When a named function is defined, it gets registered in BOTH maps so it can be
//     called by name AND passed as a value (e.g. reduce(arr, init, my_fn))
//
// Instruction execution:
//   Each iteration of the main loop: read instr = chunk.code[ip], clone it (releases the
//   immutable borrow so we can mutate frames/stack in the match arms), then match on it.
//   Normal instructions increment ip at the bottom of the loop.
//   Jump instructions set ip = target and `continue` (skip the bottom increment).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use crate::compiler::{Chunk, Instruction};
use crate::evaluator::{format_value, Value, ChannelInner, deep_clone, make_array, make_map, collect_cycles};
use crossbeam_channel::{TryRecvError, Receiver};

// One active function call on the call stack.
struct CallFrame {
    chunk:      Arc<Chunk>,             // the compiled bytecode of this function
    ip:         usize,                  // index of the next instruction to execute
    locals:     HashMap<String, Value>, // local variables for this call (params + let bindings)
    closure_id: Option<u64>,            // Some(id) for lambdas — write locals back to closures map on return
    deferred:   Vec<Arc<Chunk>>,        // sub-chunks pushed by `defer`; run LIFO on return
}

// Saved state for the nearest active try block.
// On throw, the VM rewinds to exactly this state so the catch block starts clean.
struct TryHandler {
    frame_depth: usize, // how many frames were on the call stack when try started
    stack_depth: usize, // how many values were on the value stack when try started
    catch_ip:    usize, // instruction index to jump to in the current chunk on throw
}

pub struct Vm {
    stack:           Vec<Value>,                                                      // operand stack
    globals:         HashMap<String, Value>,                                          // top-level variables
    functions:       HashMap<String, (Vec<String>, Arc<Chunk>, bool)>,                  // named functions — (params, chunk, variadic)
    closures:        HashMap<u64, (Vec<String>, Arc<Chunk>, HashMap<String, Value>)>,  // lambdas + named-funcs-as-values
    closure_counter: u64,          // incremented each time a new closure/lambda is registered
    pending_throw:   Option<Value>, // runtime error waiting to be caught; checked at top of main loop
    methods:         HashMap<(String, String), (Vec<String>, Arc<Chunk>)>,             // (type_name, method_name) → impl body
    current_line:    usize,         // most recently seen SetLine value — used in error messages
}

// Terminate with a VM error — sets LAST_ERROR so catch_unwind in spawned threads can read it,
// then panics instead of process::exit so it can be caught.
fn vm_panic(msg: &str) -> ! {
    crate::error::LAST_ERROR.with(|e| *e.borrow_mut() = msg.to_string());
    let in_try = crate::error::TRY_DEPTH.with(|d| d.get() > 0);
    if !in_try { eprintln!("{}", msg); }
    std::panic::panic_any("nova_error")
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            stack:           Vec::new(),
            globals:         HashMap::new(),
            functions:       HashMap::new(),
            closures:        HashMap::new(),
            closure_counter: 0,
            pending_throw:   None,
            methods:         HashMap::new(),
            current_line:    0,
            // NOTE: the VM has no cycle collector. collect_cycles() lives in evaluator.rs
            // and is only called on the --tree path. This is intentional for now:
            // CoW (copy-on-write on array/hashmap writes) prevents cycles from forming
            // in practice, so cycles_collected stays 0 in all tested programs.
            // Phase 10 (concurrency) requires a full GC redesign with multiple stack roots
            // anyway — porting the single-threaded collector here would just be thrown away.
        }
    }

    pub fn run(&mut self, top_chunk: Chunk) -> Option<Value> {
        let mut frames: Vec<CallFrame> = vec![
            CallFrame { chunk: Arc::new(top_chunk), ip: 0, locals: HashMap::new(), closure_id: None, deferred: vec![] }
        ];
        let mut try_handlers: Vec<TryHandler> = Vec::new();

        loop {
            // pending throw check
            // Runtime errors inside instruction handlers (bad index, wrong type, etc.)
            // set pending_throw instead of panicking when a try handler is active.
            // We check it at the TOP of every iteration so the handler fires immediately,
            // before any further instructions execute on the corrupted state.
            if let Some(thrown) = self.pending_throw.take() {
                // Attach line number to VM-internal runtime errors (string messages).
                // User `throw` statements bypass pending_throw entirely, so this only
                // affects errors the VM itself generates.
                let thrown = match thrown {
                    Value::Str(s) => Value::Str(format!("Error on line {}: {}", self.current_line, s)),
                    other => other,
                };
                if let Some(handler) = try_handlers.pop() {
                    // unwind the call stack and value stack back to the state when try started
                    while frames.len() > handler.frame_depth { frames.pop(); }
                    self.stack.truncate(handler.stack_depth);
                    self.stack.push(thrown); // catch variable will be bound to this
                    frames.last_mut().unwrap().ip = handler.catch_ip; // jump to catch block
                } else {
                    vm_panic(&format!("{}", format_value(&thrown)));
                }
                continue;
            }

            // advance ip / end of function
            let ip = frames.last().unwrap().ip;
            if ip >= frames.last().unwrap().chunk.code.len() {
                // ran off the end of this function's bytecode — implicitly return nil
                let frame = frames.pop().unwrap();
                if !frame.deferred.is_empty() {
                    // same continuation approach as Instruction::Return
                    let mut cont = Chunk::new();
                    cont.emit(Instruction::LoadConst(Value::Nil));
                    cont.emit(Instruction::Return);
                    frames.push(CallFrame { chunk: Arc::new(cont), ip: 0, locals: HashMap::new(), closure_id: None, deferred: vec![] });
                    for d in frame.deferred.iter() {
                        frames.push(CallFrame { chunk: d.clone(), ip: 0, locals: frame.locals.clone(), closure_id: None, deferred: vec![] });
                    }
                } else if frames.is_empty() {
                    break; // top-level chunk finished — program done
                }
                continue;
            }

            // Clone the instruction before matching so we drop the immutable borrow on
            // frames.last() — without this, Rust won't let us mutably access frames inside
            // the match arms (you can't hold an immutable and mutable borrow simultaneously).
            let instr = frames.last().unwrap().chunk.code[ip].clone();

            match instr {
                // constants & globals
                Instruction::LoadConst(v) => {
                    self.stack.push(v);
                }
                Instruction::LoadGlobal(name) => {
                    let v = self.globals.get(&name)
                        .unwrap_or_else(|| self.runtime_error(&format!("undefined variable '{}'", name)))
                        .clone();
                    self.stack.push(v);
                }
                Instruction::DefineGlobal(name) => {
                    let v = self.pop();
                    self.globals.insert(name, v);
                }
                Instruction::StoreGlobal(name) => {
                    if !self.globals.contains_key(&name) {
                        self.runtime_error(&format!("'{}' is not declared — use 'let {} = ...' first", name, name));
                    }
                    let v = self.pop();
                    self.globals.insert(name, v);
                }

                // locals
                Instruction::LoadLocal(name) => {
                    let v = if let Some(v) = frames.last().unwrap().locals.get(&name) {
                        v.clone()
                    } else if let Some(v) = self.globals.get(&name) {
                        v.clone()
                    } else {
                        self.runtime_error(&format!("undefined variable '{}'", name))
                    };
                    self.stack.push(v);
                }
                Instruction::DefineLocal(name) => {
                    let v = self.pop();
                    frames.last_mut().unwrap().locals.insert(name, v);
                }
                Instruction::StoreLocal(name) => {
                    let v = self.pop();
                    let frame = frames.last_mut().unwrap();
                    if frame.locals.contains_key(&name) {
                        frame.locals.insert(name, v);
                    } else if self.globals.contains_key(&name) {
                        self.globals.insert(name, v);
                    } else {
                        self.runtime_error(&format!("'{}' is not declared — use 'let {} = ...' first", name, name));
                    }
                }

                // arithmetic
                Instruction::Add => {
                    let (l, r) = self.pop2();
                    self.stack.push(Self::add(l, r));
                }
                Instruction::Sub => {
                    let (l, r) = self.pop2();
                    self.stack.push(Self::numeric_op(l, r, '-'));
                }
                Instruction::Mul => {
                    let (l, r) = self.pop2();
                    self.stack.push(Self::numeric_op(l, r, '*'));
                }
                Instruction::Div => {
                    let (l, r) = self.pop2();
                    if matches!((&l, &r), (Value::Int(_), Value::Int(0))) {
                        self.pending_throw = Some(Value::Str("division by zero".to_string()));
                        continue;
                    }
                    self.stack.push(Self::numeric_op(l, r, '/'));
                }
                Instruction::Mod => {
                    let (l, r) = self.pop2();
                    if matches!((&l, &r), (Value::Int(_), Value::Int(0))) {
                        self.pending_throw = Some(Value::Str("modulo by zero".to_string()));
                        continue;
                    }
                    self.stack.push(Self::numeric_op(l, r, '%'));
                }

                // bitwise
                Instruction::BitAnd => {
                    let (l, r) = self.pop2();
                    match (l, r) {
                        (Value::Int(a), Value::Int(b)) => self.stack.push(Value::Int(a & b)),
                        (l, r) => self.runtime_error(&format!("& requires int operands, got {:?} and {:?}", l, r)),
                    }
                }
                Instruction::BitOr => {
                    let (l, r) = self.pop2();
                    match (l, r) {
                        (Value::Int(a), Value::Int(b)) => self.stack.push(Value::Int(a | b)),
                        (l, r) => self.runtime_error(&format!("| requires int operands, got {:?} and {:?}", l, r)),
                    }
                }
                Instruction::BitXor => {
                    let (l, r) = self.pop2();
                    match (l, r) {
                        (Value::Int(a), Value::Int(b)) => self.stack.push(Value::Int(a ^ b)),
                        (l, r) => self.runtime_error(&format!("^ requires int operands, got {:?} and {:?}", l, r)),
                    }
                }
                Instruction::Shl => {
                    let (l, r) = self.pop2();
                    match (l, r) {
                        (Value::Int(a), Value::Int(b)) => self.stack.push(Value::Int(a << b)),
                        (l, r) => self.runtime_error(&format!("<< requires int operands, got {:?} and {:?}", l, r)),
                    }
                }
                Instruction::Shr => {
                    let (l, r) = self.pop2();
                    match (l, r) {
                        (Value::Int(a), Value::Int(b)) => self.stack.push(Value::Int(a >> b)),
                        (l, r) => self.runtime_error(&format!(">> requires int operands, got {:?} and {:?}", l, r)),
                    }
                }

                // comparisons
                Instruction::Equal    => { let (l, r) = self.pop2(); self.stack.push(Value::Bool( Self::vals_eq(&l, &r))); }
                Instruction::NotEqual => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(!Self::vals_eq(&l, &r))); }
                Instruction::Less     => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::compare(&l, &r, '<'))); }
                Instruction::LessEq   => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::compare(&l, &r, 'l'))); }
                Instruction::Greater  => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::compare(&l, &r, '>'))); }
                Instruction::GreaterEq=> { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::compare(&l, &r, 'g'))); }

                // logical
                Instruction::And => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::truthy(&l) && Self::truthy(&r))); }
                Instruction::Or  => { let (l, r) = self.pop2(); self.stack.push(Value::Bool(Self::truthy(&l) || Self::truthy(&r))); }
                Instruction::Not => { let v = self.pop(); self.stack.push(Value::Bool(!Self::truthy(&v))); }

                // control flow
                Instruction::JumpIfFalse(target) => {
                    let v = self.pop();
                    if !Self::truthy(&v) {
                        frames.last_mut().unwrap().ip = target;
                        continue;
                    }
                }
                Instruction::JumpIfNotNil(target) => {
                    // Peek without popping: if TOS != nil, jump (leaving TOS on stack)
                    if !matches!(self.stack.last().unwrap(), Value::Nil) {
                        frames.last_mut().unwrap().ip = target;
                        continue;
                    }
                    // TOS is nil — fall through; the Pop + right-side will follow in the bytecode
                }
                Instruction::Jump(target) => {
                    frames.last_mut().unwrap().ip = target;
                    continue;
                }

                // named function definition
                Instruction::DefineFunc(name, params, func_chunk, variadic) => {
                    // Dual registration:
                    //   functions map — for Call(name, N) which looks here first (fast path)
                    //   closures map + globals — so the function can be loaded as a first-class
                    //   value: `reduce(arr, init, my_fn)` compiles my_fn as LoadGlobal("my_fn"),
                    //   which must find a Value in globals. Without this, passing named functions
                    //   to higher-order functions panics with "undefined variable 'my_fn'".
                    self.closure_counter += 1;
                    let id = self.closure_counter;
                    self.closures.insert(id, (params.clone(), func_chunk.clone(), HashMap::new()));
                    self.globals.insert(name.clone(), Value::VmFunc(id)); // makes fn loadable as value
                    self.functions.insert(name, (params, func_chunk, variadic)); // makes fn callable by name
                }

                // closure creation
                Instruction::MakeFunc(params, func_chunk) => {
                    self.closure_counter += 1;
                    let id = self.closure_counter;
                    // Capture a snapshot of all visible variables: globals first, locals override.
                    // This matches the tree-walker's env.clone() semantics so each closure
                    // sees the correct values of loop variables at creation time.
                    let mut captured = self.globals.clone();
                    for (k, v) in &frames.last().unwrap().locals {
                        captured.insert(k.clone(), v.clone());
                    }
                    self.closures.insert(id, (params, func_chunk, captured));
                    self.stack.push(Value::VmFunc(id));
                }

                // call named function (or variable holding a VmFunc)
                Instruction::Call(name, argc) => {
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc { args.push(self.pop()); }
                    args.reverse();

                    // Resolve the callee. Returns Some((params, chunk, captured, closure_id,
                    // variadic)) on success, or None if we already handled it (EnumConstructor)
                    // or set pending_throw (catchable errors).
                    let resolved = if let Some((p, c, var)) = self.functions.get(&name) {
                        Some((p.clone(), c.clone(), HashMap::new(), None, *var))
                    } else {
                        let var_val = {
                            let frame = frames.last().unwrap();
                            frame.locals.get(&name).cloned()
                                .or_else(|| self.globals.get(&name).cloned())
                        };
                        match var_val {
                            None => {
                                self.pending_throw = Some(Value::Str(format!("undefined function '{}'", name)));
                                frames.last_mut().unwrap().ip += 1;
                                continue;
                            }
                            Some(Value::VmFunc(id)) => {
                                let (p, c, cap) = self.closures.get(&id)
                                    .unwrap_or_else(|| self.runtime_error(&format!("closure {} not found", id)))
                                    .clone();
                                Some((p, c, cap, Some(id), false))
                            }
                            Some(Value::EnumConstructor { enum_name, variant, arity }) => {
                                if argc != arity {
                                    self.pending_throw = Some(Value::Str(format!(
                                        "enum constructor '{}.{}' expects {} args, got {}", enum_name, variant, arity, argc)));
                                    frames.last_mut().unwrap().ip += 1;
                                    continue;
                                }
                                self.stack.push(Value::EnumVariant { enum_name, variant, payload: args });
                                frames.last_mut().unwrap().ip += 1;
                                continue;
                            }
                            Some(other) => {
                                self.pending_throw = Some(Value::Str(format!(
                                    "'{}' is not callable (got {})", name, Self::type_of(&other))));
                                frames.last_mut().unwrap().ip += 1;
                                continue;
                            }
                        }
                    };
                    let (params, func_chunk, captured, call_closure_id, variadic) = resolved.unwrap();

                    let mut locals = captured;
                    if variadic {
                        let regular = params.len().saturating_sub(1);
                        if argc < regular {
                            self.pending_throw = Some(Value::Str(format!(
                                "'{}' expects at least {} args, got {}", name, regular, argc)));
                            frames.last_mut().unwrap().ip += 1;
                            continue;
                        }
                        for (p, a) in params[..regular].iter().zip(args.iter()) {
                            locals.insert(p.clone(), a.clone());
                        }
                        // pack remaining args into an array for the last param
                        let rest: Vec<Value> = args[regular..].to_vec();
                        locals.insert(params[regular].clone(), make_array(rest));
                    } else {
                        if argc > params.len() {
                            self.pending_throw = Some(Value::Str(format!(
                                "'{}' expects at most {} args, got {}", name, params.len(), argc)));
                            frames.last_mut().unwrap().ip += 1;
                            continue;
                        }
                        for (p, a) in params.iter().zip(args.iter()) {
                            locals.insert(p.clone(), a.clone());
                        }
                        for p in params.iter().skip(argc) {
                            locals.insert(p.clone(), Value::Nil);
                        }
                    }

                    frames.last_mut().unwrap().ip += 1;
                    frames.push(CallFrame { chunk: func_chunk, ip: 0, locals, closure_id: call_closure_id, deferred: vec![] });
                    continue;
                }

                // dynamic call: VmFunc popped from stack
                Instruction::DynCall(argc) => {
                    let callee = self.pop();
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc { args.push(self.pop()); }
                    args.reverse();

                    if let Value::EnumConstructor { enum_name, variant, arity } = callee {
                        if args.len() != arity {
                            self.runtime_error(&format!("enum constructor '{}.{}' expects {} args, got {}", enum_name, variant, arity, args.len()));
                        }
                        self.stack.push(Value::EnumVariant { enum_name, variant, payload: args });
                        frames.last_mut().unwrap().ip += 1;
                        continue;
                    }

                    let id = match callee {
                        Value::VmFunc(id) => id,
                        other => self.runtime_error(&format!("attempt to call a non-function value (got {:?})", other)),
                    };

                    let (params, func_chunk, captured) = self.closures.get(&id)
                        .unwrap_or_else(|| self.runtime_error(&format!("closure {} not found", id)))
                        .clone();

                    if argc > params.len() {
                        self.runtime_error(&format!("function expects at most {} args, got {}", params.len(), argc));
                    }

                    let mut locals = captured;
                    for (p, a) in params.iter().zip(args.iter()) {
                        locals.insert(p.clone(), a.clone());
                    }
                    for p in params.iter().skip(argc) {
                        locals.insert(p.clone(), Value::Nil);
                    }

                    frames.last_mut().unwrap().ip += 1;
                    frames.push(CallFrame { chunk: func_chunk, ip: 0, locals, closure_id: Some(id), deferred: vec![] });
                    continue;
                }

                Instruction::Return => {
                    let val = self.pop();
                    let frame = frames.pop().unwrap();
                    // write back any mutations to the closure's captured env so they persist
                    if let Some(id) = frame.closure_id {
                        if let Some(entry) = self.closures.get_mut(&id) {
                            let keys: Vec<String> = entry.2.keys().cloned().collect();
                            for k in keys {
                                if let Some(v) = frame.locals.get(&k) {
                                    entry.2.insert(k, v.clone());
                                }
                            }
                        }
                    }
                    if !frame.deferred.is_empty() {
                        // Build a continuation chunk that will push the return value and return
                        // after all deferred chunks have run.
                        let mut cont = Chunk::new();
                        cont.emit(Instruction::LoadConst(val));
                        cont.emit(Instruction::Return);
                        // Push continuation first (runs last), then deferred chunks in LIFO order
                        // (last-pushed defer runs first, so push them in original order — last = top)
                        frames.push(CallFrame { chunk: Arc::new(cont), ip: 0, locals: HashMap::new(), closure_id: None, deferred: vec![] });
                        for d in frame.deferred.iter() {
                            frames.push(CallFrame { chunk: d.clone(), ip: 0, locals: frame.locals.clone(), closure_id: None, deferred: vec![] });
                        }
                    } else {
                        self.stack.push(val);
                        if frames.is_empty() { break; }
                    }
                    continue;
                }

                // collections
                Instruction::MakeArray(n) => {
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n { items.push(self.pop()); }
                    items.reverse();
                    self.stack.push(make_array(items));
                }
                Instruction::MakeHashMap(n) => {
                    let mut pairs = Vec::with_capacity(n);
                    for _ in 0..n {
                        let v = self.pop();
                        let k = self.pop();
                        pairs.push((k, v));
                    }
                    pairs.reverse();
                    self.stack.push(make_map(pairs));
                }
                Instruction::GetIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let result = match (obj, &idx) {
                        (Value::Array(arr), Value::Int(n)) => {
                            let len = arr.lock().unwrap().len() as i64;
                            let i = if *n < 0 { len + n } else { *n };
                            if i < 0 || i >= len {
                                self.pending_throw = Some(Value::Str(
                                    format!("index out of bounds: {} (len {})", n, len)
                                ));
                                continue;
                            }
                            arr.lock().unwrap()[i as usize].clone()
                        }
                        (Value::Str(s), Value::Int(n)) => {
                            let chars: Vec<char> = s.chars().collect();
                            let len = chars.len() as i64;
                            let i = if *n < 0 { len + n } else { *n };
                            if i < 0 || i >= len {
                                self.pending_throw = Some(Value::Str(
                                    format!("string index out of bounds: {} (len {})", n, len)
                                ));
                                continue;
                            }
                            Value::Str(chars[i as usize].to_string())
                        }
                        (Value::HashMap(map), key) => {
                            map.lock().unwrap().iter()
                                .find(|(k, _)| Self::vals_eq(k, &key))
                                .map(|(_, v)| v.clone())
                                .unwrap_or(Value::Nil)
                        }
                        (obj, idx) => {
                            self.pending_throw = Some(Value::Str(
                                format!("cannot index {} with {}", format_value(&obj), format_value(&idx))
                            ));
                            continue;
                        }
                    };
                    self.stack.push(result);
                }
                Instruction::GetIndexOrNil => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let result = match (obj, &idx) {
                        (Value::Array(arr), Value::Int(n)) => {
                            let len = arr.lock().unwrap().len() as i64;
                            let i = if *n < 0 { len + n } else { *n };
                            if i < 0 || i >= len {
                                Value::Nil
                            } else {
                                arr.lock().unwrap()[i as usize].clone()
                            }
                        }
                        (Value::HashMap(map), key) => {
                            map.lock().unwrap().iter()
                                .find(|(k, _)| Self::vals_eq(k, &key))
                                .map(|(_, v)| v.clone())
                                .unwrap_or(Value::Nil)
                        }
                        _ => Value::Nil,
                    };
                    self.stack.push(result);
                }
                Instruction::SetIndex(name) => {
                    let idx = self.pop();
                    let val = self.pop();
                    let container = {
                        let frame = frames.last().unwrap();
                        if let Some(v) = frame.locals.get(&name) { v.clone() }
                        else if let Some(v) = self.globals.get(&name) { v.clone() }
                        else { self.runtime_error(&format!("undefined variable '{}'", name)) }
                    };
                    match container {
                        Value::Array(arr) => {
                            let arr = if Arc::strong_count(&arr) > 1 {
                                let cow = make_array(arr.lock().unwrap().clone());
                                let new_rc = if let Value::Array(rc) = &cow { rc.clone() } else { unreachable!() };
                                Self::store_named(&mut frames, &mut self.globals, &name, cow);
                                new_rc
                            } else { arr };
                            let len = arr.lock().unwrap().len() as i64;
                            let i = match &idx {
                                Value::Int(n) => {
                                    let i = if *n < 0 { len + n } else { *n };
                                    if i < 0 || i >= len {
                                        self.pending_throw = Some(Value::Str(
                                            format!("index out of bounds: {} (len {})", n, len)
                                        ));
                                        continue;
                                    }
                                    i as usize
                                }
                                _ => {
                                    self.pending_throw = Some(Value::Str("array index must be an integer".to_string()));
                                    continue;
                                }
                            };
                            arr.lock().unwrap()[i] = val;
                        }
                        Value::HashMap(map) => {
                            let map = if Arc::strong_count(&map) > 1 {
                                let cow = make_map(map.lock().unwrap().clone());
                                let new_rc = if let Value::HashMap(rc) = &cow { rc.clone() } else { unreachable!() };
                                Self::store_named(&mut frames, &mut self.globals, &name, cow);
                                new_rc
                            } else { map };
                            let mut m = map.lock().unwrap();
                            if let Some(entry) = m.iter_mut().find(|(k, _)| Self::vals_eq(k, &idx)) {
                                entry.1 = val;
                            } else {
                                m.push((idx, val));
                            }
                        }
                        other => {
                            self.pending_throw = Some(Value::Str(
                                format!("cannot index-assign into {}", format_value(&other))
                            ));
                            continue;
                        }
                    }
                }

                // exceptions
                Instruction::EnterTry(catch_ip) => {
                    try_handlers.push(TryHandler {
                        frame_depth: frames.len(),
                        stack_depth: self.stack.len(),
                        catch_ip,
                    });
                }
                Instruction::ExitTry => {
                    try_handlers.pop();
                }
                Instruction::Throw => {
                    let thrown = self.pop();
                    if let Some(handler) = try_handlers.pop() {
                        while frames.len() > handler.frame_depth { frames.pop(); }
                        self.stack.truncate(handler.stack_depth);
                        self.stack.push(thrown);
                        frames.last_mut().unwrap().ip = handler.catch_ip;
                        continue;
                    } else {
                        vm_panic(&format!("Error: {}", format_value(&thrown)));
                    }
                }

                // built-ins
                Instruction::CallBuiltin(name, argc) => {
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc { args.push(self.pop()); }
                    args.reverse();
                    match Self::call_builtin(&name, args) {
                        Ok(result) => self.stack.push(result),
                        Err(msg) => {
                            self.pending_throw = Some(Value::Str(msg));
                            continue;
                        }
                    }
                }

                // I/O
                Instruction::Print  => { let v = self.pop(); println!("{}", format_value(&v)); }
                Instruction::Printn => { let v = self.pop(); print!("{}", format_value(&v)); }

                // structs
                Instruction::MakeStruct(name, field_names) => {
                    let n = field_names.len();
                    let vals: Vec<Value> = self.stack.drain(self.stack.len() - n..).collect();
                    let mut map = std::collections::HashMap::new();
                    for (fname, fval) in field_names.iter().zip(vals) {
                        map.insert(fname.clone(), fval);
                    }
                    self.stack.push(Value::Struct {
                        name: name.clone(),
                        fields: Arc::new(Mutex::new(map)),
                    });
                }

                Instruction::GetField(field) => {
                    let obj = self.pop();
                    match obj {
                        Value::Struct { fields, .. } => {
                            let val = fields.lock().unwrap().get(field.as_str()).cloned();
                            match val {
                                Some(v) => self.stack.push(v),
                                None => {
                                    self.pending_throw = Some(Value::Str(format!("no field '{}' on struct", field)));
                                    continue;
                                }
                            }
                        }
                        Value::EnumDef { name: enum_name, variants } => {
                            if let Some(&(_, arity)) = variants.iter().find(|(v, _)| v == field.as_str()) {
                                if arity == 0 {
                                    self.stack.push(Value::EnumVariant {
                                        enum_name,
                                        variant: field.clone(),
                                        payload: vec![],
                                    });
                                } else {
                                    self.stack.push(Value::EnumConstructor {
                                        enum_name,
                                        variant: field.clone(),
                                        arity,
                                    });
                                }
                            } else {
                                self.pending_throw = Some(Value::Str(format!("no variant '{}' on enum '{}'", field, enum_name)));
                                continue;
                            }
                        }
                        _ => {
                            self.pending_throw = Some(Value::Str("field access requires a struct or enum".to_string()));
                            continue;
                        }
                    }
                }

                Instruction::SetField(field) => {
                    let new_val = self.pop();
                    let obj = self.pop();
                    match obj {
                        Value::Struct { fields, .. } => {
                            fields.lock().unwrap().insert(field.clone(), new_val);
                            self.stack.push(Value::Nil);
                        }
                        _ => {
                            self.pending_throw = Some(Value::Str("field assignment requires a struct".to_string()));
                            continue;
                        }
                    }
                }

                Instruction::DefineMethod(type_name, method_name, params, method_chunk) => {
                    self.methods.insert((type_name, method_name), (params, method_chunk));
                }

                Instruction::CallMethod(method_name, argc) => {
                    // stack: [..., self_obj, arg1, arg2, ..., argN]
                    let mut args: Vec<Value> = self.stack.drain(self.stack.len() - argc..).collect();
                    let receiver = self.pop();
                    // EnumDef receiver: Shape.Circle(5) — act as constructor, not method call
                    if let Value::EnumDef { name: enum_name, variants } = &receiver {
                        if let Some(&(_, _arity)) = variants.iter().find(|(v, _)| v == &method_name) {
                            self.stack.push(Value::EnumVariant {
                                enum_name: enum_name.clone(),
                                variant: method_name.clone(),
                                payload: args,
                            });
                        } else {
                            self.pending_throw = Some(Value::Str(
                                format!("no variant '{}' on enum '{}'", method_name, enum_name)));
                        }
                        frames.last_mut().unwrap().ip += 1; // advance past CallMethod
                        continue;
                    }
                    let type_name = match &receiver {
                        Value::Struct { name, .. } => name.clone(),
                        Value::EnumVariant { enum_name, .. } => enum_name.clone(),
                        _ => {
                            self.pending_throw = Some(Value::Str("method calls are only supported on structs and enums".to_string()));
                            continue;
                        }
                    };
                    match self.methods.get(&(type_name.clone(), method_name.clone())).cloned() {
                        None => {
                            self.pending_throw = Some(Value::Str(
                                format!("no method '{}' on type '{}'", method_name, type_name)));
                            continue;
                        }
                        Some((params, method_chunk)) => {
                            let mut locals = HashMap::new();
                            locals.insert(params[0].clone(), receiver); // bind self
                            for (param, val) in params.iter().skip(1).zip(args.drain(..)) {
                                locals.insert(param.clone(), val);
                            }
                            frames.last_mut().unwrap().ip += 1; // advance caller past CallMethod
                            frames.push(CallFrame { chunk: method_chunk, ip: 0, locals, closure_id: None, deferred: vec![] });
                            continue; // skip the bottom ip++ so method starts at 0
                        }
                    }
                }

                // enums
                Instruction::CheckEnumVariant(enum_name, variant_name) => {
                    let val = self.pop();
                    let ok = match &val {
                        Value::EnumVariant { enum_name: en, variant: vn, .. } => {
                            en == &enum_name && vn == &variant_name
                        }
                        _ => false,
                    };
                    self.stack.push(Value::Bool(ok));
                }

                Instruction::GetEnumPayload(n) => {
                    let val = self.pop();
                    match val {
                        Value::EnumVariant { payload, .. } => {
                            self.stack.push(payload.into_iter().nth(n).unwrap_or(Value::Nil));
                        }
                        _ => {
                            self.pending_throw = Some(Value::Str("GetEnumPayload: not an enum variant".to_string()));
                            continue;
                        }
                    }
                }

                // stack management
                Instruction::Pop => {
                    self.pop();
                    // Between top-level statements the stack is empty and globals is the
                    // complete root set — safe to run the cycle collector here.
                    if frames.len() == 1 {
                        collect_cycles(&self.globals);
                    }
                }

                // concurrency
                Instruction::Defer(sub_chunk) => {
                    // push the sub-chunk onto the current frame's deferred list
                    frames.last_mut().unwrap().deferred.push(sub_chunk.clone());
                }

                Instruction::Spawn(arc_chunk) => {
                    let chunk_to_run = (*arc_chunk).clone();
                    // start with deep-cloned globals, then overlay current frame's locals
                    // so spawn can reference variables defined inside a function
                    let mut cloned_globals: HashMap<String, Value> = self.globals.iter()
                        .map(|(k, v)| (k.clone(), deep_clone(v)))
                        .collect();
                    for (k, v) in &frames.last().unwrap().locals {
                        cloned_globals.insert(k.clone(), deep_clone(v));
                    }
                    let cloned_functions = self.functions.clone();
                    let cloned_methods  = self.methods.clone();
                    let cloned_closures: HashMap<u64, (Vec<String>, Arc<Chunk>, HashMap<String, Value>)> =
                        self.closures.iter()
                            .map(|(id, (params, chunk, env))| {
                                let cloned_env = env.iter()
                                    .map(|(k, v)| (k.clone(), deep_clone(v)))
                                    .collect();
                                (*id, (params.clone(), chunk.clone(), cloned_env))
                            })
                            .collect();
                    let (s, r) = crossbeam_channel::bounded::<Result<Value, String>>(1);
                    rayon::spawn(move || {
                        crate::error::LAST_ERROR.with(|e| *e.borrow_mut() = String::new());
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut vm = Vm {
                                stack:           Vec::new(),
                                globals:         cloned_globals,
                                functions:       cloned_functions,
                                closures:        cloned_closures,
                                closure_counter: 0,
                                pending_throw:   None,
                                methods:         cloned_methods,
                                current_line:    0,
                            };
                            vm.run(chunk_to_run).unwrap_or(Value::Nil)
                        })).map_err(|_| crate::error::LAST_ERROR.with(|e| e.borrow().clone()));
                        let _ = s.send(result);
                    });
                    self.stack.push(Value::Task(Arc::new(r)));
                }

                Instruction::SpawnAll => {
                    let func_val = self.pop();
                    let arr_val  = self.pop();
                    let elements = match &arr_val {
                        Value::Array(arc) => arc.lock().unwrap().clone(),
                        _ => self.runtime_error("spawnAll(): first argument must be an array"),
                    };
                    let mut receivers: Vec<Arc<Receiver<Result<Value, String>>>> = Vec::new();
                    for elem in &elements {
                        // sub-chunk: push arg, push fn (DynCall expects args first, then callee)
                        let mut sub = Chunk::new();
                        sub.emit(Instruction::LoadGlobal("__sa_arg__".to_string()));
                        sub.emit(Instruction::LoadGlobal("__sa_fn__".to_string()));
                        sub.emit(Instruction::DynCall(1));
                        let mut cg: HashMap<String, Value> = self.globals.iter()
                            .map(|(k, v)| (k.clone(), deep_clone(v)))
                            .collect();
                        for (k, v) in &frames.last().unwrap().locals {
                            cg.insert(k.clone(), deep_clone(v));
                        }
                        cg.insert("__sa_arg__".to_string(), deep_clone(elem));
                        cg.insert("__sa_fn__".to_string(),  deep_clone(&func_val));
                        let cf = self.functions.clone();
                        let cm = self.methods.clone();
                        let cc: HashMap<u64, (Vec<String>, Arc<Chunk>, HashMap<String, Value>)> =
                            self.closures.iter()
                                .map(|(id, (params, chunk, env))| {
                                    let ce = env.iter().map(|(k, v)| (k.clone(), deep_clone(v))).collect();
                                    (*id, (params.clone(), chunk.clone(), ce))
                                }).collect();
                        let (s, r) = crossbeam_channel::bounded::<Result<Value, String>>(1);
                        rayon::spawn(move || {
                            crate::error::LAST_ERROR.with(|e| *e.borrow_mut() = String::new());
                            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let mut vm = Vm {
                                    stack: Vec::new(), globals: cg, functions: cf,
                                    closures: cc, closure_counter: 0, pending_throw: None,
                                    methods: cm, current_line: 0,
                                };
                                vm.run(sub).unwrap_or(Value::Nil)
                            })).map_err(|_| crate::error::LAST_ERROR.with(|e| e.borrow().clone()));
                            let _ = s.send(result);
                        });
                        receivers.push(Arc::new(r));
                    }
                    let mut results: Vec<Value> = Vec::new();
                    for r in receivers {
                        match r.recv() {
                            Ok(Ok(v))    => results.push(v),
                            Ok(Err(msg)) => self.runtime_error(&format!("spawnAll task threw: {}", msg)),
                            Err(_)       => self.runtime_error("spawnAll: task channel disconnected"),
                        }
                    }
                    self.stack.push(make_array(results));
                }

                Instruction::SelectJump(n, arm_starts, default_start) => {
                    // Pop N channels (pushed in arm order, popped in reverse)
                    let mut channel_arcs: Vec<Arc<ChannelInner>> = Vec::new();
                    for _ in 0..n {
                        match self.pop() {
                            Value::Channel(arc) => channel_arcs.push(arc),
                            _ => self.runtime_error("select: each case must be a channel"),
                        }
                    }
                    channel_arcs.reverse(); // restore original arm order
                    // Register one recv per channel with crossbeam Select
                    let mut sel = crossbeam_channel::Select::new();
                    let mut op_indices: Vec<usize> = Vec::new();
                    for arc in &channel_arcs {
                        op_indices.push(sel.recv(&arc.receiver));
                    }
                    if let Some(default_ip) = default_start {
                        // Non-blocking: try_select returns Err immediately if nothing is ready
                        match sel.try_select() {
                            Ok(oper) => {
                                let ready = oper.index();
                                let arm_idx = op_indices.iter().position(|&i| i == ready).unwrap();
                                let msg = oper.recv(&channel_arcs[arm_idx].receiver).unwrap_or(Value::Nil);
                                self.stack.push(msg);
                                frames.last_mut().unwrap().ip = arm_starts[arm_idx];
                            }
                            Err(_) => {
                                // Nothing ready — jump to default arm (no value pushed)
                                frames.last_mut().unwrap().ip = default_ip;
                            }
                        }
                    } else {
                        // Blocking: wait until a channel fires
                        let oper = sel.select();
                        let ready = oper.index();
                        let arm_idx = op_indices.iter().position(|&i| i == ready).unwrap();
                        let msg = oper.recv(&channel_arcs[arm_idx].receiver).unwrap_or(Value::Nil);
                        self.stack.push(msg);
                        frames.last_mut().unwrap().ip = arm_starts[arm_idx];
                    }
                    continue; // ip was set manually, don't increment
                }

                Instruction::SetLine(n) => { self.current_line = n; }
            }

            frames.last_mut().unwrap().ip += 1;
        }

        self.stack.pop() // last value on stack (Some for REPL expressions, None for file mode)
    }

    // Restore VM to a clean state after a REPL error.
    // Globals and functions are replaced with pre-error snapshots; stack and throw are cleared.
    pub fn restore(
        &mut self,
        globals: HashMap<String, Value>,
        functions: HashMap<String, (Vec<String>, Arc<crate::compiler::Chunk>, bool)>,
        methods: HashMap<(String, String), (Vec<String>, Arc<crate::compiler::Chunk>)>,
    ) {
        self.globals = globals;
        self.functions = functions;
        self.methods = methods;
        self.stack.clear();
        self.pending_throw = None;
    }

    pub fn snapshot(&self) -> (
        HashMap<String, Value>,
        HashMap<String, (Vec<String>, Arc<crate::compiler::Chunk>, bool)>,
        HashMap<(String, String), (Vec<String>, Arc<crate::compiler::Chunk>)>,
    ) {
        (self.globals.clone(), self.functions.clone(), self.methods.clone())
    }

    // private helpers

    fn runtime_error(&self, msg: &str) -> ! {
        let full = if self.current_line > 0 {
            format!("Error on line {}: {}", self.current_line, msg)
        } else {
            format!("Error: {}", msg)
        };
        vm_panic(&full)
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or_else(|| self.runtime_error("VM stack underflow"))
    }

    fn pop2(&mut self) -> (Value, Value) {
        let r = self.pop();
        let l = self.pop();
        (l, r)
    }

    fn truthy(v: &Value) -> bool {
        match v {
            Value::Bool(b) => *b,
            Value::Nil     => false,
            Value::Int(n)  => *n != 0,
            _              => true,
        }
    }

    fn vals_eq(a: &Value, b: &Value) -> bool {
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
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| Self::vals_eq(x, y))
            }
            (Value::HashMap(a), Value::HashMap(b)) => {
                let a = a.lock().unwrap();
                let b = b.lock().unwrap();
                a.len() == b.len() && a.iter().all(|(k, v)| {
                    b.iter().any(|(k2, v2)| Self::vals_eq(k, k2) && Self::vals_eq(v, v2))
                })
            }
            (Value::EnumVariant { enum_name: en1, variant: v1, payload: p1 },
             Value::EnumVariant { enum_name: en2, variant: v2, payload: p2 }) => {
                en1 == en2 && v1 == v2 && p1.len() == p2.len()
                    && p1.iter().zip(p2.iter()).all(|(x, y)| Self::vals_eq(x, y))
            }
            (Value::Struct { fields: fa, .. }, Value::Struct { fields: fb, .. }) => Arc::ptr_eq(fa, fb),
            _                                  => false,
        }
    }

    fn type_of(v: &Value) -> &'static str {
        match v {
            Value::Int(_)              => "int",
            Value::Float(_)            => "float",
            Value::Str(_)              => "string",
            Value::Bool(_)             => "bool",
            Value::Array(_)            => "array",
            Value::HashMap(_)          => "hashmap",
            Value::Nil                 => "nil",
            Value::Function { .. }
            | Value::VmFunc(_)         => "function",
            _                          => "unknown",
        }
    }

    fn bool_to_num(v: Value) -> Value {
        match v {
            Value::Bool(b) => Value::Int(if b { 1 } else { 0 }),
            other => other,
        }
    }

    fn add(l: Value, r: Value) -> Value {
        let l = Self::bool_to_num(l);
        let r = Self::bool_to_num(r);
        match (&l, &r) {
            (Value::Int(a),   Value::Int(b))   => Value::Int(a + b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Int(a),   Value::Float(b)) => Value::Float(*a as f64 + b),
            (Value::Float(a), Value::Int(b))   => Value::Float(a + *b as f64),
            (Value::Str(a),   Value::Str(b))   => Value::Str(format!("{}{}", a, b)),
            (Value::Array(a), Value::Array(b)) => {
                let mut combined = a.lock().unwrap().clone();
                combined.extend(b.lock().unwrap().clone());
                make_array(combined)
            }
            _ => vm_panic(&format!("Error: cannot add {} and {}", Self::type_of(&l), Self::type_of(&r))),
        }
    }

    fn numeric_op(l: Value, r: Value, op: char) -> Value {
        let l = Self::bool_to_num(l);
        let r = Self::bool_to_num(r);
        macro_rules! int_op {
            ($a:expr, $b:expr) => {
                match op {
                    '-' => Value::Int($a - $b),
                    '*' => Value::Int($a * $b),
                    '/' => { if $b == 0 { vm_panic("Error: division by zero") }
                             Value::Float($a as f64 / $b as f64) }
                    '%' => { if $b == 0 { vm_panic("Error: modulo by zero") }
                             Value::Int($a % $b) }
                    _   => unreachable!(),
                }
            };
        }
        macro_rules! flt_op {
            ($a:expr, $b:expr) => {
                match op {
                    '-' => Value::Float($a - $b),
                    '*' => Value::Float($a * $b),
                    '/' => Value::Float($a / $b),
                    '%' => Value::Float($a % $b),
                    _   => unreachable!(),
                }
            };
        }
        match (&l, &r) {
            (Value::Int(a),   Value::Int(b))   => int_op!(*a, *b),
            (Value::Float(a), Value::Float(b)) => flt_op!(*a, *b),
            (Value::Int(a),   Value::Float(b)) => flt_op!(*a as f64, *b),
            (Value::Float(a), Value::Int(b))   => flt_op!(*a, *b as f64),
            _ => vm_panic(&format!("Error: cannot apply '{}' to {} and {}", op, Self::type_of(&l), Self::type_of(&r))),
        }
    }

    fn compare(l: &Value, r: &Value, op: char) -> bool {
        macro_rules! cmp {
            ($a:expr, $b:expr) => {
                match op {
                    '<' => $a < $b,
                    '>' => $a > $b,
                    'l' => $a <= $b,
                    'g' => $a >= $b,
                    _   => unreachable!(),
                }
            };
        }
        match (l, r) {
            (Value::Int(a),   Value::Int(b))   => cmp!(a, b),
            (Value::Float(a), Value::Float(b)) => cmp!(a, b),
            (Value::Int(a),   Value::Float(b)) => cmp!(*a as f64, *b),
            (Value::Float(a), Value::Int(b))   => cmp!(*a, *b as f64),
            (Value::Str(a),   Value::Str(b))   => cmp!(a, b),
            _ => vm_panic(&format!("Error: cannot compare {} and {}", Self::type_of(l), Self::type_of(r))),
        }
    }

    fn store_named(frames: &mut Vec<CallFrame>, globals: &mut HashMap<String, Value>, name: &str, v: Value) {
        let frame = frames.last_mut().unwrap();
        if frame.locals.contains_key(name) {
            frame.locals.insert(name.to_string(), v);
        } else {
            globals.insert(name.to_string(), v);
        }
    }

    fn call_builtin(name: &str, args: Vec<Value>) -> Result<Value, String> {
        macro_rules! err {
            ($($t:tt)*) => { return Err(format!($($t)*)) };
        }
        Ok(match name {
            "str" => Value::Str(format_value(args.get(0).unwrap_or(&Value::Nil))),
            "int" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Int(*n),
                Value::Float(f) => Value::Int(*f as i64),
                Value::Bool(b)  => Value::Int(if *b { 1 } else { 0 }),
                Value::Str(s)   => match s.trim().parse::<i64>() {
                    Ok(n) => Value::Int(n),
                    Err(_) => err!("int(): cannot convert {:?}", s),
                },
                v => err!("int(): cannot convert {:?}", v),
            },
            "float" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Float(*n as f64),
                Value::Float(f) => Value::Float(*f),
                Value::Bool(b)  => Value::Float(if *b { 1.0 } else { 0.0 }),
                Value::Str(s)   => match s.trim().parse::<f64>() {
                    Ok(f) => Value::Float(f),
                    Err(_) => err!("float(): cannot convert {:?}", s),
                },
                v => err!("float(): cannot convert {:?}", v),
            },
            "bool" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Bool(b) => Value::Bool(*b),
                Value::Int(n)  => Value::Bool(*n != 0),
                Value::Nil     => Value::Bool(false),
                _              => Value::Bool(true),
            },
            "type" => {
                let v = args.into_iter().next().unwrap_or(Value::Nil);
                let t = match &v {
                    Value::Int(_)              => "int".to_string(),
                    Value::Float(_)            => "float".to_string(),
                    Value::Bool(_)             => "bool".to_string(),
                    Value::Str(_)              => "string".to_string(),
                    Value::Nil                 => "nil".to_string(),
                    Value::Array(_)            => "array".to_string(),
                    Value::HashMap(_)          => "hashmap".to_string(),
                    Value::VmFunc(_)           => "function".to_string(),
                    Value::StructDef { .. }    => "structdef".to_string(),
                    Value::Struct { name, .. } => format!("struct:{}", name),
                    Value::EnumDef { .. }      => "enumdef".to_string(),
                    Value::EnumVariant { enum_name, variant, .. } => format!("enum:{}:{}", enum_name, variant),
                    Value::EnumConstructor { .. } => "enumconstructor".to_string(),
                    Value::Channel(_)          => "channel".to_string(),
                    Value::Mutex(_)            => "mutex".to_string(),
                    Value::Task(_)             => "task".to_string(),
                    _                          => "unknown".to_string(),
                };
                Value::Str(t)
            }
            "upper" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(s) => Value::Str(s.to_uppercase()),
                v => err!("upper(): expected str, got {:?}", v),
            },
            "lower" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(s) => Value::Str(s.to_lowercase()),
                v => err!("lower(): expected str, got {:?}", v),
            },
            "len" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(s)      => Value::Int(s.chars().count() as i64),
                Value::Array(arr)  => Value::Int(arr.lock().unwrap().len() as i64),
                Value::HashMap(m)  => Value::Int(m.lock().unwrap().len() as i64),
                v => err!("len(): unsupported type {:?}", v),
            },
            "substr" => match (args.get(0), args.get(1), args.get(2)) {
                (Some(Value::Str(s)), Some(Value::Int(start)), Some(Value::Int(len))) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = (*start as usize).min(chars.len());
                    let end = (start + *len as usize).min(chars.len());
                    Value::Str(chars[start..end].iter().collect())
                }
                _ => err!("substr(): expected (str, int, int)"),
            },
            "contains" => match (args.get(0), args.get(1)) {
                (Some(Value::Str(h)), Some(Value::Str(n)))  => Value::Bool(h.contains(n.as_str())),
                (Some(Value::Array(arr)), Some(val))         => Value::Bool(arr.lock().unwrap().iter().any(|v| Self::vals_eq(v, val))),
                _ => err!("contains(): expected (str, str) or (array, value)"),
            },
            "push" => match (args.get(0), args.get(1)) {
                (Some(Value::Array(arr)), Some(val)) => {
                    let mut new_vec = arr.lock().unwrap().clone();
                    new_vec.push(val.clone());
                    make_array(new_vec)
                }
                _ => err!("push(): expected (array, value)"),
            },
            "pop" => match args.get(0) {
                Some(Value::Array(arr)) => {
                    let mut new_vec = arr.lock().unwrap().clone();
                    new_vec.pop();
                    make_array(new_vec)
                }
                _ => err!("pop(): expected array"),
            },
            "reverse" => match args.get(0) {
                Some(Value::Array(arr)) => {
                    let mut new_vec = arr.lock().unwrap().clone();
                    new_vec.reverse();
                    make_array(new_vec)
                }
                _ => err!("reverse(): expected array"),
            },
            "sort" => match args.get(0) {
                Some(Value::Array(arr)) => {
                    let mut new_vec = arr.lock().unwrap().clone();
                    new_vec.sort_by(|a, b| match (a, b) {
                        (Value::Int(x),   Value::Int(y))   => x.cmp(y),
                        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
                        (Value::Int(x),   Value::Float(y)) => (*x as f64).total_cmp(y),
                        (Value::Float(x), Value::Int(y))   => x.total_cmp(&(*y as f64)),
                        (Value::Str(x),   Value::Str(y))   => x.cmp(y),
                        _ => vm_panic("Error: sort(): only supports arrays of numbers or strings"),
                    });
                    make_array(new_vec)
                }
                _ => err!("sort(): expected array"),
            },
            "sum" => match args.get(0) {
                Some(Value::Array(arr)) => {
                    arr.lock().unwrap().iter().fold(Value::Int(0), |acc, v| Self::add(acc, v.clone()))
                }
                _ => err!("sum(): expected array"),
            },
            "product" => match args.get(0) {
                Some(Value::Array(arr)) => {
                    arr.lock().unwrap().iter().fold(Value::Int(1), |acc, v| Self::numeric_op(acc, v.clone(), '*'))
                }
                _ => err!("product(): expected array"),
            },
            "keys" => match args.get(0) {
                Some(Value::HashMap(map)) => {
                    let keys: Vec<Value> = map.lock().unwrap().iter().map(|(k, _)| k.clone()).collect();
                    make_array(keys)
                }
                _ => err!("keys(): expected hashmap"),
            },
            "values" => match args.get(0) {
                Some(Value::HashMap(map)) => {
                    let vals: Vec<Value> = map.lock().unwrap().iter().map(|(_, v)| v.clone()).collect();
                    make_array(vals)
                }
                _ => err!("values(): expected hashmap"),
            },
            "hasKey" => match (args.get(0), args.get(1)) {
                (Some(Value::HashMap(map)), Some(key)) => {
                    Value::Bool(map.lock().unwrap().iter().any(|(k, _)| Self::vals_eq(k, key)))
                }
                _ => err!("hasKey(): expected (hashmap, key)"),
            },
            "setKey" => match (args.get(0), args.get(1), args.get(2)) {
                (Some(Value::HashMap(map)), Some(key), Some(val)) => {
                    let mut new_pairs = map.lock().unwrap().clone();
                    if let Some(entry) = new_pairs.iter_mut().find(|(k, _)| Self::vals_eq(k, key)) {
                        entry.1 = val.clone();
                    } else {
                        new_pairs.push((key.clone(), val.clone()));
                    }
                    make_map(new_pairs)
                }
                _ => err!("setKey(): expected (hashmap, key, value)"),
            },
            "delete" => match (args.get(0), args.get(1)) {
                (Some(Value::HashMap(map)), Some(key)) => {
                    let new_pairs: Vec<(Value, Value)> = map.lock().unwrap().iter()
                        .filter(|(k, _)| !Self::vals_eq(k, key))
                        .cloned()
                        .collect();
                    make_map(new_pairs)
                }
                _ => err!("delete(): expected (hashmap, key)"),
            },
            "mergeMap" => match (args.get(0), args.get(1)) {
                (Some(Value::HashMap(base)), Some(Value::HashMap(extra))) => {
                    let mut result = base.lock().unwrap().clone();
                    for (k, v) in extra.lock().unwrap().iter() {
                        if let Some(entry) = result.iter_mut().find(|(rk, _)| Self::vals_eq(rk, k)) {
                            entry.1 = v.clone();
                        } else {
                            result.push((k.clone(), v.clone()));
                        }
                    }
                    make_map(result)
                }
                _ => err!("mergeMap(): expected (hashmap, hashmap)"),
            },
            "split" => match (args.get(0), args.get(1)) {
                (Some(Value::Str(s)), Some(Value::Str(delim))) => {
                    let parts: Vec<Value> = if delim.is_empty() {
                        s.chars().map(|c| Value::Str(c.to_string())).collect()
                    } else {
                        s.split(delim.as_str()).map(|p| Value::Str(p.to_string())).collect()
                    };
                    make_array(parts)
                }
                _ => err!("split(): expected (str, str)"),
            },
            "join" => match (args.get(0), args.get(1)) {
                (Some(Value::Array(arr)), Some(Value::Str(delim))) => {
                    let s = arr.lock().unwrap().iter().map(|v| format_value(v)).collect::<Vec<_>>().join(delim.as_str());
                    Value::Str(s)
                }
                _ => err!("join(): expected (array, str)"),
            },
            "trim" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(s) => Value::Str(s.trim().to_string()),
                v => err!("trim(): expected str, got {:?}", v),
            },
            "startsWith" => match (args.get(0), args.get(1)) {
                (Some(Value::Str(s)), Some(Value::Str(p))) => Value::Bool(s.starts_with(p.as_str())),
                _ => err!("startsWith(): expected (str, str)"),
            },
            "endsWith" => match (args.get(0), args.get(1)) {
                (Some(Value::Str(s)), Some(Value::Str(p))) => Value::Bool(s.ends_with(p.as_str())),
                _ => err!("endsWith(): expected (str, str)"),
            },
            "ord" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(s) => {
                    match s.chars().next() {
                        Some(ch) => Value::Int(ch as i64),
                        None => err!("ord(): empty string"),
                    }
                }
                v => err!("ord(): expected str, got {:?}", v),
            },
            "chr" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n) => {
                    match char::from_u32(*n as u32) {
                        Some(ch) => Value::Str(ch.to_string()),
                        None => err!("chr(): invalid codepoint {}", n),
                    }
                }
                v => err!("chr(): expected int, got {:?}", v),
            },
            "abs" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Int(n.abs()),
                Value::Float(f) => Value::Float(f.abs()),
                v => err!("abs(): expected number, got {:?}", v),
            },
            "max" => {
                if args.len() == 1 {
                    match args.into_iter().next().unwrap() {
                        Value::Array(arr) => {
                            let inner = arr.lock().unwrap().clone();
                            if inner.is_empty() { return Ok(Value::Nil); }
                            inner.into_iter().reduce(|a, b| if Self::compare(&b, &a, '>') { b } else { a }).unwrap()
                        }
                        v => err!("max(): single-arg form expects array, got {:?}", v),
                    }
                } else {
                    if args.len() < 2 { err!("max(): expected at least 2 args"); }
                    args.into_iter().reduce(|a, b| if Self::compare(&b, &a, '>') { b } else { a }).unwrap()
                }
            }
            "min" => {
                if args.len() == 1 {
                    match args.into_iter().next().unwrap() {
                        Value::Array(arr) => {
                            let inner = arr.lock().unwrap().clone();
                            if inner.is_empty() { return Ok(Value::Nil); }
                            inner.into_iter().reduce(|a, b| if Self::compare(&b, &a, '<') { b } else { a }).unwrap()
                        }
                        v => err!("min(): single-arg form expects array, got {:?}", v),
                    }
                } else {
                    if args.len() < 2 { err!("min(): expected at least 2 args"); }
                    args.into_iter().reduce(|a, b| if Self::compare(&b, &a, '<') { b } else { a }).unwrap()
                }
            }
            "sqrt" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Float((*n as f64).sqrt()),
                Value::Float(f) => Value::Float(f.sqrt()),
                v => err!("sqrt(): expected number, got {:?}", v),
            },
            "floor" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Int(*n),
                Value::Float(f) => Value::Int(f.floor() as i64),
                v => err!("floor(): expected number, got {:?}", v),
            },
            "ceil" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Int(n)   => Value::Int(*n),
                Value::Float(f) => Value::Int(f.ceil() as i64),
                v => err!("ceil(): expected number, got {:?}", v),
            },
            "pow" => match (args.get(0), args.get(1)) {
                (Some(Value::Int(b)),   Some(Value::Int(e)))   => Value::Float((*b as f64).powi(*e as i32)),
                (Some(Value::Float(b)), Some(Value::Int(e)))   => Value::Float(b.powi(*e as i32)),
                (Some(Value::Int(b)),   Some(Value::Float(e))) => Value::Float((*b as f64).powf(*e)),
                (Some(Value::Float(b)), Some(Value::Float(e))) => Value::Float(b.powf(*e)),
                _ => err!("pow(): expected (number, number)"),
            },
            "mod" => match (args.get(0), args.get(1)) {
                (Some(Value::Int(a)), Some(Value::Int(b))) => {
                    if *b == 0 { err!("mod(): division by zero"); }
                    Value::Int(((a % b) + b) % b)
                }
                _ => err!("mod(): expected (int, int)"),
            },
            "random" => {
                Value::Float(rand_f64())
            }
            "input" => {
                use std::io::{self, BufRead, Write};
                if !args.is_empty() {
                    if let Value::Str(prompt) = &args[0] {
                        print!("{}", prompt);
                        io::stdout().flush().unwrap();
                    }
                }
                let stdin = io::stdin();
                let line = stdin.lock().lines().next()
                    .unwrap_or_else(|| Ok(String::new()))
                    .unwrap_or_default();
                Value::Str(line)
            }
            // print/println/printn as callable functions (same behaviour as the Print instructions)
            "print" => {
                use crate::evaluator::format_value;
                let s = args.into_iter().map(|v| format_value(&v)).collect::<Vec<_>>().join(" ");
                println!("{}", s);
                Value::Nil
            }
            "println" => {
                use crate::evaluator::format_value;
                let s = args.into_iter().map(|v| format_value(&v)).collect::<Vec<_>>().join(" ");
                println!("{}", s);
                Value::Nil
            }
            "printn" => {
                use crate::evaluator::NEEDS_NEWLINE;
                use std::sync::atomic::Ordering;
                use crate::evaluator::format_value;
                let s = args.into_iter().map(|v| format_value(&v)).collect::<Vec<_>>().join(" ");
                print!("{}", s);
                use std::io::Write;
                std::io::stdout().flush().ok();
                NEEDS_NEWLINE.store(true, Ordering::Relaxed);
                Value::Nil
            }
            "round" => match (args.get(0), args.get(1)) {
                (Some(Value::Int(n)),   None) => Value::Int(*n),
                (Some(Value::Float(f)), None) => Value::Int(f.round() as i64),
                (Some(Value::Int(n)),   Some(Value::Int(d))) => {
                    let factor = 10f64.powi(*d as i32);
                    Value::Float(((*n as f64) * factor).round() / factor)
                }
                (Some(Value::Float(f)), Some(Value::Int(d))) => {
                    let factor = 10f64.powi(*d as i32);
                    Value::Float((f * factor).round() / factor)
                }
                _ => err!("round(): expected (number) or (number, int)"),
            },
            "slice" => match (args.get(0), args.get(1), args.get(2)) {
                (Some(Value::Array(arr)), Some(Value::Int(start)), Some(Value::Int(end))) => {
                    let inner = arr.lock().unwrap();
                    let s = (*start as usize).min(inner.len());
                    let e = (*end as usize).min(inner.len());
                    make_array(inner[s..e].to_vec())
                }
                _ => err!("slice(): expected (array, int, int)"),
            },
            "concat" => match (args.get(0), args.get(1)) {
                (Some(Value::Array(a)), Some(Value::Array(b))) => {
                    let mut result = a.lock().unwrap().clone();
                    result.extend(b.lock().unwrap().iter().cloned());
                    make_array(result)
                }
                _ => err!("concat(): expected (array, array)"),
            },
            "zip" => match (args.get(0), args.get(1)) {
                (Some(Value::Array(a)), Some(Value::Array(b))) => {
                    let pairs: Vec<Value> = a.lock().unwrap().iter().cloned()
                        .zip(b.lock().unwrap().iter().cloned())
                        .map(|(x, y)| make_array(vec![x, y]))
                        .collect();
                    make_array(pairs)
                }
                _ => err!("zip(): expected (array, array)"),
            },
            "replace" => match (args.get(0), args.get(1), args.get(2)) {
                (Some(Value::Str(s)), Some(Value::Str(from)), Some(Value::Str(to))) => {
                    Value::Str(s.replace(from.as_str(), to.as_str()))
                }
                _ => err!("replace(): expected (str, str, str)"),
            },
            "isInt"     => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Int(_))),
            "isFloat"   => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Float(_))),
            "isString"  => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Str(_))),
            "isBool"    => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Bool(_))),
            "isArray"   => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Array(_))),
            "isHashmap" => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::HashMap(_))),
            "isNil"     => Value::Bool(matches!(args.get(0).unwrap_or(&Value::Nil), Value::Nil)),
            "readFile" => match args.get(0).unwrap_or(&Value::Nil) {
                Value::Str(path) => match std::fs::read_to_string(path) {
                    Ok(content) => Value::Str(content),
                    Err(e) => err!("readFile(): {}", e),
                },
                v => err!("readFile(): expected str, got {:?}", v),
            },
            "writeFile" => match (args.get(0), args.get(1)) {
                (Some(Value::Str(path)), Some(Value::Str(content))) => {
                    std::fs::write(path, content).map_err(|e| format!("writeFile(): {}", e))?;
                    Value::Nil
                }
                _ => err!("writeFile(): expected (str, str)"),
            },
            // channel primitives
            "channel" => {
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }))
            }
            "send" => {
                match (args.get(0), args.get(1)) {
                    (Some(Value::Channel(arc)), Some(val)) => {
                        let sent = deep_clone(val);
                        arc.pending.lock().unwrap().push(sent.clone());
                        arc.sender.send(sent).map_err(|_| "send() failed: channel is closed".to_string())?;
                        Value::Nil
                    }
                    _ => err!("send() expects a channel and a value"),
                }
            }
            "recv" => {
                match args.get(0) {
                    Some(Value::Channel(arc)) => match arc.receiver.recv() {
                        Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                        Err(_) => Value::Nil,
                    },
                    _ => err!("recv() argument must be a channel"),
                }
            }
            "tryRecv" => {
                match args.get(0) {
                    Some(Value::Channel(arc)) => match arc.receiver.try_recv() {
                        Ok(v) => { arc.pending.lock().unwrap().remove(0); v }
                        Err(TryRecvError::Empty) => Value::Nil,
                        Err(TryRecvError::Disconnected) => Value::Nil,
                    },
                    _ => err!("tryRecv() argument must be a channel"),
                }
            }
            "close" => {
                match args.get(0) {
                    Some(Value::Channel(_)) => Value::Nil,
                    _ => err!("close() argument must be a channel"),
                }
            }
            "wait" => {
                match args.into_iter().next() {
                    Some(Value::Task(arc)) => match arc.recv() {
                        Ok(Ok(v))    => v,
                        Ok(Err(msg)) => err!("spawned task threw: {}", msg),
                        Err(_)       => err!("wait(): task channel disconnected"),
                    },
                    _ => err!("wait() argument must be a task"),
                }
            }
            "ticker" => {
                let ms = match args.into_iter().next() {
                    Some(Value::Int(n))   => n as u64,
                    Some(Value::Float(f)) => f as u64,
                    _ => err!("ticker(): expected integer milliseconds"),
                };
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                let tick_sender = s.clone();
                std::thread::spawn(move || {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        if tick_sender.send(Value::Bool(true)).is_err() { break; }
                    }
                });
                Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }))
            }
            "timeout" => {
                let ms = match args.into_iter().next() {
                    Some(Value::Int(n))   => n as u64,
                    Some(Value::Float(f)) => f as u64,
                    _ => err!("timeout(): expected integer milliseconds"),
                };
                let (s, r) = crossbeam_channel::unbounded::<Value>();
                let timeout_sender = s.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    let _ = timeout_sender.send(Value::Bool(true));
                });
                Value::Channel(Arc::new(ChannelInner { sender: s, receiver: r, pending: Mutex::new(vec![]) }))
            }

            "mutex" => {
                let initial = args.into_iter().next().unwrap_or(Value::Nil);
                let sent = deep_clone(&initial);
                let (s, r) = crossbeam_channel::bounded::<Value>(1);
                let inner = Arc::new(ChannelInner {
                    sender: s, receiver: r,
                    pending: Mutex::new(vec![sent.clone()]),
                });
                inner.sender.send(sent).map_err(|_| "mutex(): failed to initialise".to_string())?;
                Value::Mutex(inner)
            }
            "lock" => {
                match args.into_iter().next() {
                    Some(Value::Mutex(arc)) => match arc.receiver.recv() {
                        Ok(v)  => { arc.pending.lock().unwrap().remove(0); v }
                        Err(_) => err!("lock(): mutex is poisoned"),
                    },
                    _ => err!("lock() argument must be a mutex"),
                }
            }
            "unlock" => {
                let mut it = args.into_iter();
                match (it.next(), it.next()) {
                    (Some(Value::Mutex(arc)), Some(new_val)) => {
                        let sent = deep_clone(&new_val);
                        arc.pending.lock().unwrap().push(sent.clone());
                        arc.sender.send(sent)
                            .map_err(|_| "unlock(): mutex is full — was it already unlocked?".to_string())?;
                        Value::Nil
                    }
                    _ => err!("unlock() expects (mutex, value)"),
                }
            }
            "find" => err!("find(): higher-order builtins require --tree mode"),
            "count" => err!("count(): higher-order builtins require --tree mode"),
            "any" => err!("any(): higher-order builtins require --tree mode"),
            "all" => err!("all(): higher-order builtins require --tree mode"),
            "clock" => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                Value::Int(ms)
            }
            "make_array" => {
                let n = match args.get(0).unwrap_or(&Value::Nil) {
                    Value::Int(n) => *n,
                    _ => err!("make_array(): first argument must be int"),
                };
                if n < 0 { err!("make_array(): size must be non-negative"); }
                let default_val = args.into_iter().nth(1).unwrap_or(Value::Nil);
                let v: Vec<Value> = (0..n).map(|_| default_val.clone()).collect();
                make_array(v)
            }
            other => err!("unknown built-in function '{}'", other),
        })
    }
}

// Simple LCG-based pseudo-random float in [0, 1) — avoids pulling in a crate.
fn rand_f64() -> f64 {
    use std::time::SystemTime;
    static SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut s = SEED.load(std::sync::atomic::Ordering::Relaxed);
    if s == 0 {
        s = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64).unwrap_or(12345);
    }
    s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    SEED.store(s, std::sync::atomic::Ordering::Relaxed);
    (s >> 11) as f64 / (1u64 << 53) as f64
}
