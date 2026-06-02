// warnings.rs — static analysis pass that runs after parsing, before execution.
//
// Currently implements one check: unused variable detection.
// Two-collection walk: declared collects every name that was *bound* (let, fn, for, etc.)
// used collects every name that was *read* (Ident, Call, etc.)
// After the walk, anything in declared but not in used gets a warning.
// This pass is intentionally skipped in the REPL — you type line by line, so future
// uses of a variable declared now would always appear as unused.
use crate::parser::{Expr, Parser};
use crate::lexer::Lexer;
use std::collections::HashSet;

pub fn check_unused(stmts: &[Expr]) {
    let mut declared: Vec<(String, usize)> = Vec::new();  // (name, line) — order preserved for warnings
    let mut used: HashSet<String> = HashSet::new();       // every name that was actually read
    let mut current_line: usize = 0;                      // tracks the line of the current statement
    used.insert("_".to_string()); // _ is the conventional discard — never warn about it

    for stmt in stmts {
        collect(stmt, &mut declared, &mut used, &mut current_line);
    }

    for (name, line) in &declared {
        if !used.contains(name) {
            if *line > 0 {
                eprintln!("Warning on line {}: '{}' is declared but never used", line, name);
            } else {
                eprintln!("Warning: '{}' is declared but never used", name);
            }
        }
    }
}

