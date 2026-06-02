// main.rs — entry point for the Nova language runtime.
mod lexer;
mod parser;
mod evaluator;
mod error;
mod warnings;
mod typechecker;
mod repl;
mod compiler;
mod vm;
mod codegen;

use lexer::{Lexer, Token};
use parser::Parser;
use evaluator::{eval, Env, collect_cycles, memory_report};
extern crate rayon;

fn main() {
    // suppress the default Rust panic traceback for nova_error panics —
    // nova_error already printed its own message, so the extra traceback is just noise
    std::panic::set_hook(Box::new(|info| {
        if let Some(s) = info.payload().downcast_ref::<&str>() {
            if *s == "nova_error" {
                return; // nova_error already printed — nothing more to show
            }
        }
        eprintln!("{}", info); // real Rust bug — print it normally
    }));

    // Spawn on a larger stack to handle deep recursion without OS stack overflow.
    // Debug builds need 64 MB (unoptimized stack frames are large).
    // Release builds need far less but we give 8 MB so the depth counter fires first.
    #[cfg(debug_assertions)]
    let stack_size = 64 * 1024 * 1024;
    #[cfg(not(debug_assertions))]
    let stack_size = 8 * 1024 * 1024;

    let builder = std::thread::Builder::new().stack_size(stack_size);
    let handler = builder.spawn(run).unwrap();
    std::process::exit(match handler.join() {
        Ok(()) => 0,
        Err(_) => 1,
    });
}

fn run() {
    let args: Vec<String> = std::env::args().collect();
    // args[0] is always the binary name, so we look at args[1] onward

    if args.len() >= 3 && args[1] == "run" {
        // nova run [--memory] [--tree] [--vm] [--jobs N] <filename>
        let show_memory = args.iter().any(|a| a == "--memory");
        let use_tree    = args.iter().any(|a| a == "--tree");
        let force_vm    = args.iter().any(|a| a == "--vm");

        // --jobs N: set rayon global pool size (default: logical core count)
        if let Some(pos) = args.iter().position(|a| a == "--jobs") {
            let n: usize = args.get(pos + 1)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("Error: --jobs requires a positive integer");
                    std::process::exit(1);
                });
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build_global()
                .unwrap_or_else(|e| eprintln!("Warning: could not set thread pool size: {}", e));
        }

        // skip flag values (e.g. the "4" after "--jobs 4") when hunting for the filename
        let filename = {
            let mut skip_next = false;
            args.iter().skip(2).find(|a| {
                if skip_next { skip_next = false; return false; }
                if **a == "--jobs" { skip_next = true; return false; }
                !a.starts_with("--")
            })
        }.unwrap_or_else(|| {
            eprintln!("Error: expected a filename — usage: nova run [--tree|--vm] [--memory] [--jobs N] <file.nova>");
            std::process::exit(1);
        });
        let source = std::fs::read_to_string(filename)
            .unwrap_or_else(|e| {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown>".to_string());
                match e.kind() {
                    std::io::ErrorKind::NotFound =>
                        eprintln!("Error: file '{}' not found\n  looking in: {}", filename, cwd),
                    std::io::ErrorKind::PermissionDenied =>
                        eprintln!("Error: permission denied reading '{}'", filename),
                    _ =>
                        eprintln!("Error: could not read '{}': {}", filename, e),
                }
                std::process::exit(1);
            });
        if use_tree {
            run_file(&source, show_memory);
        } else if force_vm {
            run_file_vm(&source);
        } else {
            // Default: use LLVM if clang is available, otherwise fall back to VM silently.
            match find_clang() {
                Some(clang) => run_file_llvm(&source, filename, &clang),
                None => {
                    eprintln!("note: clang not found — running via VM. Install it for native speed:\n  {}", clang_install_hint());
                    run_file_vm(&source);
                }
            }
        }
    } else if args.len() == 2 && args[1] == "run" {
        // nova run with no filename
        eprintln!("Error: expected a filename — usage: nova run [--tree|--vm] [--memory] <file.nova>");
        std::process::exit(1);
    } else if args.len() >= 3 && args[1] == "build" {
        // nova build <filename> — compile to a native binary via LLVM IR
        let filename = &args[2];
        let source = std::fs::read_to_string(filename).unwrap_or_else(|e| {
            eprintln!("Error: could not read '{}': {}", filename, e);
            std::process::exit(1);
        });
        let clang = find_clang().unwrap_or_else(|| {
            eprintln!("Error: clang not found. Install it with:\n  {}", clang_install_hint());
            std::process::exit(1);
        });

        let mut lex = Lexer::new(&source);
        let tokens = lex.tokenize();
        let mut parser = Parser::new(tokens);
        let mut stmts = Vec::new();
        while !matches!(parser.current_token(), Token::EOF) {
            stmts.push(parser.parse_statement());
            parser.skip_optional_semicolon();
        }
        let mut cg = codegen::Codegen::new();
        let ll = cg.compile_program(&stmts);

        // write the .ll file next to the source
        let ll_path = filename.replace(".nova", ".ll");
        std::fs::write(&ll_path, &ll).unwrap_or_else(|e| {
            eprintln!("Error writing IR: {}", e);
            std::process::exit(1);
        });

        ensure_rt_o(&clang);

        // compile with clang: link against nova_rt.o
        #[cfg(not(target_os = "windows"))]
        let out_path = filename.replace(".nova", "");
        #[cfg(target_os = "windows")]
        let out_path = filename.replace(".nova", ".exe");

        #[cfg(not(target_os = "windows"))]
        let link_args: &[&str] = &["-O1", &ll_path, "nova_rt.o", "-o", &out_path, "-lpthread"];
        #[cfg(target_os = "windows")]
        let link_args: &[&str] = &["-O1", &ll_path, "nova_rt.o", "-o", &out_path];

        let status = std::process::Command::new(&clang)
            .args(link_args)
            .status()
            .unwrap_or_else(|e| {
                eprintln!("Error running clang: {}", e);
                std::process::exit(1);
            });
        if !status.success() { std::process::exit(1); }
        println!("Built: {}", out_path);
    } else {
        // no arguments — launch the REPL
        repl::run();
    }
}

