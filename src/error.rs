// error.rs — Nova's error reporting and termination mechanism.
//
// nova_error() is the single exit point for all runtime errors.
// It prints the error, stores it in LAST_ERROR (readable by try/catch), and panics.
// Panicking (rather than process::exit) lets the REPL recover with catch_unwind.
// TRY_DEPTH suppresses printing to stderr when we're inside a try block — the handler
// will print it via the catch variable instead.
use std::cell::{Cell, RefCell};

thread_local! {
    // last error message — readable by catch_unwind handlers in Expr::Try
    pub static LAST_ERROR: RefCell<String> = RefCell::new(String::new());
    // depth of nested try blocks — when > 0, nova_error suppresses stderr printing
    pub static TRY_DEPTH: Cell<usize> = Cell::new(0);
}

// nova_error — prints a formatted error message and terminates.
// The `-> !` return type means this function NEVER returns.
pub fn nova_error(line: usize, message: &str) -> ! {
    let full_msg = if line > 0 {
        format!("Error on line {}: {}", line, message)
    } else {
        format!("Error: {}", message)
    };
    LAST_ERROR.with(|e| *e.borrow_mut() = full_msg.clone());
    // only print to stderr if we're not inside a try block
    let in_try = TRY_DEPTH.with(|d| d.get() > 0);
    if !in_try {
        eprintln!("{}", full_msg);
    }
    // panic instead of process::exit so the REPL can catch errors with catch_unwind and keep running.
    // The string "nova_error" is a sentinel so main.rs can suppress the default Rust panic traceback.
    std::panic::panic_any("nova_error")
}