fn collect(expr: &Expr, declared: &mut Vec<(String, usize)>, used: &mut HashSet<String>, current_line: &mut usize) {
    match expr {
        // Line wrapper — update current line, then recurse into the inner expression
        Expr::Line(line, inner) => {
            *current_line = *line;
            collect(inner, declared, used, current_line);
        }

        Expr::Let { name, value } => {
            declared.push((name.clone(), *current_line)); // record name and its line
            collect(value, declared, used, current_line);
        }

        Expr::Assign { name, value } => {
            used.insert(name.clone()); // assigning to a variable counts as using it
            collect(value, declared, used, current_line);
        }

        Expr::Ident(name) => {
            used.insert(name.clone());
        }

        Expr::BinaryOp { left, right, .. } => {
            collect(left, declared, used, current_line);
            collect(right, declared, used, current_line);
        }

        Expr::If { condition, then_block, else_block } => {
            collect(condition, declared, used, current_line);
            for stmt in then_block { collect(stmt, declared, used, current_line); }
            if let Some(else_b) = else_block {
                for stmt in else_b { collect(stmt, declared, used, current_line); }
            }
        }

        Expr::While { condition, body } => {
            collect(condition, declared, used, current_line);
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::For { var, iter, body } => {
            used.insert(var.clone()); // loop variable is always considered used
            collect(iter, declared, used, current_line);
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::Fn { name, params, body, .. } => {
            declared.push((name.clone(), *current_line));
            for (p, _, _) in params { used.insert(p.clone()); }
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::Lambda { params, body } => {
            for p in params { used.insert(p.clone()); }
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::Call { name, args } => {
            used.insert(name.clone()); // calling f(...) counts as using f
            for arg in args { collect(arg, declared, used, current_line); }
        }

        Expr::Print(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Printn(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Array(elements) => {
            for e in elements { collect(e, declared, used, current_line); }
        }

        Expr::HashMap(pairs) => {
            for (k, v) in pairs {
                collect(k, declared, used, current_line);
                collect(v, declared, used, current_line);
            }
        }

        Expr::Index { object, index } => {
            collect(object, declared, used, current_line);
            collect(index, declared, used, current_line);
        }

        Expr::IndexAssign { name, index, value } => {
            used.insert(name.clone());
            collect(index, declared, used, current_line);
            collect(value, declared, used, current_line);
        }

        Expr::Match { value, arms } => {
            collect(value, declared, used, current_line);
            for (pattern, body) in arms {
                if let Some(p) = pattern { collect(p, declared, used, current_line); }
                for stmt in body { collect(stmt, declared, used, current_line); }
            }
        }

        Expr::Range { start, end } => {
            collect(start, declared, used, current_line);
            collect(end, declared, used, current_line);
        }

        // interpolated string — parse each {expr} and recursively collect used names
        Expr::StrInterp(parts) => {
            for part in parts {
                if let crate::lexer::StringPart::Interp(expr_text) = part {
                    if expr_text.is_empty() { continue; }
                    let mut lex = Lexer::new(expr_text);
                    let tokens = lex.tokenize();
                    let mut parser = Parser::new(tokens);
                    let expr = parser.parse_expression();
                    collect(&expr, declared, used, current_line);
                }
            }
        }

        Expr::Not(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::ForEnumerate { index_var, item_var, iter, body } => {
            used.insert(index_var.clone()); // loop vars are always considered used
            used.insert(item_var.clone());
            collect(iter, declared, used, current_line);
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::ForDestructure { vars, iter, body } => {
            for v in vars { used.insert(v.clone()); }
            collect(iter, declared, used, current_line);
            for stmt in body { collect(stmt, declared, used, current_line); }
        }

        Expr::Return(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Throw(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Spawn(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Defer(expr) => {
            collect(expr, declared, used, current_line);
        }

        Expr::Select { arms, default_body } => {
            for (ch_expr, bind_var, body) in arms {
                collect(ch_expr, declared, used, current_line);
                declared.push((bind_var.clone(), *current_line));
                for stmt in body { collect(stmt, declared, used, current_line); }
            }
            if let Some(stmts) = default_body {
                for stmt in stmts { collect(stmt, declared, used, current_line); }
            }
        }

        Expr::Try { body, catch_var, catch_body } => {
            for stmt in body { collect(stmt, declared, used, current_line); }
            used.insert(catch_var.clone()); // catch variable is always considered used
            for stmt in catch_body { collect(stmt, declared, used, current_line); }
        }

        Expr::Import(_) => {} // imported file's symbols are not tracked by the local checker

        Expr::LetArrayDestructure { names, value } => {
            for name in names { declared.push((name.clone(), *current_line)); }
            collect(value, declared, used, current_line);
        }

        Expr::LetMapDestructure { names, value } => {
            for name in names { declared.push((name.clone(), *current_line)); }
            collect(value, declared, used, current_line);
        }

        Expr::DynCall { callee, args } => {
            collect(callee, declared, used, current_line);
            for arg in args { collect(arg, declared, used, current_line); }
        }

        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_) | Expr::NilLit |
        Expr::StrLit(_) | Expr::Break | Expr::Continue => {}

        Expr::StructDef { name, .. } => { declared.push((name.clone(), *current_line)); }
        Expr::StructLit { name, fields } => {
            used.insert(name.clone());
            for (_, fexpr) in fields { collect(fexpr, declared, used, current_line); }
        }
        Expr::FieldAccess { object, .. } => { collect(object, declared, used, current_line); }
        Expr::FieldAssign { object, value, .. } => {
            collect(object, declared, used, current_line);
            collect(value, declared, used, current_line);
        }
        Expr::EnumDef { name, .. } => { declared.push((name.clone(), *current_line)); }
        Expr::EnumPattern { .. } => {}
        Expr::ImplBlock { methods, .. } => {
            for m in methods {
                // unwrap Line wrapper, then match Fn
                let func = match m {
                    Expr::Line(_, inner) => inner.as_ref(),
                    other => other,
                };
                // mark method names as used so they don't trigger unused-variable warnings;
                // methods are bound to types, not free variables, so the warning doesn't apply
                if let Expr::Fn { name, params, body, .. } = func {
                    used.insert(name.clone());
                    // warn if first param isn't named self — convention enforcement
                    if let Some((first_param, _, _)) = params.first() {
                        if first_param != "self" {
                            eprintln!("Warning on line {}: method '{}' first parameter is '{}', expected 'self'",
                                current_line, name, first_param);
                        }
                    }
                    for (p, _, _) in params { used.insert(p.clone()); }
                    for stmt in body { collect(stmt, declared, used, current_line); }
                }
            }
        }
        Expr::MethodCall { object, args, .. } => {
            collect(object, declared, used, current_line);
            for a in args { collect(a, declared, used, current_line); }
        }
    }
}