// Returns the first clang executable found on this machine, or None.
fn find_clang() -> Option<String> {
    let candidates: &[&str] = &[
        "clang",
        r"C:\Program Files\LLVM\bin\clang.exe",
    ];
    for &c in candidates {
        if std::process::Command::new(c)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(c.to_string());
        }
    }
    None
}

// Platform-specific one-liner for installing clang — shown when build fails.
fn clang_install_hint() -> &'static str {
    #[cfg(target_os = "windows")]
    { "winget install LLVM.LLVM" }
    #[cfg(target_os = "macos")]
    { "brew install llvm  (or: xcode-select --install)" }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    { "apt install clang  (or: dnf install clang / pacman -S clang)" }
}

// Rebuild nova_rt.o if it is missing or older than nova_rt.c.
fn ensure_rt_o(clang: &str) {
    let rt_c = {
        let mut p = std::env::current_exe()
            .unwrap_or_default()
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("nova_rt.c");
        if !p.exists() { p = std::path::PathBuf::from("nova_rt.c"); }
        p
    };
    let rt_o = std::path::PathBuf::from("nova_rt.o");
    let needs_rebuild = if rt_o.exists() {
        let mt_c = std::fs::metadata(&rt_c).and_then(|m| m.modified()).ok();
        let mt_o = std::fs::metadata(&rt_o).and_then(|m| m.modified()).ok();
        matches!((mt_c, mt_o), (Some(c), Some(o)) if c > o)
    } else {
        true
    };
    if needs_rebuild {
        if !rt_c.exists() {
            eprintln!("Error: nova_rt.c not found — cannot build runtime object");
            std::process::exit(1);
        }
        eprintln!("Building nova_rt.o...");
        let st = std::process::Command::new(clang)
            .args([rt_c.to_str().unwrap(), "-O2", "-c", "-o", "nova_rt.o"])
            .status()
            .unwrap_or_else(|e| { eprintln!("clang error: {}", e); std::process::exit(1); });
        if !st.success() { std::process::exit(1); }
    }
}

