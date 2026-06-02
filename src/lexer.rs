// StringPart represents one segment of an interpolated string.
// "hello {name}!" can't be stored as a plain string because {name} needs to be
// substituted at runtime, so the lexer splits it into pieces:
//   [Literal("hello "), Interp("name"), Literal("!")]
// The evaluator then replaces each Interp with the variable's value from env.
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String), // plain text — printed as-is
    Interp(String),  // variable name inside { } — looked up in env at runtime
}

// The lexer turns raw text into a list of these before the parser sees it.
// Example: "let x = 5" becomes [Let, Ident("x"), Equals, Number(5.0), EOF]
//
// #[derive(Debug, Clone, PartialEq)] auto-generates three things:
//   Debug     — lets you print a token with {:?} (used in error messages)
//   Clone     — lets you copy a token when you need an independent version of it
//   PartialEq — lets you compare two tokens with ==
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    IntLit(i64),                  // an integer literal like 42 or -5
    FloatLit(f64),                // a decimal literal like 3.14 or 2.0
    StringLit(String),            // a plain string like "hello" with no {var} inside
    StrInterp(Vec<StringPart>),   // a string like "hi {name}" that has at least one {var}
    Bool(bool),                   // true or false

    Ident(String), // a user-defined name like x, myVar, add
    Let,           // the keyword "let"
    Fn,            // the keyword "fn"
    If,            // the keyword "if"
    Else,          // the keyword "else"
    While,         // the keyword "while"
    Return,        // the keyword "return"
    Break,         // the keyword "break" — exits the current loop immediately
    Continue,      // the keyword "continue" — skips to the next loop iteration
    Print,         // the keyword "print"  — prints with a newline
    Printn,        // the keyword "printn" — prints without a newline (stays on same line)
    In,            // the keyword "in" (used in: for x in arr)
    For,           // the keyword "for"
    Nil,           // the keyword "nil" — the no-value value
    Match,         // the keyword "match"
    Try,           // the keyword "try"
    Catch,         // the keyword "catch"
    Throw,         // the keyword "throw"
    Import,        // the keyword "import"
    Struct,        // the keyword "struct"
    Enum,          // the keyword "enum"
    Impl,          // the keyword "impl"
    Spawn,         // the keyword "spawn" — launch a task on a new thread
    Select,        // the keyword "select" — wait on multiple channels simultaneously
    Case,          // the keyword "case" — one arm inside a select block
    Default,       // the keyword "default" — fallback arm in a non-blocking select
    Defer,         // the keyword "defer" — schedule a call to run when the enclosing function returns

    Plus,             // +
    Minus,            // -
    Star,             // *
    Slash,            // /
    PlusEquals,       // +=
    MinusEquals,      // -=
    StarEquals,       // *=
    SlashEquals,      // /=
    Percent,          // %   (modulo — remainder after division, e.g. 7 % 3 = 1)
    Bang,             // !   (logical NOT)
    Equals,           // =   (assignment, not comparison)
    EqualsEquals,     // ==  (equality check)
    BangEquals,       // !=  (not-equal check)
    Less,             // <
    LessEquals,       // <=
    Greater,          // >
    GreaterEquals,    // >=
    And,              // &&  (logical AND — true only if BOTH sides are true)
    Or,               // ||  (logical OR  — true if EITHER side is true)
    Pipe,             // |>  (pipe: x |> f() rewrites to f(x))
    QuestionQuestion, // ??  (null coalescing: use left value unless it's nil)
    BitAnd,           // &   (bitwise AND)
    BitOr,            // |   (bitwise OR)
    BitXor,           // ^   (bitwise XOR)
    Shl,              // <<  (left shift)
    Shr,              // >>  (right shift)
    Dot,              // .   (field access: p.x)
    DotDot,           // ..  (range: 0..10 means 0,1,2,...,9)
    Ellipsis,         // ... (variadic marker: fn f(args...) collects all args into an array)

    LParen,    // (
    RParen,    // )
    LBrace,    // {
    RBrace,    // }
    LBracket,  // [
    RBracket,  // ]
    Comma,     // ,
    Semicolon, // ;
    Colon,     // :  (used in hash map literals: {"key": value})
    Arrow,     // -> (lambda body separator: (x) -> x * 2)
    FatArrow,  // => (match arm separator: 1 => "one")

    EOF, // signals the end of the file so the parser knows when to stop
}

