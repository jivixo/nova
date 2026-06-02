// parser.rs — recursive descent parser. Turns a flat Vec<Token> into a tree of Expr nodes.
//
// How recursive descent works:
//   Each level of operator precedence is one function. The function at the TOP of the chain
//   handles the LOWEST-precedence operators (weakest binding). Each function calls the one
//   below it (higher precedence) to parse its operands.
//
//   Full precedence chain (top = weakest, bottom = strongest):
//     parse_null_coalesce  (??)
//       parse_or           (||)
//         parse_and        (&&)
//           parse_pipe     (|>)
//             parse_bitor  (|)
//               parse_bitxor (^)
//                 parse_bitand (&)
//                   parse_shift (<< >>)
//                     parse_comparison (== != < <= > >=)
//                       parse_expr     (+ -)
//                         parse_term   (* / %)
//                           parse_unary (- !)
//                             parse_primary (literals, calls, parens, if, match, ...)
//
//   Why this chain gives correct precedence:
//     parse_term calls parse_primary for its operands, so * never reaches + (which lives higher).
//     This means "1 + 2 * 3" becomes BinaryOp(+, 1, BinaryOp(*, 2, 3)) automatically.
//
// Every statement produced by parse_statement is wrapped in Expr::Line(line, inner) so that
// runtime errors can report the accurate line number of the failing statement.
use crate::lexer::Token;
use crate::error::nova_error;

// Expr is the AST node — every piece of Nova code becomes one of these after parsing.
// The parser turns a flat list of tokens into a nested tree of Exprs.
// Example: "1 + 2 * 3" becomes BinaryOp(+, IntLit(1), BinaryOp(*, IntLit(2), IntLit(3)))
// Box<Expr> is a heap pointer — needed because Expr contains Expr inside it (recursive).
// Rust requires every type to have a fixed size; Box breaks the recursion with a pointer.
#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i64),                              // an integer literal like 42
    FloatLit(f64),                            // a float literal like 3.14
    BoolLit(bool),                            // true or false
    NilLit,                                   // nil — the "no value" value
    StrLit(String),                           // a plain string like "hello"
    StrInterp(Vec<crate::lexer::StringPart>), // a string with {var} inside it

    Ident(String), // a variable being read, e.g. x in "print x"

    // a binary operation like 1 + 2 or x > 5
    // left and right are Boxes because they are Exprs containing Exprs (recursive)
    BinaryOp {
        left: Box<Expr>,  // the expression on the left side of the operator
        op: Token,        // which operator it is (Plus, Minus, EqualsEquals, etc.)
        right: Box<Expr>, // the expression on the right side
    },

    // let x = 10 — declares a variable
    Let {
        name: String,     // the variable name
        value: Box<Expr>, // the expression whose result gets stored
    },

    // if condition { ... } else { ... }
    // else_block is Option because the else branch is optional
    If {
        condition: Box<Expr>,        // must evaluate to a Bool
        then_block: Vec<Expr>,       // statements to run when condition is true
        else_block: Option<Vec<Expr>>, // statements to run when condition is false (may not exist)
    },

    // while condition { ... }
    While {
        condition: Box<Expr>, // re-evaluated every iteration
        body: Vec<Expr>,      // statements to run each iteration
    },

    Print(Box<Expr>),  // print <expr>  — evaluates and prints with a newline
    Printn(Box<Expr>), // printn <expr> — evaluates and prints without a newline

    // fn name(params) { body } — a named function declaration
    // variadic: true means the last param collects all extra args into an array
    // type annotations are optional: fn add(a: int, b: int): int { ... }
    // generics: fn first<T>(arr: [T]): T { ... }
    Fn {
        name: String,
        type_params: Vec<String>,              // generic type variables: ["T", "U"]
        params: Vec<(String, Option<String>, Option<Box<Expr>>)>, // (param_name, optional_type, optional_default)
        body: Vec<Expr>,
        variadic: bool,
        return_type: Option<String>,           // optional return type annotation
    },

    // (x) -> x * 2 or (x, y) -> { x + y } — an anonymous function (lambda)
    // Unlike Fn, it has no name and is used inline as a value
    Lambda {
        params: Vec<String>, // parameter names
        body: Vec<Expr>,     // the function body
    },

    // add(1, 2) — a function call by name
    Call {
        name: String,     // the function name to look up
        args: Vec<Expr>,  // the arguments to pass in
    },

    // expr(args) — calling the result of any expression: f(1)(2), arr[0](x), ((x)->x)(7)
    DynCall {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },

    Array(Vec<Expr>),               // [1, 2, 3] — an array literal
    HashMap(Vec<(Expr, Expr)>),     // {"key": value} — a hash map literal

    // arr[0] or map["key"] — reading a value at an index
    Index {
        object: Box<Expr>, // the array or map being indexed
        index: Box<Expr>,  // the key or position
    },

    // arr[0] = 5 — writing a value at an index
    // name is a String (not Box<Expr>) because we can only assign into named variables
    IndexAssign {
        name: String,     // the variable name holding the array
        index: Box<Expr>, // which position to write to
        value: Box<Expr>, // the new value to store
    },

    // for i in 0..10 { } or for item in arr { }
    For {
        var: String,      // the loop variable name (created fresh each iteration)
        iter: Box<Expr>,  // the range or array to iterate over
        body: Vec<Expr>,  // statements to run each iteration
    },

    // 0..10 — a range from start (inclusive) to end (exclusive)
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },

    // match x { 1 => ... _ => ... }
    // arms is a list of (pattern, body) pairs checked in order
    // pattern is None for the _ wildcard arm (matches anything)
    Match {
        value: Box<Expr>,
        arms: Vec<(Option<Expr>, Vec<Expr>)>,
    },

    // x = expr — reassigns an already-declared variable (no let needed after first declaration)
    Assign {
        name: String,
        value: Box<Expr>,
    },

    Not(Box<Expr>), // !expr — logical negation of a boolean

    // for i, item in arr — gives both the index and the value each iteration
    ForEnumerate {
        index_var: String,
        item_var: String,
        iter: Box<Expr>,
        body: Vec<Expr>,
    },

    // for [dx, dy] in arr — destructures each element into named variables
    ForDestructure {
        vars: Vec<String>,
        iter: Box<Expr>,
        body: Vec<Expr>,
    },

    Break,    // break — exits the innermost loop immediately
    Continue, // continue — skips the rest of this iteration and moves to the next
    Return(Box<Expr>), // return expr — exits the current function early with a value

    // try { body } catch name { handler } — run body; if throw is called, bind message to name and run handler
    Try {
        body: Vec<Expr>,
        catch_var: String,
        catch_body: Vec<Expr>,
    },

    Throw(Box<Expr>), // throw expr — raises an error with any value as the payload
    Spawn(Box<Expr>), // spawn expr — evaluates expr on a new thread, returns a Task handle
    Defer(Box<Expr>), // defer expr — schedules expr to run when the enclosing function returns (LIFO)

    // select { case ch -> v { body } ... default { body } }
    // if default is present: non-blocking — runs default if no channel is ready immediately.
    // if default is absent:  blocking — waits until a channel fires.
    Select {
        arms: Vec<(Box<Expr>, String, Vec<Expr>)>, // (channel_expr, bind_var, body_stmts)
        default_body: Option<Vec<Expr>>,           // Some(body) if a default arm was written
    },

    Import(String), // import "path.nova" — runs the file and merges its env into the current one

    // let [a, b, c] = arr  — bind each name to the corresponding array element
    LetArrayDestructure {
        names: Vec<String>,
        value: Box<Expr>,
    },

    // let {name, age} = map  — bind each name to map[name]
    LetMapDestructure {
        names: Vec<String>,
        value: Box<Expr>,
    },

    // struct Point { x, y } — defines a struct type
    StructDef { name: String, fields: Vec<String> },

    // Point { x: 1, y: 2 } — creates a struct instance
    StructLit { name: String, fields: Vec<(String, Expr)> },

    // p.x — reads a field from a struct
    FieldAccess { object: Box<Expr>, field: String },

    // p.x = 5 — writes a field on a struct
    FieldAssign { object: Box<Expr>, field: String, value: Box<Expr> },

    // enum Direction { North, South } or enum Shape { Circle(r), Rect(w, h) }
    EnumDef { name: String, variants: Vec<(String, usize)> },

    // match pattern only: Direction.North or Shape.Circle(r) — not a standalone expression
    EnumPattern { enum_name: String, variant: String, bindings: Vec<String> },

    // impl Point { fn distance(self) { ... } } — attaches methods to a struct type
    ImplBlock { type_name: String, methods: Vec<Expr> },

    // p.distance(args) — calls a method on a struct; object is pushed as `self`
    MethodCall { object: Box<Expr>, method: String, args: Vec<Expr> },

    // internal wrapper — carries the source line so the evaluator can report accurate errors
    // every statement produced by parse_statement is wrapped in one of these
    Line(usize, Box<Expr>),
}