// Compile source to a temp binary, run it, then delete the temp files.
fn run_file_llvm(source: &str, filename: &str, clang: &str) {
    let mut lex = Lexer::new(source);
    let tokens = lex.tokenize();
    let mut parser = Parser::new(tokens);
    let mut stmts = Vec::new();
    while !matches!(parser.current_token(), Token::EOF) {
        stmts.push(parser.parse_statement());
        parser.skip_optional_semicolon();
    }
    warnings::check_unused(&stmts);
    if typechecker::typecheck(&stmts) { std::process::exit(1); }
    let mut cg = codegen::Codegen::new();
    let ll = cg.compile_program(&stmts);

    let tmp = std::env::temp_dir();
    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("nova_tmp");
    let ll_path  = tmp.join(format!("{}_nova_run.ll", stem));
    #[cfg(target_os = "windows")]
    let exe_path = tmp.join(format!("{}_nova_run.exe", stem));
    #[cfg(not(target_os = "windows"))]
    let exe_path = tmp.join(format!("{}_nova_run", stem));

    std::fs::write(&ll_path, &ll).unwrap_or_else(|e| {
        eprintln!("Error writing IR: {}", e);
        std::process::exit(1);
    });

    ensure_rt_o(clang);

    let ll_str  = ll_path.to_str().unwrap();
    let exe_str = exe_path.to_str().unwrap();
    #[cfg(not(target_os = "windows"))]
    let link_args = vec!["-O1", ll_str, "nova_rt.o", "-o", exe_str, "-lpthread"];
    #[cfg(target_os = "windows")]
    let link_args = vec!["-O1", ll_str, "nova_rt.o", "-o", exe_str];

    let compile_ok = std::process::Command::new(clang)
        .args(&link_args)
        .stderr(std::process::Stdio::null()) // suppress the target-triple warning
        .status()
        .unwrap_or_else(|e| { eprintln!("Error running clang: {}", e); std::process::exit(1); })
        .success();
    let _ = std::fs::remove_file(&ll_path);
    if !compile_ok { std::process::exit(1); }

    let exit_code = std::process::Command::new(&exe_path)
        .status()
        .unwrap_or_else(|e| { eprintln!("Error running program: {}", e); std::process::exit(1); })
        .code()
        .unwrap_or(0);
    let _ = std::fs::remove_file(&exe_path);
    if exit_code != 0 { std::process::exit(exit_code); }
}

fn run_file_vm(source: &str) {
    let mut lex = Lexer::new(source);
    let tokens = lex.tokenize();
    let mut parser = Parser::new(tokens);
    let mut stmts = Vec::new();
    while !matches!(parser.current_token(), Token::EOF) {
        stmts.push(parser.parse_statement());
        parser.skip_optional_semicolon();
    }
    warnings::check_unused(&stmts);
    if typechecker::typecheck(&stmts) { std::process::exit(1); }
    let chunk = compiler::compile(&stmts);
    let mut vm = vm::Vm::new();
    vm.run(chunk);
}

fn run_file(source: &str, show_memory: bool) {
    let mut lex = Lexer::new(source);
    let tokens = lex.tokenize();
    let mut parser = Parser::new(tokens);

    let mut stmts = Vec::new();
    while !matches!(parser.current_token(), Token::EOF) {
        stmts.push(parser.parse_statement());
        parser.skip_optional_semicolon();
    }

    warnings::check_unused(&stmts);

    if typechecker::typecheck(&stmts) {
        std::process::exit(1); // type errors found (program doesnt run)
    }

    let mut env = Env::new();
    for stmt in &stmts {
        eval(stmt, &mut env);
        collect_cycles(&env);
    }

    if show_memory {
        eprintln!("{}", memory_report());
    }
}