// Lexer holds the source code and tracks where we currently are while reading.
pub struct Lexer {
    source: Vec<char>, // source stored as individual characters rather than a raw string,
                       // because indexing into UTF-8 text by character is tricky in Rust —
                       // Vec<char> makes it simple and safe
    pos: usize,        // index of the character we're currently looking at
    line: usize,       // which line we're currently on (starts at 1)
}

impl Lexer {
    // Creates a new Lexer from a source code string.
    // .chars() iterates over Unicode characters; .collect() gathers them into a Vec<char>.
    pub fn new(source: &str) -> Self {
        Lexer {
            source: source.chars().collect(),
            pos: 0,
            line: 1, // line counting starts at 1
        }
    }

    // Returns the character at the current position WITHOUT consuming it (peek).
    // .get(pos) returns Option<&char>; .copied() converts &char → char.
    // Returns None when we've read past the end of the source.
    fn current(&self) -> Option<char> {
        self.source.get(self.pos).copied()
    }

    // Moves pos forward by one — "consumes" the current character.
    fn advance(&mut self) {
        self.pos += 1;
    }

    // The main loop — reads every character and produces a Vec<(Token, usize)>.
    // Each token is paired with the line number it appeared on.
    pub fn tokenize(&mut self) -> Vec<(Token, usize)> {
        let mut tokens = Vec::new();

        while let Some(c) = self.current() {
            match c {
                // Newlines increment the line counter so tokens after them get the right line number.
                // \r (carriage return on Windows) is skipped without incrementing — \n does the counting.
                '\n' => { self.line += 1; self.advance(); }
                ' ' | '\t' | '\r' => self.advance(),

                // Numbers, strings, identifiers are multi-character — delegate to helpers
                // self.line is captured at the START of the token so multi-char tokens
                // report the line they began on, not where they ended
                '0'..='9' => { let l = self.line; tokens.push((self.read_number(), l)); }

                // Check for triple-quote """ BEFORE single " so multiline strings are
                // detected first — otherwise we'd read three separate empty strings
                '"' => {
                    let l = self.line;
                    if self.source.get(self.pos + 1).copied() == Some('"')
                        && self.source.get(self.pos + 2).copied() == Some('"')
                    {
                        tokens.push((self.read_multiline_string(), l));
                    } else {
                        tokens.push((self.read_string(), l));
                    }
                }

                'a'..='z' | 'A'..='Z' | '_' => { let l = self.line; tokens.push((self.read_ident(), l)); }

                // Single-character tokens — push (token, current line) and advance
                '+' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('=') {
                        tokens.push((Token::PlusEquals, l)); self.advance(); // +=
                    } else {
                        tokens.push((Token::Plus, l));                       // +
                    }
                }
                '*' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('=') {
                        tokens.push((Token::StarEquals, l)); self.advance(); // *=
                    } else {
                        tokens.push((Token::Star, l));                       // *
                    }
                }

                '/' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('/') {
                        // comment — skip until end of line
                        while let Some(c) = self.current() {
                            if c == '\n' { break; }
                            self.advance();
                        }
                    } else if self.current() == Some('=') {
                        tokens.push((Token::SlashEquals, l)); self.advance(); // /=
                    } else {
                        tokens.push((Token::Slash, l));                       // /
                    }
                }

                '%' => { tokens.push((Token::Percent,   self.line)); self.advance(); }
                '(' => { tokens.push((Token::LParen,    self.line)); self.advance(); }
                ')' => { tokens.push((Token::RParen,    self.line)); self.advance(); }
                '{' => { tokens.push((Token::LBrace,    self.line)); self.advance(); }
                '}' => { tokens.push((Token::RBrace,    self.line)); self.advance(); }
                '[' => { tokens.push((Token::LBracket,  self.line)); self.advance(); }
                ']' => { tokens.push((Token::RBracket,  self.line)); self.advance(); }
                ',' => { tokens.push((Token::Comma,     self.line)); self.advance(); }
                ';' => { tokens.push((Token::Semicolon, self.line)); self.advance(); }
                ':' => { tokens.push((Token::Colon,     self.line)); self.advance(); }