// The Parser holds the token list and tracks where it currently is.
pub struct Parser {
    tokens: Vec<(Token, usize)>, // each token paired with the line number it was on
    pos: usize,                  // which token we're currently looking at
}

impl Parser {
    pub fn new(tokens: Vec<(Token, usize)>) -> Self {
        Parser { tokens, pos: 0 }
    }

    // Returns the current token WITHOUT consuming it (peek, not advance)
    pub fn current_token(&self) -> &Token {
        &self.tokens[self.pos].0 // .0 gets the Token from the (Token, usize) tuple
    }

    // Returns the line number of the current token — used in error messages
    pub fn current_line(&self) -> usize {
        self.tokens[self.pos].1 // .1 gets the line number
    }

    // Returns the current token AND moves pos forward by one (consume).
    // Clamps at the last token (EOF) so callers never go out of bounds.
    fn advance(&mut self) -> &Token {
        let token = &self.tokens[self.pos].0;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    // Looks ahead from a '(' to decide: is this a lambda or a grouped expression?
    // A lambda has only identifiers and commas inside (), followed by ->
    // e.g. (x) -> x * 2       ← lambda
    //      (1 + 2)            ← grouped expression
    // We scan forward WITHOUT consuming tokens so we can decide which path to take.
    fn is_lambda(&self) -> bool {
        let mut i = self.pos + 1; // start one past the ( we already see
        while i < self.tokens.len() {
            match &self.tokens[i].0 { // .0 to get the Token from the (Token, usize) tuple
                Token::Ident(_) => { i += 1; } // param name — keep scanning
                Token::Comma    => { i += 1; } // separator between params — keep scanning
                Token::RParen   => {
                    // reached the closing ) — check if the very next token is ->
                    return matches!(self.tokens.get(i + 1).map(|(t, _)| t), Some(Token::Arrow));
                }
                _ => return false, // found something that can't be in a param list — not a lambda
            }
        }
        false
    }

    // Reads a type annotation — plain name, "[T]" array type, or "(T, U) -> V" function type.
    fn parse_type_annotation(&mut self) -> String {
        if matches!(self.current_token(), Token::LBracket) {
            self.advance(); // skip [
            let inner = match self.advance().clone() {
                Token::Ident(t) => t,
                _ => nova_error(self.current_line(), "expected type name inside [...]"),
            };
            self.advance(); // skip ]
            format!("[{}]", inner)
        } else if matches!(self.current_token(), Token::LParen) {
            // function type: (T, U) -> V
            self.advance(); // skip (
            let mut param_types = vec![];
            while !matches!(self.current_token(), Token::RParen | Token::EOF) {
                match self.advance().clone() {
                    Token::Ident(t) => param_types.push(t),
                    _ => nova_error(self.current_line(), "expected type name in function type"),
                }
                if matches!(self.current_token(), Token::Comma) { self.advance(); }
            }
            self.advance(); // skip )
            self.advance(); // skip ->
            let ret = self.parse_type_annotation();
            format!("({}) -> {}", param_types.join(", "), ret)
        } else {
            match self.advance().clone() {
                Token::Ident(t) => t,
                _ => nova_error(self.current_line(), "expected type name after ':'"),
            }
        }
    }

    // Precedence chain — each method calls the one below it.
    // Lower in the chain = higher precedence (tighter binding).
    //
    //   parse_null_coalesce  (??)
    //     parse_pipe         (|)
    //       parse_comparison (== != < <= > >=)
    //         parse_expr     (+ -)
    //           parse_term   (* / %)
    //             parse_primary  (literals, calls, parens)
    //
    // This ordering ensures * always binds tighter than +, etc.

    // ?? — null coalescing, lowest precedence so it wraps everything else
    pub fn parse_null_coalesce(&mut self) -> Expr {
        let mut left = self.parse_or(); // parse the left side first

        while matches!(self.current_token(), Token::QuestionQuestion) {
            let op = self.advance().clone();
            let right = self.parse_or();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // || — logical OR: true if either side is true
    pub fn parse_or(&mut self) -> Expr {
        let mut left = self.parse_and();

        while matches!(self.current_token(), Token::Or) {
            let op = self.advance().clone();
            let right = self.parse_and();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // && — logical AND: true only if both sides are true
    pub fn parse_and(&mut self) -> Expr {
        let mut left = self.parse_pipe();

        while matches!(self.current_token(), Token::And) {
            let op = self.advance().clone();
            let right = self.parse_pipe();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // | — pipe operator
    // x | f(args) is rewritten to f(x, args) — x is inserted as the first argument
    pub fn parse_pipe(&mut self) -> Expr {
        let mut left = self.parse_comparison(); // parse the left side first

        while matches!(self.current_token(), Token::Pipe) {
            self.advance(); // consume |

            // print is a keyword not a function, so it needs a special case
            if matches!(self.current_token(), Token::Print) {
                self.advance(); // consume print
                left = Expr::Print(Box::new(left)); // wrap the left side in a Print node
                continue;
            }

            // read the function name after the |
            let name = match self.advance().clone() {
                Token::Ident(s) => s,
                t => nova_error(self.current_line(), &format!("expected function name after |, got {:?}", t)),
            };
            self.advance(); // skip (

            // the piped value goes in as the FIRST argument — args starts with left
            let mut args = vec![left];
            while !matches!(self.current_token(), Token::RParen) {
                args.push(self.parse_null_coalesce()); // parse additional arguments
                if matches!(self.current_token(), Token::Comma) {
                    self.advance(); // skip ,
                }
            }
            self.advance(); // skip )
            left = Expr::Call { name, args }; // build a normal function call with all args
        }

        left
    }

    // | — bitwise OR (lowest bitwise precedence)
    fn parse_bitor(&mut self) -> Expr {
        let mut left = self.parse_bitxor();
        while matches!(self.current_token(), Token::BitOr) {
            let op = self.advance().clone();
            let right = self.parse_bitxor();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }
        left
    }

    // ^ — bitwise XOR
    fn parse_bitxor(&mut self) -> Expr {
        let mut left = self.parse_bitand();
        while matches!(self.current_token(), Token::BitXor) {
            let op = self.advance().clone();
            let right = self.parse_bitand();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }
        left
    }

    // & — bitwise AND
    fn parse_bitand(&mut self) -> Expr {
        let mut left = self.parse_shift();
        while matches!(self.current_token(), Token::BitAnd) {
            let op = self.advance().clone();
            let right = self.parse_shift();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }
        left
    }

    // << >> — bit shifts (tightest bitwise, just above arithmetic)
    fn parse_shift(&mut self) -> Expr {
        let mut left = self.parse_expr();
        while matches!(self.current_token(), Token::Shl | Token::Shr) {
            let op = self.advance().clone();
            let right = self.parse_expr();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }
        left
    }

    // == != < <= > >= — comparison operators
    // Calls parse_bitor first so bitwise is resolved before comparing
    // e.g. x & mask == 0 correctly becomes (x & mask) == 0
    pub fn parse_comparison(&mut self) -> Expr {
        let mut left = self.parse_bitor();

        while matches!(self.current_token(),
            Token::EqualsEquals | Token::BangEquals |
            Token::Less | Token::LessEquals |
            Token::Greater | Token::GreaterEquals
        ) {
            let op = self.advance().clone();
            let right = self.parse_expr();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // + and - — addition and subtraction
    // Calls parse_term first so * and / bind tighter
    pub fn parse_expr(&mut self) -> Expr {
        let mut left = self.parse_term();

        while matches!(self.current_token(), Token::Plus | Token::Minus) {
            let op = self.advance().clone();
            let right = self.parse_term();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // * / % — multiplication, division, modulo (highest precedence binary operators)
    fn parse_term(&mut self) -> Expr {
        let mut left = self.parse_primary();

        while matches!(self.current_token(), Token::Star | Token::Slash | Token::Percent) {
            let op = self.advance().clone();
            let right = self.parse_primary();
            left = Expr::BinaryOp { left: Box::new(left), op, right: Box::new(right) };
        }

        left
    }

    // Literals, identifiers, calls, parenthesised expressions — tightest binding
    fn parse_primary(&mut self) -> Expr {
        let mut expr = match self.current_token().clone() {
            Token::IntLit(n)   => { self.advance(); Expr::IntLit(n) }
            Token::FloatLit(n) => { self.advance(); Expr::FloatLit(n) }
            Token::Bool(b)   => { self.advance(); Expr::BoolLit(b) }
            Token::Nil       => { self.advance(); Expr::NilLit }
            Token::StringLit(s)    => { self.advance(); Expr::StrLit(s) }
            Token::StrInterp(parts) => { self.advance(); Expr::StrInterp(parts) }

            // An identifier could be: a function call f(args), a struct literal Point { x: 1 },
            // or a plain variable read. Peek at the next token to decide.
            Token::Ident(s) => {
                self.advance();
                if matches!(self.current_token(), Token::LParen) && !self.is_lambda() {
                    self.advance(); // skip (
                    let mut args = Vec::new();
                    while !matches!(self.current_token(), Token::RParen) {
                        args.push(self.parse_null_coalesce());
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip )
                    Expr::Call { name: s, args }
                } else if matches!(self.current_token(), Token::LBrace)
                    && s.chars().next().map_or(false, |c| c.is_uppercase())
                {
                    // Struct literal: UppercaseName { field: expr, ... }
                    self.advance(); // skip {
                    let mut fields = Vec::new();
                    while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                        let fname = match self.advance().clone() {
                            Token::Ident(n) => n,
                            t => nova_error(self.current_line(), &format!("expected field name, got {:?}", t)),
                        };
                        self.advance(); // skip :
                        let fval = self.parse_null_coalesce();
                        fields.push((fname, fval));
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip }
                    Expr::StructLit { name: s, fields }
                } else {
                    Expr::Ident(s) // just reading a variable
                }
            }

            // ( could be a lambda (x) -> ... or a grouped expression (1 + 2)
            // is_lambda() peeks forward to decide which one it is
            Token::LParen => {
                if self.is_lambda() {
                    self.advance(); // skip (
                    let mut params = Vec::new();
                    while !matches!(self.current_token(), Token::RParen) {
                        if let Token::Ident(p) = self.advance().clone() { params.push(p); }
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip )
                    self.advance(); // skip ->
                    // body can be a single expression OR a block in {}
                    let body = if matches!(self.current_token(), Token::LBrace) {
                        self.parse_block()
                    } else {
                        vec![self.parse_null_coalesce()]
                    };
                    Expr::Lambda { params, body }
                } else {
                    self.advance(); // skip (
                    let expr = self.parse_null_coalesce(); // parse the inner expression at full precedence
                    self.advance(); // skip )
                    expr
                }
            }

            // [1, 2, 3] — array literal
            Token::LBracket => {
                self.advance(); // skip [
                let mut elements = Vec::new();
                while !matches!(self.current_token(), Token::RBracket) {
                    elements.push(self.parse_null_coalesce());
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip ]
                Expr::Array(elements)
            }

            // {"key": value} — hash map literal
            Token::LBrace => {
                self.advance(); // skip {
                let mut pairs = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    let key = self.parse_null_coalesce();
                    self.advance(); // skip :
                    let value = self.parse_null_coalesce();
                    pairs.push((key, value));
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip }
                Expr::HashMap(pairs)
            }

            // match can appear in expression position too: let x = match y { ... }
            Token::Match => {
                self.advance(); // skip match
                let value = self.parse_null_coalesce();
                self.advance(); // skip {
                let mut arms = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    let pattern = self.parse_match_pattern();
                    self.advance(); // skip =>
                    let body = if matches!(self.current_token(), Token::LBrace) {
                        self.parse_block()
                    } else {
                        vec![self.parse_statement()]
                    };
                    arms.push((pattern, body));
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip }
                Expr::Match { value: Box::new(value), arms }
            }

            // Unary minus: -5 is turned into BinaryOp(-, 0, 5)
            Token::Minus => {
                self.advance();
                let expr = self.parse_primary();
                Expr::BinaryOp {
                    left: Box::new(Expr::IntLit(0)),
                    op: Token::Minus,
                    right: Box::new(expr),
                }
            }

            // Logical NOT: !expr — negates a boolean
            Token::Bang => {
                self.advance();
                let expr = self.parse_primary();
                Expr::Not(Box::new(expr))
            }

            // if-else as expression: let x = if cond { a } else { b }
            Token::If => {
                self.advance();
                let condition = self.parse_null_coalesce();
                let then_block = self.parse_block();
                let else_block = if matches!(self.current_token(), Token::Else) {
                    self.advance();
                    if matches!(self.current_token(), Token::If) {
                        Some(vec![self.parse_statement()])
                    } else {
                        Some(self.parse_block())
                    }
                } else {
                    None
                };
                Expr::If { condition: Box::new(condition), then_block, else_block }
            }

            // throw as expression — valid inside lambdas, match arms, ?? fallbacks etc.
            Token::Throw => {
                self.advance();
                let value = self.parse_null_coalesce();
                Expr::Throw(Box::new(value))
            }

            // spawn as expression — let t = spawn compute(42)
            Token::Spawn => {
                self.advance();
                let inner = self.parse_null_coalesce();
                Expr::Spawn(Box::new(inner))
            }

            Token::Defer => {
                self.advance();
                let inner = self.parse_null_coalesce();
                Expr::Defer(Box::new(inner))
            }

            // select as expression — select { case ch -> v { body } ... default { body } }
            Token::Select => {
                self.advance(); // skip select
                self.advance(); // skip {
                let mut arms = Vec::new();
                let mut default_body: Option<Vec<Expr>> = None;
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    if matches!(self.current_token(), Token::Default) {
                        self.advance(); // skip default
                        default_body = Some(self.parse_block());
                        break; // default must be last arm
                    }
                    if !matches!(self.current_token(), Token::Case) {
                        nova_error(self.current_line(), "expected 'case' or 'default' inside select block");
                    }
                    self.advance(); // skip case
                    let ch_expr = self.parse_null_coalesce(); // channel expression
                    self.advance(); // skip ->
                    let bind_var = match self.advance().clone() {
                        Token::Ident(name) => name,
                        t => nova_error(self.current_line(), &format!("expected variable name after ->, got {:?}", t)),
                    };
                    let body = self.parse_block();
                    arms.push((Box::new(ch_expr), bind_var, body));
                }
                self.advance(); // skip outer }
                Expr::Select { arms, default_body }
            }

            _ => nova_error(self.current_line(), &format!("unexpected token {:?}", self.current_token())),
        };

        // After parsing the primary, check if .. follows — that makes it a range
        if matches!(self.current_token(), Token::DotDot) {
            self.advance(); // skip ..
            let end = self.parse_expr();
            return Expr::Range { start: Box::new(expr), end: Box::new(end) };
        }

        // Chain field access, index accesses, and dynamic calls: p.x, arr[0][1](args), etc.
        loop {
            if matches!(self.current_token(), Token::Dot) {
                self.advance(); // skip .
                let field = match self.advance().clone() {
                    Token::Ident(f) => f,
                    t => nova_error(self.current_line(), &format!("expected field name after '.', got {:?}", t)),
                };
                // p.method(args) → MethodCall; p.field → FieldAccess
                if matches!(self.current_token(), Token::LParen) {
                    self.advance(); // skip (
                    let mut args = Vec::new();
                    while !matches!(self.current_token(), Token::RParen | Token::EOF) {
                        args.push(self.parse_null_coalesce());
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip )
                    expr = Expr::MethodCall { object: Box::new(expr), method: field, args };
                } else {
                    expr = Expr::FieldAccess { object: Box::new(expr), field };
                }
            } else if matches!(self.current_token(), Token::LBracket) {
                self.advance(); // skip [
                let index = self.parse_null_coalesce();
                self.advance(); // skip ]
                expr = Expr::Index { object: Box::new(expr), index: Box::new(index) };
            } else if matches!(self.current_token(), Token::LParen) && !self.is_lambda() {
                // Guard: if the ( starts a lambda (x) -> ..., it belongs to the NEXT statement —
                // don't consume it as a DynCall argument list (e.g. `let x = 5\n(y) -> y+x`).
                self.advance(); // skip (
                let mut args = Vec::new();
                while !matches!(self.current_token(), Token::RParen | Token::EOF) {
                    args.push(self.parse_null_coalesce());
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip )
                expr = Expr::DynCall { callee: Box::new(expr), args };
            } else {
                break;
            }
        }

        expr
    }

    // Parses a match arm pattern. Returns None for wildcard `_`, Some(EnumPattern) for
    // `EnumName.Variant` or `EnumName.Variant(a, b)`, or Some(expr) for a literal/range pattern.
    fn parse_match_pattern(&mut self) -> Option<Expr> {
        if let Token::Ident(s) = self.current_token().clone() {
            if s == "_" {
                self.advance();
                return None;
            }
            if s.chars().next().map_or(false, |c| c.is_uppercase()) {
                if self.tokens.get(self.pos + 1).map_or(false, |(t, _)| matches!(t, Token::Dot)) {
                    let enum_name = s;
                    self.advance(); // consume EnumName
                    self.advance(); // consume .
                    let variant = match self.advance().clone() {
                        Token::Ident(v) => v,
                        t => nova_error(self.current_line(), &format!("expected variant name, got {:?}", t)),
                    };
                    let bindings = if matches!(self.current_token(), Token::LParen) {
                        self.advance(); // skip (
                        let mut bs = Vec::new();
                        while !matches!(self.current_token(), Token::RParen | Token::EOF) {
                            if let Token::Ident(b) = self.advance().clone() { bs.push(b); }
                            if matches!(self.current_token(), Token::Comma) { self.advance(); }
                        }
                        self.advance(); // skip )
                        bs
                    } else {
                        Vec::new()
                    };
                    return Some(Expr::EnumPattern { enum_name, variant, bindings });
                }
            }
        }
        Some(self.parse_null_coalesce())
    }

    // Parses a { ... } block and returns all the statements inside as a Vec<Expr>
    fn parse_block(&mut self) -> Vec<Expr> {
        self.advance(); // skip {
        let mut exprs = Vec::new();
        while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
            exprs.push(self.parse_statement());
            if matches!(self.current_token(), Token::Semicolon) { self.advance(); }
        }
        self.advance(); // skip }
        exprs
    }

    // Parses a single expression — used by the evaluator to evaluate interp expressions
    pub fn parse_expression(&mut self) -> Expr {
        self.parse_null_coalesce()
    }

    // Skips a semicolon if one is present — called by top-level parse loops
    pub fn skip_optional_semicolon(&mut self) {
        if matches!(self.current_token(), Token::Semicolon) { self.advance(); }
    }

    // Top-level entry point — parses one complete statement
    // Called by main.rs in a loop and by parse_block for nested statements
    pub fn parse_statement(&mut self) -> Expr {
        let line = self.current_line(); // capture line before parsing so errors point to the right place
        let stmt = match self.current_token().clone() {

            Token::Let => {
                self.advance(); // skip let
                // let [a, b, c] = arr  — array destructuring
                if matches!(self.current_token(), Token::LBracket) {
                    self.advance(); // skip [
                    let mut names = Vec::new();
                    while !matches!(self.current_token(), Token::RBracket | Token::EOF) {
                        if let Token::Ident(n) = self.advance().clone() { names.push(n); }
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip ]
                    self.advance(); // skip =
                    let value = self.parse_null_coalesce();
                    return Expr::Line(line, Box::new(Expr::LetArrayDestructure { names, value: Box::new(value) }));
                }
                // let {name, age} = map  — hashmap destructuring
                if matches!(self.current_token(), Token::LBrace) {
                    self.advance(); // skip {
                    let mut names = Vec::new();
                    while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                        if let Token::Ident(n) = self.advance().clone() { names.push(n); }
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip }
                    self.advance(); // skip =
                    let value = self.parse_null_coalesce();
                    return Expr::Line(line, Box::new(Expr::LetMapDestructure { names, value: Box::new(value) }));
                }
                let name = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected variable name after 'let'"),
                };
                self.advance(); // skip =
                let value = self.parse_null_coalesce(); // parse the right-hand side at full precedence
                Expr::Let { name, value: Box::new(value) }
            }

            Token::If => {
                self.advance(); // skip if
                let condition = self.parse_null_coalesce();
                let then_block = self.parse_block();
                let else_block = if matches!(self.current_token(), Token::Else) {
                    self.advance(); // skip else
                    if matches!(self.current_token(), Token::If) {
                        // else if — parse the next if as a single statement inside an implicit block
                        Some(vec![self.parse_statement()])
                    } else {
                        Some(self.parse_block())
                    }
                } else {
                    None
                };
                Expr::If { condition: Box::new(condition), then_block, else_block }
            }

            Token::While => {
                self.advance(); // skip while
                let condition = self.parse_null_coalesce();
                let body = self.parse_block();
                Expr::While { condition: Box::new(condition), body }
            }

            Token::Print => {
                self.advance(); // skip print
                let value = self.parse_null_coalesce();
                Expr::Print(Box::new(value))
            }

            Token::Printn => {
                self.advance(); // skip printn
                let value = self.parse_null_coalesce();
                Expr::Printn(Box::new(value))
            }

            // fn name(a, b) { body } or fn name(a: int, b: int): int { body }
            // generics: fn first<T>(arr: [T]): T { body }
            Token::Fn => {
                self.advance(); // skip fn
                let name = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected function name after 'fn'"),
                };
                // optional generic type parameters: <T, U, ...>
                let mut type_params: Vec<String> = Vec::new();
                if matches!(self.current_token(), Token::Less) {
                    self.advance(); // skip <
                    while !matches!(self.current_token(), Token::Greater | Token::EOF) {
                        if let Token::Ident(t) = self.advance().clone() { type_params.push(t); }
                        if matches!(self.current_token(), Token::Comma) { self.advance(); }
                    }
                    self.advance(); // skip >
                }
                self.advance(); // skip (
                let mut params: Vec<(String, Option<String>, Option<Box<Expr>>)> = Vec::new();
                let mut variadic = false;
                while !matches!(self.current_token(), Token::RParen) {
                    let param_name = match self.advance().clone() {
                        Token::Ident(p) => p,
                        _ => nova_error(self.current_line(), "expected parameter name"),
                    };
                    // optional type annotation: param: TypeName or param: [TypeName]
                    let param_type = if matches!(self.current_token(), Token::Colon) {
                        self.advance(); // skip :
                        Some(self.parse_type_annotation())
                    } else {
                        None
                    };
                    // optional default value: param = expr
                    let param_default = if matches!(self.current_token(), Token::Equals) {
                        self.advance(); // skip =
                        Some(Box::new(self.parse_null_coalesce()))
                    } else {
                        None
                    };
                    params.push((param_name, param_type, param_default));
                    if matches!(self.current_token(), Token::Ellipsis) {
                        self.advance(); // skip ...
                        variadic = true;
                        break;
                    }
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip )
                // optional return type annotation: -> TypeName or : TypeName (both accepted)
                let return_type = if matches!(self.current_token(), Token::Arrow | Token::Colon) {
                    self.advance(); // skip -> or :
                    Some(self.parse_type_annotation())
                } else {
                    None
                };
                let body = self.parse_block();
                Expr::Fn { name, type_params, params, body, variadic, return_type }
            }

            Token::For => {
                self.advance(); // skip for
                // destructure syntax: for [a, b, ...] in arr
                if matches!(self.current_token(), Token::LBracket) {
                    self.advance(); // skip [
                    let mut vars = vec![];
                    loop {
                        match self.advance().clone() {
                            Token::Ident(s) => vars.push(s),
                            _ => nova_error(self.current_line(), "expected identifier in destructure pattern"),
                        }
                        match self.current_token() {
                            Token::Comma => { self.advance(); }
                            Token::RBracket => { self.advance(); break; }
                            _ => nova_error(self.current_line(), "expected ',' or ']' in destructure pattern"),
                        }
                    }
                    self.advance(); // skip in
                    let iter = self.parse_null_coalesce();
                    let body = self.parse_block();
                    return Expr::ForDestructure { vars, iter: Box::new(iter), body };
                }
                let var = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected variable name after 'for'"),
                };
                // check for enumerate syntax: for i, item in arr
                if matches!(self.current_token(), Token::Comma) {
                    self.advance(); // skip ,
                    let item_var = match self.advance().clone() {
                        Token::Ident(s) => s,
                        _ => nova_error(self.current_line(), "expected variable name after ','"),
                    };
                    self.advance(); // skip in
                    let iter = self.parse_null_coalesce();
                    let body = self.parse_block();
                    Expr::ForEnumerate { index_var: var, item_var, iter: Box::new(iter), body }
                } else {
                    self.advance(); // skip in
                    let iter = self.parse_null_coalesce();
                    let body = self.parse_block();
                    Expr::For { var, iter: Box::new(iter), body }
                }
            }

            Token::Break => {
                self.advance(); // consume break
                Expr::Break
            }

            Token::Continue => {
                self.advance(); // consume continue
                Expr::Continue
            }

            Token::Return => {
                self.advance(); // consume return
                // bare return with no value returns nil
                let value = if matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    Expr::NilLit
                } else {
                    self.parse_null_coalesce()
                };
                Expr::Return(Box::new(value))
            }

            Token::Match => {
                self.advance(); // skip match
                let value = self.parse_null_coalesce();
                self.advance(); // skip {
                let mut arms = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    let pattern = self.parse_match_pattern();
                    self.advance(); // skip =>
                    let body = if matches!(self.current_token(), Token::LBrace) {
                        self.parse_block()
                    } else {
                        vec![self.parse_statement()]
                    };
                    arms.push((pattern, body));
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip }
                Expr::Match { value: Box::new(value), arms }
            }

            Token::Try => {
                self.advance(); // skip try
                let body = self.parse_block();
                // expect: catch <name> { ... }
                if !matches!(self.current_token(), Token::Catch) {
                    nova_error(self.current_line(), "expected 'catch' after try block");
                }
                self.advance(); // skip catch
                let catch_var = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected variable name after 'catch'"),
                };
                let catch_body = self.parse_block();
                Expr::Try { body, catch_var, catch_body }
            }

            Token::Throw => {
                self.advance(); // skip throw
                let value = self.parse_null_coalesce();
                Expr::Throw(Box::new(value))
            }

            Token::Spawn => {
                self.advance(); // skip spawn
                let inner = self.parse_null_coalesce();
                Expr::Spawn(Box::new(inner))
            }

            Token::Defer => {
                self.advance(); // skip defer
                let inner = self.parse_null_coalesce();
                Expr::Defer(Box::new(inner))
            }

            Token::Select => {
                self.advance(); // skip select
                self.advance(); // skip {
                let mut arms = Vec::new();
                let mut default_body: Option<Vec<Expr>> = None;
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    if matches!(self.current_token(), Token::Default) {
                        self.advance(); // skip default
                        default_body = Some(self.parse_block());
                        break;
                    }
                    if !matches!(self.current_token(), Token::Case) {
                        nova_error(self.current_line(), "expected 'case' or 'default' inside select block");
                    }
                    self.advance(); // skip case
                    let ch_expr = self.parse_null_coalesce();
                    self.advance(); // skip ->
                    let bind_var = match self.advance().clone() {
                        Token::Ident(name) => name,
                        t => nova_error(self.current_line(), &format!("expected variable name after ->, got {:?}", t)),
                    };
                    let body = self.parse_block();
                    arms.push((Box::new(ch_expr), bind_var, body));
                }
                self.advance(); // skip outer }
                Expr::Select { arms, default_body }
            }

