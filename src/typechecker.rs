// typechecker.rs — gradual static type checker. Runs after parsing, before execution.
//
// Three-pass design:
//   Pass 1 — collect_signatures: harvest every fn's explicit type annotations into fn_sigs
//   Pass 2 — infer_return_types: fixed-point loop (up to 10 passes) that infers the return
//             type of unannotated functions from their bodies; repeated until stable so chains
//             of unannotated functions resolve regardless of declaration order
//   Pass 3 — check_stmts: walk every call site and return statement, compare inferred/actual
//             types against fn_sigs; collect all errors before stopping
//
// Key design choices:
//   Type::Unknown is compatible with everything — the checker never errors on ambiguous code
//   int passes where float is expected (widening) — no false errors on mixed arithmetic
//   Generics are erased at runtime — Type::Var exists only here, never in the evaluator/VM
//   Program does NOT start if any type errors are found
use crate::parser::Expr;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Str,
    Bool,
    // untyped array — we know it's an array but don't know what's inside
    Array,
    // typed array with a concrete or variable element type, e.g. [int] or [T]
    TypedArray(Box<Type>),
    HashMap,
    Nil,
    // Unknown means "we couldn't figure out the type" — treated as compatible with everything
    // so unannotated code never produces false positives
    Unknown,
    // type variable used for generics, e.g. T in fn map(arr: [T]) -> [T]
    // only exists at check time; erased before execution
    Var(String),
}

impl Type {
    fn name(&self) -> String {
        match self {
            Type::Int              => "int".to_string(),
            Type::Float            => "float".to_string(),
            Type::Str              => "string".to_string(),
            Type::Bool             => "bool".to_string(),
            Type::Array            => "array".to_string(),
            Type::TypedArray(inner) => format!("[{}]", inner.name()),
            Type::HashMap          => "hashmap".to_string(),
            Type::Nil              => "nil".to_string(),
            Type::Unknown          => "unknown".to_string(),
            Type::Var(n)           => n.clone(),
        }
    }
}

// Converts a string annotation from the source code (e.g. "int", "[T]") into a Type.
// Returns None if the string isn't a recognised type — callers treat that as Unknown.
fn parse_type(s: &str) -> Option<Type> {
    // [T] or [int] — strip the brackets and parse the inner type recursively
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        let inner_t = parse_type(inner).unwrap_or(Type::Unknown);
        return Some(Type::TypedArray(Box::new(inner_t)));
    }
    match s {
        "int"     => Some(Type::Int),
        "float"   => Some(Type::Float),
        "string"  => Some(Type::Str),
        "bool"    => Some(Type::Bool),
        "array"   => Some(Type::Array),
        "hashmap" => Some(Type::HashMap),
        "nil"     => Some(Type::Nil),
        // convention: uppercase first letter = type variable (T, U, K, V, etc.)
        _ if s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) => {
            Some(Type::Var(s.to_string()))
        }
        _ => None,
    }
}

// Returns true if a value of type `got` can be passed where `expected` is required.
// Lenient by design — the checker is gradual, so ambiguity is always allowed through.
fn compatible(expected: &Type, got: &Type) -> bool {
    // if either side is Unknown we have no information to reject it
    if matches!(expected, Type::Unknown) || matches!(got, Type::Unknown) {
        return true;
    }
    // type variables stand in for any concrete type — always compatible
    if matches!(expected, Type::Var(_)) || matches!(got, Type::Var(_)) {
        return true;
    }
    if expected == got { return true; }
    // int widens to float — passing an int where a float is expected is fine
    if matches!(expected, Type::Float) && matches!(got, Type::Int) { return true; }
    // untyped Array is compatible with any typed array and vice versa
    // this avoids errors when an empty array [] is passed to a fn expecting [int]
    if matches!(expected, Type::Array) && matches!(got, Type::TypedArray(_)) { return true; }
    if matches!(expected, Type::TypedArray(_)) && matches!(got, Type::Array) { return true; }
    // both are typed arrays — recurse so [int] vs [float] is handled by the same widening rule
    if let (Type::TypedArray(ei), Type::TypedArray(gi)) = (expected, got) {
        return compatible(ei, gi);
    }
    false
}

