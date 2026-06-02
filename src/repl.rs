// repl.rs — the interactive Read-Eval-Print Loop.
//
// The REPL runs on a persistent Vm — globals and functions defined in one input line
// remain available in all subsequent lines. This is what makes the REPL feel interactive.
//
// Multi-line input:
//   The REPL counts unmatched { braces. If open > close, the block isn't finished yet
//   so we print "...>" and keep accumulating lines before trying to parse/run.
//
// Error recovery:
//   Every parse and every vm.run is wrapped in catch_unwind(AssertUnwindSafe(...)).
//   nova_error() panics instead of calling process::exit precisely so these handlers can
//   catch the error, print it, and keep the REPL alive.
//   On error, vm.restore(globals_snap, fns_snap) rewinds the VM state to the snapshot
//   taken before the failing input — so partial side effects don't corrupt future lines.
//
// NEEDS_NEWLINE:
//   printn (print without newline) sets this flag. The REPL checks it at the top of each
//   loop iteration and prints a blank line if needed, so the "nova>" prompt always starts
//   on a fresh line instead of being appended after the printn output.
use std::io::{self, Write};
use std::panic;
use std::sync::atomic::Ordering;
use crate::lexer::{Lexer, Token};
use crate::parser::Parser;
use crate::evaluator::{format_value, Value, NEEDS_NEWLINE};
use crate::compiler;
use crate::vm::Vm;

pub fn run() {
    let mut vm = Vm::new();
    let mut input = String::new(); // accumulates lines until the block is complete

    println!("Nova REPL — type 'quit' to exit");

    loop {
        // if the last output used printn (no newline), add a newline before the prompt
        if NEEDS_NEWLINE.swap(false, Ordering::Relaxed) {
            println!();
        }

        // "nova>" for fresh input, "...>" for continuation of a multi-line block
        if input.is_empty() {
            print!("nova> ");
        } else {
            print!("...>  ");
        }
        io::stdout().flush().unwrap(); // flush so the prompt appears before stdin blocks

        let mut line = String::new();
        if io::stdin().read_line(&mut line).unwrap() == 0 {
            break; // EOF (Ctrl-D / piped input ended) — exit cleanly
        }

        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

        if input.is_empty() && (trimmed == "quit" || trimmed == "exit") {
            break;
        }

        input.push_str(trimmed);
        input.push('\n');

        // wait for all opened braces to be closed before parsing
        // this lets the user type multi-line functions and blocks naturally
        let open  = input.chars().filter(|&c| c == '{').count();
        let close = input.chars().filter(|&c| c == '}').count();
        if open > close {
            continue; // block not complete yet — show "...>" and read more
        }

        // if triple-quote count is odd, we're still inside a multiline string
        if input.matches("\"\"\"").count() % 2 != 0 {
            continue;
        }

        // snapshot VM state so we can roll back on error
        let (globals_snap, fns_snap, methods_snap) = vm.snapshot();

        // Phase 1: lex + parse + compile — all wrapped in catch_unwind for error recovery
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let mut lex = Lexer::new(&input);
            let tokens = lex.tokenize();
            let mut parser = Parser::new(tokens);
            let mut stmts = Vec::new();
            while !matches!(parser.current_token(), Token::EOF) {
                stmts.push(parser.parse_statement());
            }
            // compile_repl (not compile) so globals from previous lines remain visible
            let chunk = compiler::compile_repl(&stmts);
            chunk
        }));

        match result {
            Ok(chunk) => {
                // Phase 2: execute — also wrapped in catch_unwind
                let run_result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                    vm.run(chunk)
                }));
                match run_result {
                    Ok(val) => {
                        // auto-print non-nil expression results (like a Python REPL)
                        if let Some(v) = val {
                            if !matches!(v, Value::Nil | Value::Break | Value::Continue) {
                                println!("{}", format_value(&v));
                            }
                        }
                    }
                    Err(_) => {
                        // runtime error — roll back VM state to before this input
                        vm.restore(globals_snap, fns_snap, methods_snap);
                    }
                }
            }
            Err(_) => {
                // parse/compile error — roll back VM state
                vm.restore(globals_snap, fns_snap, methods_snap);
            }
        }

        input.clear(); // ready for the next line
    }
}