            Token::Import => {
                self.advance(); // skip import
                let path = match self.advance().clone() {
                    Token::StringLit(s) => s,
                    _ => nova_error(self.current_line(), "expected string path after 'import'"),
                };
                Expr::Import(path)
            }

            Token::Struct => {
                self.advance(); // skip struct
                let name = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected struct name"),
                };
                self.advance(); // skip {
                let mut fields = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    if let Token::Ident(f) = self.advance().clone() { fields.push(f); }
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip }
                Expr::StructDef { name, fields }
            }

            Token::Enum => {
                self.advance(); // skip enum
                let name = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected enum name"),
                };
                self.advance(); // skip {
                let mut variants = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    let vname = match self.advance().clone() {
                        Token::Ident(s) => s,
                        _ => nova_error(self.current_line(), "expected variant name"),
                    };
                    let arity = if matches!(self.current_token(), Token::LParen) {
                        self.advance(); // skip (
                        let mut count = 0;
                        while !matches!(self.current_token(), Token::RParen | Token::EOF) {
                            self.advance(); // consume field name (just for counting)
                            count += 1;
                            if matches!(self.current_token(), Token::Comma) { self.advance(); }
                        }
                        self.advance(); // skip )
                        count
                    } else {
                        0
                    };
                    variants.push((vname, arity));
                    if matches!(self.current_token(), Token::Comma) { self.advance(); }
                }
                self.advance(); // skip }
                Expr::EnumDef { name, variants }
            }

            Token::Impl => {
                self.advance(); // skip impl
                let type_name = match self.advance().clone() {
                    Token::Ident(s) => s,
                    _ => nova_error(self.current_line(), "expected type name after 'impl'"),
                };
                self.advance(); // skip {
                let mut methods = Vec::new();
                while !matches!(self.current_token(), Token::RBrace | Token::EOF) {
                    // each method is a fn definition — reuse the existing Fn parse path
                    let method = self.parse_statement();
                    methods.push(method);
                }
                self.advance(); // skip }
                Expr::ImplBlock { type_name, methods }
            }

            // Everything else is either a plain expression or an index assignment.
            // We parse the left side first, then check if = follows.
            // If it does, and the left side was arr[i], it becomes an IndexAssign.
            _ => {
                let expr = self.parse_null_coalesce();

                // compound assignment: x += 5 desugars to x = x + 5
                let compound_op = match self.current_token() {
                    Token::PlusEquals  => Some(Token::Plus),
                    Token::MinusEquals => Some(Token::Minus),
                    Token::StarEquals  => Some(Token::Star),
                    Token::SlashEquals => Some(Token::Slash),
                    _ => None,
                };
                if let Some(op) = compound_op {
                    match expr {
                        Expr::Ident(name) => {
                            self.advance(); // skip +=, -=, *=, /=
                            let value = self.parse_null_coalesce();
                            return Expr::Line(self.current_line(), Box::new(Expr::Assign {
                                name: name.clone(),
                                value: Box::new(Expr::BinaryOp {
                                    left:  Box::new(Expr::Ident(name)),
                                    op,
                                    right: Box::new(value),
                                }),
                            }));
                        }
                        Expr::FieldAccess { object, field } => {
                            self.advance(); // skip +=, -=, *=, /=
                            let rhs = self.parse_null_coalesce();
                            // p.x += 5  →  p.x = p.x + 5
                            return Expr::Line(self.current_line(), Box::new(Expr::FieldAssign {
                                object: object.clone(),
                                field: field.clone(),
                                value: Box::new(Expr::BinaryOp {
                                    left:  Box::new(Expr::FieldAccess { object, field }),
                                    op,
                                    right: Box::new(rhs),
                                }),
                            }));
                        }
                        _ => nova_error(self.current_line(), "compound assignment requires a variable or field on the left"),
                    }
                }

                if matches!(self.current_token(), Token::Equals) {
                    match expr {
                        Expr::Ident(name) => {
                            self.advance(); // skip =
                            let value = self.parse_null_coalesce();
                            Expr::Assign { name, value: Box::new(value) }
                        }
                        Expr::Index { object, index } => {
                            if let Expr::Ident(name) = *object {
                                self.advance(); // skip =
                                let value = self.parse_null_coalesce();
                                Expr::IndexAssign { name, index, value: Box::new(value) }
                            } else {
                                nova_error(self.current_line(), "can only assign into a named variable")
                            }
                        }
                        Expr::FieldAccess { object, field } => {
                            self.advance(); // skip =
                            let value = self.parse_null_coalesce();
                            Expr::FieldAssign { object, field, value: Box::new(value) }
                        }
                        _ => nova_error(self.current_line(), "invalid assignment target"),
                    }
                } else {
                    expr
                }
            }
        };
        Expr::Line(line, Box::new(stmt)) // wrap with line number for accurate runtime error reporting
    }
}