// Bind type variables in `pattern` to the corresponding position in `actual`.
// E.g. unify([T], [int]) inserts subs["T"] = Int.
// This is how generic calls get resolved: we look at the actual argument to figure out what T is.
fn unify(pattern: &Type, actual: &Type, subs: &mut HashMap<String, Type>) {
    match (pattern, actual) {
        // bare type variable — record the first concrete type we see for it
        // or_insert_with means the first call wins; later calls don't overwrite
        (Type::Var(name), _) => {
            subs.entry(name.clone()).or_insert_with(|| actual.clone());
        }
        // recurse into typed arrays so [T] vs [int] correctly binds T → Int
        (Type::TypedArray(pi), Type::TypedArray(ai)) => {
            unify(pi, ai, subs);
        }
        // anything else (int vs int, float vs bool, etc.) — nothing to bind
        _ => {}
    }
}

// Walk `ty` and replace any type variables with whatever they resolved to in `subs`.
// E.g. substitute([T], {"T": Int}) → [Int].
// Used after unify so we can turn a generic return type into a concrete one.
fn substitute(ty: &Type, subs: &HashMap<String, Type>) -> Type {
    match ty {
        // look up the variable; fall back to Unknown if it was never resolved
        Type::Var(name)         => subs.get(name).cloned().unwrap_or(Type::Unknown),
        // recurse into typed arrays so [T] → [int]
        Type::TypedArray(inner) => Type::TypedArray(Box::new(substitute(inner, subs))),
        // concrete types pass through unchanged
        other                   => other.clone(),
    }
}

// Everything the checker remembers about one function declaration.
#[derive(Debug, Clone)]
struct FnSig {
    // names of generic type parameters, e.g. ["T"] for fn foo<T>(...)
    type_params: Vec<String>,
    // one entry per parameter; None means the parameter had no type annotation
    param_types: Vec<Option<Type>>,
    // None means no return annotation AND inference hasn't figured it out yet
    return_type: Option<Type>,
}

// Walks one expression collecting explicit `return` statements into `found`.
// Does NOT recurse into nested fn declarations — each function's returns are its own.
// `checker` is read-only here so we can call infer() to get types.
fn collect_returns(expr: &Expr, locals: &HashMap<String, Type>, found: &mut Vec<Type>, checker: &Checker) {
    match expr {
        Expr::Line(_, inner) => collect_returns(inner, locals, found, checker),
        Expr::Return(val) => {
            let t = checker.infer(val, locals);
            // only record if we actually know the type — Unknown contributes nothing
            if !matches!(t, Type::Unknown) { found.push(t); }
        }
        // recurse into control-flow branches so returns inside an if/while/for are found
        Expr::If { then_block, else_block, .. } => {
            for s in then_block { collect_returns(s, locals, found, checker); }
            if let Some(b) = else_block { for s in b { collect_returns(s, locals, found, checker); } }
        }
        Expr::While { body, .. } | Expr::For { body, .. } | Expr::ForEnumerate { body, .. } | Expr::ForDestructure { body, .. } => {
            for s in body { collect_returns(s, locals, found, checker); }
        }
        Expr::Try { body, catch_body, .. } => {
            for s in body { collect_returns(s, locals, found, checker); }
            for s in catch_body { collect_returns(s, locals, found, checker); }
        }
        // stop here — an inner fn's `return` belongs to that fn, not the outer one
        Expr::Fn { .. } => {}
        _ => {}
    }
}

struct Checker {
    // maps function name → its signature; populated in pass 1, extended in pass 2
    fn_sigs: HashMap<String, FnSig>,
    // all errors collected during pass 3 — we gather them all before printing
    errors: Vec<String>,
    // tracks the current source line so error messages say something useful
    current_line: usize,
}

impl Checker {
    fn new() -> Self {
        Checker {
            fn_sigs: HashMap::new(),
            errors: Vec::new(),
            current_line: 0,
        }
    }

    fn error(&mut self, msg: String) {
        self.errors.push(format!("type error on line {}: {}", self.current_line, msg));
    }

    // Pass 1: walk all top-level statements and register fn signatures.
    // collect_sig recurses into nested fns so inner functions are also registered.
    fn collect_signatures(&mut self, stmts: &[Expr]) {
        for stmt in stmts { self.collect_sig(stmt); }
    }