                '-' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('>') {
                        tokens.push((Token::Arrow, l)); self.advance();       // ->
                    } else if self.current() == Some('=') {
                        tokens.push((Token::MinusEquals, l)); self.advance(); // -=
                    } else {
                        tokens.push((Token::Minus, l));                       // -
                    }
                }

                '.' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('.') {
                        self.advance();
                        if self.current() == Some('.') {
                            tokens.push((Token::Ellipsis, l)); self.advance(); // ...
                        } else {
                            tokens.push((Token::DotDot, l));                   // ..
                        }
                    } else {
                        tokens.push((Token::Dot, l));                          // .
                    }
                }

                '=' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('=') {
                        tokens.push((Token::EqualsEquals, l)); self.advance(); // ==
                    } else if self.current() == Some('>') {
                        tokens.push((Token::FatArrow, l)); self.advance();     // =>
                    } else {
                        tokens.push((Token::Equals, l));                       // =
                    }
                }

                '!' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('=') {
                        tokens.push((Token::BangEquals, l)); self.advance(); // !=
                    } else {
                        tokens.push((Token::Bang, l));                       // !
                    }
                }

                '<' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('<') {
                        tokens.push((Token::Shl, l)); self.advance();        // <<
                    } else if self.current() == Some('=') {
                        tokens.push((Token::LessEquals, l)); self.advance(); // <=
                    } else {
                        tokens.push((Token::Less, l));                       // <
                    }
                }

                '>' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('>') {
                        tokens.push((Token::Shr, l)); self.advance();           // >>
                    } else if self.current() == Some('=') {
                        tokens.push((Token::GreaterEquals, l)); self.advance(); // >=
                    } else {
                        tokens.push((Token::Greater, l));                       // >
                    }
                }

                '&' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('&') {
                        tokens.push((Token::And,    l)); self.advance(); // &&
                    } else {
                        tokens.push((Token::BitAnd, l));                 // &
                    }
                }

                '|' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('|') {
                        tokens.push((Token::Or,     l)); self.advance(); // ||
                    } else if self.current() == Some('>') {
                        tokens.push((Token::Pipe,   l)); self.advance(); // |>
                    } else {
                        tokens.push((Token::BitOr,  l));                 // |
                    }
                }

                '^' => {
                    let l = self.line;
                    self.advance();
                    tokens.push((Token::BitXor, l)); // ^
                }

                '?' => {
                    let l = self.line;
                    self.advance();
                    if self.current() == Some('?') {
                        tokens.push((Token::QuestionQuestion, l)); self.advance(); // ??
                    }
                }

                _ => { self.advance(); }
            }
        }

        tokens.push((Token::EOF, self.line)); // EOF also gets a line number
        tokens
    }

    // Reads an integer or decimal number.
    // If the literal contains a '.', returns FloatLit. Otherwise returns IntLit.
    // The '..' range operator is not consumed — "1..10" stops at the first dot.
    fn read_number(&mut self) -> Token {
        let start = self.pos;
        let mut is_float = false;
        while let Some(c) = self.current() {
            if c.is_ascii_digit() {
                self.advance();
            } else if c == '.' && self.source.get(self.pos + 1).copied() != Some('.') {
                // consume a decimal point only if the next char is not also a dot
                // (avoids misreading "1..10" as float "1." + ".10")
                is_float = true;
                self.advance();
            } else {
                break;
            }
        }
        let num_str: String = self.source[start..self.pos].iter().collect();
        if is_float {
            Token::FloatLit(num_str.parse().unwrap())
        } else {
            Token::IntLit(num_str.parse().unwrap())
        }
    }

    // Reads a "..." string with escape sequences and {var} interpolation.
    // Builds a Vec<StringPart> so interpolated variables can be resolved at runtime.
    // If there are no {var} sections, collapses everything into a plain StringLit.
    fn read_string(&mut self) -> Token {
        self.advance(); // skip the opening "

        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new(); // buffer for the current plain-text segment
        let mut has_interp = false;      // flipped to true when we encounter a {var}

        while let Some(c) = self.current() {
            if c == '"' { break; } // closing quote — stop reading

            if c == '\\' {
                // escape sequence — backslash followed by a special character
                self.advance(); // consume the backslash
                match self.current() {
                    Some('n')  => { current.push('\n'); self.advance(); } // \n → newline
                    Some('t')  => { current.push('\t'); self.advance(); } // \t → tab
                    Some('"')  => { current.push('"');  self.advance(); } // \" → literal quote
                    Some('\\') => { current.push('\\'); self.advance(); } // \\ → literal backslash
                    _ => { current.push('\\'); } // unknown escape — keep the backslash as-is
                }
            } else if c == '{' {
                // start of interpolation — save the plain text so far, then read the variable name
                self.advance(); // skip {
                if !current.is_empty() {
                    parts.push(StringPart::Literal(current.clone())); // flush the text buffer as a Literal segment
                    current.clear(); // reset the buffer for text after the }
                }
                let mut var_name = String::new();
                while let Some(c) = self.current() {
                    if c == '}' { self.advance(); break; } // closing brace — stop reading the variable name
                    var_name.push(c);
                    self.advance();
                }
                if var_name.is_empty() {
                    // {} with no expression inside — treat as literal "{}" text
                    current.push('{');
                    current.push('}');
                } else {
                    parts.push(StringPart::Interp(var_name)); // store the variable name for runtime lookup
                    has_interp = true;
                }
            } else {
                current.push(c); // plain character — add to the text buffer
                self.advance();
            }
        }
        self.advance(); // skip the closing "

        if !current.is_empty() {
            parts.push(StringPart::Literal(current)); // flush any remaining plain text
        }

        if has_interp {
            Token::StrInterp(parts) // contains {var} — evaluator must substitute values
        } else {
            // no interpolation — collapse all Literal parts into a single plain string
            Token::StringLit(parts.into_iter().map(|p| match p {
                StringPart::Literal(s) => s,
                _ => unreachable!(), // has_interp is false so there are no Interp parts
            }).collect::<Vec<_>>().join(""))
        }
    }

    // Reads a """...""" triple-quoted multiline string.
    // Preserves all whitespace and newlines exactly as written inside the quotes.
    // Does not support {var} interpolation — content is always a plain StringLit.
    fn read_multiline_string(&mut self) -> Token {
        self.advance(); self.advance(); self.advance(); // skip the three opening " characters

        let mut content = String::new();
        loop {
            // check if the next three characters are """ (the closing delimiter)
            if self.current() == Some('"')
                && self.source.get(self.pos + 1).copied() == Some('"')
                && self.source.get(self.pos + 2).copied() == Some('"')
            {
                self.advance(); self.advance(); self.advance(); // skip closing """
                break;
            }
            match self.current() {
                Some(c) => { content.push(c); self.advance(); }
                None    => break, // hit end of file before closing """ — stop gracefully
            }
        }
        Token::StringLit(content)
    }

    // Reads an identifier or keyword.
    // Consumes letters, digits, and underscores, then checks if the result is a reserved word.
    // Example: "let"   → Token::Let
    //          "myVar" → Token::Ident("myVar")
    fn read_ident(&mut self) -> Token {
        let start = self.pos;
        while let Some(c) = self.current() {
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }
        let word: String = self.source[start..self.pos].iter().collect(); // gather the characters into a string

        // check against every reserved keyword; anything else is a user-defined name
        match word.as_str() {
            "let"    => Token::Let,
            "fn"     => Token::Fn,
            "if"     => Token::If,
            "else"   => Token::Else,
            "while"  => Token::While,
            "return" => Token::Return,
            "print"  => Token::Print,
            "printn" => Token::Printn,
            "true"   => Token::Bool(true),
            "false"  => Token::Bool(false),
            "in"     => Token::In,
            "for"    => Token::For,
            "nil"    => Token::Nil,
            "match"  => Token::Match,
            "break"    => Token::Break,
            "continue" => Token::Continue,
            "try"      => Token::Try,
            "catch"    => Token::Catch,
            "throw"    => Token::Throw,
            "import"   => Token::Import,
            "struct"   => Token::Struct,
            "enum"     => Token::Enum,
            "impl"     => Token::Impl,
            "spawn"    => Token::Spawn,
            "select"   => Token::Select,
            "case"     => Token::Case,
            "default"  => Token::Default,
            "defer"    => Token::Defer,
            _        => Token::Ident(word), // not a keyword — it's a user-defined name
        }
    }
}