    fn collect_sig(&mut self, expr: &Expr) {
        match expr {
            Expr::Line(line, inner) => {
                self.current_line = *line;
                self.collect_sig(inner);
            }
            Expr::Fn { name, params, return_type, body, type_params, .. } => {
                // convert each parameter's optional string annotation to Option<Type>
                let param_types = params.iter()
                    .map(|(_, t, _)| t.as_deref().and_then(parse_type))
                    .collect();
                let ret = return_type.as_deref().and_then(parse_type);
                self.fn_sigs.insert(name.clone(), FnSig {
                    type_params: type_params.clone(),
                    param_types,
                    return_type: ret,
                });
                // also register any functions declared inside this one's body
                for stmt in body { self.collect_sig(stmt); }
            }
            // recurse into control-flow blocks to catch fn declarations inside them
            Expr::If { then_block, else_block, .. } => {
                for s in then_block { self.collect_sig(s); }
                if let Some(b) = else_block { for s in b { self.collect_sig(s); } }
            }
            Expr::While { body, .. } | Expr::For { body, .. } | Expr::ForEnumerate { body, .. } | Expr::ForDestructure { body, .. } => {
                for s in body { self.collect_sig(s); }
            }
            Expr::Try { body, catch_body, .. } => {
                for s in body { self.collect_sig(s); }
                for s in catch_body { self.collect_sig(s); }
            }
            _ => {}
        }
    }

    // Infer the type of one expression without modifying any state.
    // Returns Unknown when the type can't be determined — this is always safe.
    fn infer(&self, expr: &Expr, locals: &HashMap<String, Type>) -> Type {
        match expr {
            Expr::Line(_, inner)  => self.infer(inner, locals),
            Expr::IntLit(_)       => Type::Int,
            Expr::FloatLit(_)     => Type::Float,
            Expr::StrLit(_)       => Type::Str,
            Expr::StrInterp(_)    => Type::Str,
            Expr::BoolLit(_)      => Type::Bool,
            Expr::NilLit          => Type::Nil,
            Expr::HashMap(_)      => Type::HashMap,
            // ! always produces a bool regardless of what it wraps
            Expr::Not(_)          => Type::Bool,

            // Peek at the first element to figure out the element type.
            // [1, 2, 3] → [int], ["a"] → [string], [] → Array (we can't tell).
            Expr::Array(elems) => {
                if let Some(first) = elems.first() {
                    let t = self.infer(first, locals);
                    if !matches!(t, Type::Unknown) {
                        return Type::TypedArray(Box::new(t));
                    }
                }
                Type::Array
            }

            // arr[i] — if arr is [int], the result is int
            Expr::Index { object, .. } => {
                match self.infer(object, locals) {
                    Type::TypedArray(inner) => *inner,
                    // untyped array or non-array — we can't say anything useful
                    _ => Type::Unknown,
                }
            }

            // variable lookup — Unknown if it hasn't been seen yet (e.g. forward reference)
            Expr::Ident(name) => locals.get(name).cloned().unwrap_or(Type::Unknown),

            Expr::BinaryOp { left, op, right } => {
                use crate::lexer::Token;
                match op {
                    // comparison and logical operators always produce bool
                    Token::EqualsEquals | Token::BangEquals |
                    Token::Less | Token::LessEquals |
                    Token::Greater | Token::GreaterEquals |
                    Token::And | Token::Or => Type::Bool,

                    // / always promotes to float in Nova (int / int = float, same as Python 3)
                    Token::Slash => Type::Float,

                    // ?? returns the right side's type (that's what you'd actually get at runtime)
                    Token::QuestionQuestion => self.infer(right, locals),

                    _ => {
                        let l = self.infer(left, locals);
                        let r = self.infer(right, locals);
                        match (&l, &r) {
                            // if either side is float, the whole expression is float
                            (Type::Float, _) | (_, Type::Float) => Type::Float,
                            (Type::Int,   Type::Int)            => Type::Int,
                            // "a" + "b" is string concatenation
                            (Type::Str,   Type::Str) if matches!(op, Token::Plus) => Type::Str,
                            _ => Type::Unknown,
                        }
                    }
                }
            }

            // Named call — look up the signature and figure out the return type.
            // For generic functions, we first resolve T from the actual arguments.
            Expr::Call { name, args } => {
                let sig = match self.fn_sigs.get(name) {
                    Some(s) => s.clone(),
                    // unknown function (e.g. builtin, or declared after this point) — give up
                    None    => return Type::Unknown,
                };
                // non-generic: just return the declared/inferred return type directly
                if sig.type_params.is_empty() {
                    return sig.return_type.clone().unwrap_or(Type::Unknown);
                }
                // generic: build substitution map by matching params to actual arg types
                let mut subs: HashMap<String, Type> = HashMap::new();
                for (arg, param_type) in args.iter().zip(sig.param_types.iter()) {
                    if let Some(pt) = param_type {
                        let actual = self.infer(arg, locals);
                        if !matches!(actual, Type::Unknown) {
                            unify(pt, &actual, &mut subs);
                        }
                    }
                }
                // substitute the resolved type variables into the return type annotation
                match &sig.return_type {
                    Some(ret) => substitute(ret, &subs),
                    None      => Type::Unknown,
                }
            }

            // call through a variable (e.g. fn stored in a let) — can't know return type
            Expr::DynCall { .. } => Type::Unknown,
            // assignments produce nil — they're statements, not expressions with a useful value
            Expr::Let { .. } | Expr::Assign { .. } | Expr::IndexAssign { .. } => Type::Nil,

            _ => Type::Unknown,
        }
    }

    // Walk a list of statements, tracking locals as we go.
    // `is_last` tells check_expr whether a statement is the final one in a block
    // (the implicit return value) — needed for return-type checking.
    fn check_stmts(&mut self, stmts: &[Expr], locals: &mut HashMap<String, Type>, ret: Option<&Type>) {
        let n = stmts.len();
        for (i, stmt) in stmts.iter().enumerate() {
            let is_last = i == n - 1;
            self.check_expr(stmt, locals, ret, is_last);
        }
    }

    // Recursively check one expression for type errors.
    // `locals` is updated in-place as new variables are declared.
    // `ret` is the enclosing function's return type (None at top level).
    // `is_last` is true for the last statement in a block (implicit return position).
    fn check_expr(&mut self, expr: &Expr, locals: &mut HashMap<String, Type>, ret: Option<&Type>, is_last: bool) {
        match expr {
            Expr::Line(line, inner) => {
                self.current_line = *line;
                self.check_expr(inner, locals, ret, is_last);
            }

            // let x = value — check the value, then record x's type in locals
            Expr::Let { name, value } => {
                self.check_expr(value, locals, ret, false);
                let t = self.infer(value, locals);
                locals.insert(name.clone(), t);
            }

            // x = value — check the value; update the tracked type if we can infer one
            // unannotated variables can change type freely in Nova — no error for x = 1; x = "hi"
            Expr::Assign { name, value } => {
                self.check_expr(value, locals, ret, false);
                let t = self.infer(value, locals);
                if !matches!(t, Type::Unknown) {
                    locals.insert(name.clone(), t);
                }
            }

            // named call — check each argument, then verify they match the signature
            Expr::Call { name, args } => {
                for arg in args { self.check_expr(arg, locals, ret, false); }
                self.check_call(name, args, locals);
            }

            // fn declaration — open a new local scope, seed it with param types, check the body
            Expr::Fn { name: _, params, body, return_type, .. } => {
                let ret_t = return_type.as_deref().and_then(parse_type);
                // clone locals so the function body can't pollute the outer scope
                let mut fn_locals = locals.clone();
                for (pname, ptype, _) in params {
                    let t = ptype.as_deref().and_then(parse_type).unwrap_or(Type::Unknown);
                    fn_locals.insert(pname.clone(), t);
                }
                self.check_stmts(body, &mut fn_locals, ret_t.as_ref());
                // if there's an explicit return annotation, check that the last expression matches
                if let Some(expected_ret) = &ret_t {
                    if let Some(last) = body.last() {
                        let got = self.infer(last, &fn_locals);
                        if !compatible(expected_ret, &got) && !matches!(got, Type::Unknown) {
                            self.error(format!(
                                "function body returns {} but declared return type is {}",
                                got.name(), expected_ret.name()
                            ));
                        }
                    }
                }
            }

            // explicit return statement — check the value and compare to the enclosing fn's return type
            Expr::Return(value) => {
                self.check_expr(value, locals, ret, false);
                if let Some(expected_ret) = ret {
                    let got = self.infer(value, locals);
                    if !compatible(expected_ret, &got) && !matches!(got, Type::Unknown) {
                        self.error(format!(
                            "return type mismatch: expected {}, got {}",
                            expected_ret.name(), got.name()
                        ));
                    }
                }
            }

            // if/else — check condition and both branches; locals from one branch don't leak to the other
            Expr::If { condition, then_block, else_block } => {
                self.check_expr(condition, locals, ret, false);
                self.check_stmts(then_block, locals, ret);
                if let Some(b) = else_block { self.check_stmts(b, locals, ret); }
            }

            Expr::While { condition, body } => {
                self.check_expr(condition, locals, ret, false);
                self.check_stmts(body, locals, ret);
            }

            // for x in iter — infer the element type from the iterable and bind the loop variable
            Expr::For { var, iter, body } => {
                self.check_expr(iter, locals, ret, false);
                let mut loop_locals = locals.clone();
                let elem_type = match self.infer(iter, locals) {
                    Type::TypedArray(inner) => *inner,
                    Type::Unknown           => Type::Unknown,
                    // anything non-array (e.g. a range expression) produces ints
                    _                       => Type::Int,
                };
                loop_locals.insert(var.clone(), elem_type);
                self.check_stmts(body, &mut loop_locals, ret);
            }

            // for i, x in iter — index is always int, element type comes from the iterable
            Expr::ForEnumerate { iter, body, index_var, item_var } => {
                self.check_expr(iter, locals, ret, false);
                let mut loop_locals = locals.clone();
                loop_locals.insert(index_var.clone(), Type::Int);
                let elem_type = match self.infer(iter, locals) {
                    Type::TypedArray(inner) => *inner,
                    _                       => Type::Unknown,
                };
                loop_locals.insert(item_var.clone(), elem_type);
                self.check_stmts(body, &mut loop_locals, ret);
            }

            // for a, b in iter — destructuring, we can't know individual types so mark all Unknown
            Expr::ForDestructure { vars, iter, body } => {
                self.check_expr(iter, locals, ret, false);
                let mut loop_locals = locals.clone();
                for v in vars { loop_locals.insert(v.clone(), Type::Unknown); }
                self.check_stmts(body, &mut loop_locals, ret);
            }

            // try/catch — catch var is always Unknown since we don't type exceptions
            Expr::Try { body, catch_body, catch_var } => {
                self.check_stmts(body, locals, ret);
                let mut catch_locals = locals.clone();
                catch_locals.insert(catch_var.clone(), Type::Unknown);
                self.check_stmts(catch_body, &mut catch_locals, ret);
            }

            // match expression — check the matched value and each arm's body
            Expr::Match { value, arms } => {
                self.check_expr(value, locals, ret, false);
                for (pattern, body) in arms {
                    if let Some(p) = pattern { self.check_expr(p, locals, ret, false); }
                    self.check_stmts(body, locals, ret);
                }
            }

            // single-expression statements that don't introduce bindings — just recurse
            Expr::Print(e) | Expr::Printn(e) | Expr::Not(e) | Expr::Throw(e) | Expr::Spawn(e) | Expr::Defer(e) => {
                self.check_expr(e, locals, ret, false);
            }

            // select { ch -> ... } — check each channel expression and each arm's body
            Expr::Select { arms, default_body } => {
                for (ch_expr, _, body) in arms {
                    self.check_expr(ch_expr, locals, ret, false);
                    for stmt in body { self.check_expr(stmt, locals, ret, false); }
                }
                if let Some(stmts) = default_body {
                    for stmt in stmts { self.check_expr(stmt, locals, ret, false); }
                }
            }

            Expr::BinaryOp { left, right, .. } => {
                self.check_expr(left, locals, ret, false);
                self.check_expr(right, locals, ret, false);
            }

            Expr::Array(elems) => {
                for e in elems { self.check_expr(e, locals, ret, false); }
            }

            Expr::HashMap(pairs) => {
                for (k, v) in pairs {
                    self.check_expr(k, locals, ret, false);
                    self.check_expr(v, locals, ret, false);
                }
            }

            Expr::Index { object, index } => {
                self.check_expr(object, locals, ret, false);
                self.check_expr(index, locals, ret, false);
            }

            Expr::IndexAssign { index, value, .. } => {
                self.check_expr(index, locals, ret, false);
                self.check_expr(value, locals, ret, false);
            }

            // lambda — gets its own local scope; no return type checking (can't annotate lambdas)
            Expr::Lambda { body, .. } => {
                let mut lambda_locals = locals.clone();
                self.check_stmts(body, &mut lambda_locals, None);
            }

            Expr::Range { start, end } => {
                self.check_expr(start, locals, ret, false);
                self.check_expr(end, locals, ret, false);
            }

            // call through a variable — check the callee expression and each argument
            Expr::DynCall { callee, args } => {
                self.check_expr(callee, locals, ret, false);
                for arg in args { self.check_expr(arg, locals, ret, false); }
            }

            // literals and control-flow keywords — nothing to check
            Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_) |
            Expr::NilLit | Expr::StrLit(_) | Expr::StrInterp(_) |
            Expr::Ident(_) | Expr::Break | Expr::Continue | Expr::Import(_) |
            Expr::LetArrayDestructure { .. } | Expr::LetMapDestructure { .. } |
            Expr::StructDef { .. } => {}

            Expr::StructLit { fields, .. } => {
                for (_, fexpr) in fields { self.check_expr(fexpr, locals, ret, false); }
            }
            Expr::FieldAccess { object, .. } => { self.check_expr(object, locals, ret, false); }
            Expr::FieldAssign { object, value, .. } => {
                self.check_expr(object, locals, ret, false);
                self.check_expr(value, locals, ret, false);
            }
            Expr::EnumDef { .. } | Expr::EnumPattern { .. } => {}
            Expr::ImplBlock { methods, .. } => {
                for m in methods { self.check_expr(m, locals, ret, false); }
            }
            Expr::MethodCall { object, args, .. } => {
                self.check_expr(object, locals, ret, false);
                for a in args { self.check_expr(a, locals, ret, false); }
            }
        }
    }

    // Pass 2 helpers — infer return types for functions that have no annotation.

    fn infer_return_types(&mut self, stmts: &[Expr]) {
        for stmt in stmts { self.infer_return_type_stmt(stmt); }
    }

    fn infer_return_type_stmt(&mut self, expr: &Expr) {
        match expr {
            Expr::Line(line, inner) => {
                self.current_line = *line;
                self.infer_return_type_stmt(inner);
            }
            Expr::Fn { name, params, body, return_type, .. } => {
                // only try to infer if the user didn't already annotate a return type
                if return_type.is_none() {
                    self.maybe_infer_return(name, params, body);
                }
                // recurse into the body to catch nested function declarations
                for s in body { self.infer_return_type_stmt(s); }
            }
            Expr::If { then_block, else_block, .. } => {
                for s in then_block { self.infer_return_type_stmt(s); }
                if let Some(b) = else_block { for s in b { self.infer_return_type_stmt(s); } }
            }
            Expr::While { body, .. } | Expr::For { body, .. } | Expr::ForEnumerate { body, .. } | Expr::ForDestructure { body, .. } => {
                for s in body { self.infer_return_type_stmt(s); }
            }
            _ => {}
        }
    }

    // Try to infer the return type of function `name` from its body.
    // We look at two sources:
    //   1. explicit `return` statements anywhere in the body
    //   2. the last expression (implicit return, like Rust)
    // If all sources agree on one type, we record it in fn_sigs.
    // If they conflict or there's nothing to go on, we leave the return type as None.
    fn maybe_infer_return(&mut self, name: &str, params: &[(String, Option<String>, Option<Box<crate::parser::Expr>>)], body: &[Expr]) {
        // skip if we already have a return type — don't overwrite an explicit annotation
        // or a type that was inferred in an earlier pass
        if self.fn_sigs.get(name).and_then(|s| s.return_type.as_ref()).is_some() {
            return;
        }

        // build a minimal locals map so infer() can look up param types
        let mut fn_locals: HashMap<String, Type> = HashMap::new();
        for (pname, ptype, _) in params {
            let t = ptype.as_deref().and_then(parse_type).unwrap_or(Type::Unknown);
            fn_locals.insert(pname.clone(), t);
        }

        // collect types from all explicit return statements
        let mut found: Vec<Type> = Vec::new();
        for s in body {
            collect_returns(s, &fn_locals, &mut found, self);
        }

        // also consider the last expression as the implicit return value
        // but only if there are no explicit returns (or if the last expr is non-trivial)
        if let Some(last) = body.last() {
            let t = self.infer(last, &fn_locals);
            if !matches!(t, Type::Unknown | Type::Nil) || found.is_empty() {
                if !matches!(t, Type::Unknown) { found.push(t); }
            }
        }

        // nothing usable — give up, leave return type as Unknown
        if found.is_empty() { return; }

        // if all observed returns agree on the same type, that's our inferred return type
        // if they disagree (e.g. sometimes int, sometimes string) we can't say anything
        let first = found[0].clone();
        if found.iter().all(|t| t == &first) {
            if let Some(sig) = self.fn_sigs.get_mut(name) {
                sig.return_type = Some(first);
            }
        }
    }

    // Verify that all arguments at a call site match the function's expected parameter types.
    fn check_call(&mut self, name: &str, args: &[Expr], locals: &HashMap<String, Type>) {
        // borrow the whole sig by cloning — avoids holding a reference into fn_sigs
        // while we also need &mut self for error()
        let sig = match self.fn_sigs.get(name) {
            Some(s) => s.clone(),
            // no signature recorded means it's a builtin or an unannotated fn — skip
            None    => return,
        };

        // for generic functions, resolve type variables first so we check concrete types
        // e.g. calling sort([1,2,3]) on fn sort(arr: [T]) resolves T → int before checking
        let mut subs: HashMap<String, Type> = HashMap::new();
        if !sig.type_params.is_empty() {
            for (arg, param_type) in args.iter().zip(sig.param_types.iter()) {
                if let Some(pt) = param_type {
                    let actual = self.infer(arg, locals);
                    if !matches!(actual, Type::Unknown) {
                        unify(pt, &actual, &mut subs);
                    }
                }
            }
        }

        for (i, (arg, param_type)) in args.iter().zip(sig.param_types.iter()).enumerate() {
            let expected_raw = match param_type {
                Some(t) => t,
                // unannotated parameter — nothing to check
                None    => continue,
            };
            // for generics, substitute resolved vars; for non-generics, use the type as-is
            let expected = if sig.type_params.is_empty() {
                expected_raw.clone()
            } else {
                substitute(expected_raw, &subs)
            };
            let got = self.infer(arg, locals);
            if !compatible(&expected, &got) && !matches!(got, Type::Unknown) {
                self.error(format!(
                    "argument {} of '{}': expected {}, got {}",
                    i + 1, name, expected.name(), got.name()
                ));
            }
        }
    }
}

pub fn typecheck(stmts: &[Expr]) -> bool {
    let mut checker = Checker::new();

    // Pass 1 — record all explicit type annotations
    checker.collect_signatures(stmts);

    // Pass 2 — infer return types for unannotated functions, fixed-point style.
    // One pass won't do if function A calls function B and B's return type isn't known yet.
    // We repeat until no new return types are discovered (or cap at 10 to avoid infinite loops).
    for _ in 0..10 {
        let before = checker.fn_sigs.values().filter(|s| s.return_type.is_some()).count();
        checker.infer_return_types(stmts);
        let after = checker.fn_sigs.values().filter(|s| s.return_type.is_some()).count();
        // stable — no new types discovered this pass, no point continuing
        if after == before { break; }
    }

    // Pass 3 — walk every statement and check types at call sites and return statements
    let mut locals = HashMap::new();
    checker.check_stmts(stmts, &mut locals, None);

    // return true = errors found = program should NOT run
    if checker.errors.is_empty() {
        return false;
    }
    for e in &checker.errors {
        eprintln!("{}", e);
    }
    true
}
