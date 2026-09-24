use anyhow::{Result, bail, ensure};

#[derive(Clone, Debug)]
pub enum Expr {
    Int(i64),
    Str(String),
    Bool(bool),
    Nil,
    Var(String),
    Call(String, Vec<Expr>),
    Unary(String, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
}
impl Expr {
    fn depth(&self) -> usize {
        1 + match self {
            Self::Unary(_, value) => value.depth(),
            Self::Binary(_, left, right) => left.depth().max(right.depth()),
            Self::Call(_, args) => args.iter().map(Self::depth).max().unwrap_or(0),
            _ => 0,
        }
    }
}
#[derive(Clone, Debug)]
pub enum Stmt {
    Local(String, Expr),
    Assign(String, Expr),
    Call(Expr),
    Return(Option<Expr>),
    If(Vec<(Expr, Vec<Stmt>)>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
}
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}
#[derive(Clone, Debug)]
struct Token {
    value: String,
    string: bool,
    line: usize,
    col: usize,
}

pub fn parse(source: &str) -> Result<Vec<Function>> {
    ensure!(source.len() <= 256 * 1024, "RGL source exceeds 256 KiB");
    let tokens = lex(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    let mut functions = vec![];
    while !parser.at("<eof>") {
        parser.expect("function")?;
        let name = parser.ident()?;
        ensure!(
            !functions.iter().any(|f: &Function| f.name == name),
            "duplicate function {name}"
        );
        parser.expect("(")?;
        let mut params = vec![];
        if !parser.at(")") {
            loop {
                let p = parser.ident()?;
                ensure!(!params.contains(&p), "duplicate parameter {p}");
                params.push(p);
                if !parser.eat(",") {
                    break;
                }
            }
        }
        parser.expect(")")?;
        let body = parser.block()?;
        parser.expect("end")?;
        functions.push(Function { name, params, body });
    }
    ensure!(
        functions.iter().any(|f| f.name == "on_request"),
        "on_request() is required"
    );
    Ok(functions)
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let mut it = source.chars().peekable();
    let (mut line, mut col) = (1, 1);
    let mut out = vec![];
    while let Some(c) = it.next() {
        let (start_line, start_col) = (line, col);
        col += 1;
        if c == '\n' {
            line += 1;
            col = 1;
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && it.peek() == Some(&'-') {
            it.next();
            col += 1;
            while it.peek().is_some_and(|c| *c != '\n') {
                it.next();
                col += 1;
            }
            continue;
        }
        let string = c == '\'' || c == '"';
        let mut value = String::new();
        if string {
            let mut closed = false;
            while let Some(next) = it.next() {
                col += 1;
                if next == c {
                    closed = true;
                    break;
                }
                if next == '\n' {
                    bail!("{start_line}:{start_col}: string must not span lines");
                }
                if next == '\\' {
                    let escaped = it
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("{line}:{col}: incomplete escape"))?;
                    col += 1;
                    value.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        _ => bail!("{line}:{col}: unsupported escape"),
                    });
                } else {
                    value.push(next);
                }
            }
            ensure!(closed, "{start_line}:{start_col}: unterminated string");
        } else {
            value.push(c);
            if c.is_ascii_alphanumeric() || c == '_' {
                while it
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    value.push(it.next().unwrap());
                    col += 1;
                }
            } else if let Some(next) = it.peek() {
                let pair = format!("{c}{next}");
                if ["==", "~=", "<=", ">=", ".."].contains(&pair.as_str()) {
                    value.push(it.next().unwrap());
                    col += 1;
                }
            }
            ensure!(
                c.is_ascii_alphanumeric() || c == '_' || "()+-*/%=~<>.,;".contains(c),
                "{line}:{col}: unexpected character {c}"
            );
        }
        out.push(Token {
            value,
            string,
            line: start_line,
            col: start_col,
        });
        ensure!(out.len() <= 65536, "too many tokens");
    }
    out.push(Token {
        value: "<eof>".into(),
        string: false,
        line,
        col,
    });
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}
impl Parser {
    fn at(&self, value: &str) -> bool {
        !self.tokens[self.pos].string && self.tokens[self.pos].value == value
    }
    fn eat(&mut self, value: &str) -> bool {
        if self.at(value) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, value: &str) -> Result<()> {
        if !self.eat(value) {
            let t = &self.tokens[self.pos];
            bail!("{}:{}: expected {value}, got {}", t.line, t.col, t.value);
        }
        Ok(())
    }
    fn ident(&mut self) -> Result<String> {
        let t = &self.tokens[self.pos];
        ensure!(
            !t.string
                && t.value
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && ![
                    "if", "then", "else", "elseif", "end", "while", "do", "return", "function",
                    "local", "true", "false", "nil", "and", "or", "not"
                ]
                .contains(&t.value.as_str()),
            "{}:{}: expected identifier",
            t.line,
            t.col
        );
        let value = t.value.clone();
        self.pos += 1;
        Ok(value)
    }
    fn block(&mut self) -> Result<Vec<Stmt>> {
        self.depth += 1;
        ensure!(self.depth <= 64, "block nesting exceeds 64");
        let mut statements = vec![];
        while !["end", "else", "elseif", "<eof>"]
            .iter()
            .any(|x| self.at(x))
        {
            statements.push(self.statement()?);
            self.eat(";");
        }
        self.depth -= 1;
        Ok(statements)
    }
    fn statement(&mut self) -> Result<Stmt> {
        if self.eat("local") {
            let name = self.ident()?;
            self.expect("=")?;
            return Ok(Stmt::Local(name, self.expr(0)?));
        }
        if self.eat("return") {
            let empty = ["end", "else", "elseif", ";", "<eof>"]
                .iter()
                .any(|v| self.at(v));
            return Ok(Stmt::Return(if empty { None } else { Some(self.expr(0)?) }));
        }
        if self.eat("if") {
            let mut branches = vec![];
            loop {
                let expr = self.expr(0)?;
                self.expect("then")?;
                let body = self.block()?;
                branches.push((expr, body));
                ensure!(branches.len() <= 64, "if has more than 64 branches");
                if !self.eat("elseif") {
                    break;
                }
            }
            let other = if self.eat("else") {
                self.block()?
            } else {
                vec![]
            };
            self.expect("end")?;
            return Ok(Stmt::If(branches, other));
        }
        if self.eat("while") {
            let e = self.expr(0)?;
            self.expect("do")?;
            let b = self.block()?;
            self.expect("end")?;
            return Ok(Stmt::While(e, b));
        }
        let e = self.expr(0)?;
        if self.eat("=") {
            if let Expr::Var(name) = e {
                return Ok(Stmt::Assign(name, self.expr(0)?));
            }
            bail!("assignment target must be a local variable");
        }
        ensure!(
            matches!(e, Expr::Call(..)),
            "only calls may be used as expression statements"
        );
        Ok(Stmt::Call(e))
    }
    fn expr(&mut self, min: u8) -> Result<Expr> {
        self.depth += 1;
        ensure!(self.depth <= 128, "expression nesting exceeds 128");
        let mut left = if self.eat("not") {
            Expr::Unary("not".into(), Box::new(self.expr(7)?))
        } else if self.eat("-") {
            Expr::Unary("-".into(), Box::new(self.expr(7)?))
        } else if self.eat("(") {
            let e = self.expr(0)?;
            self.expect(")")?;
            e
        } else {
            let t = self.tokens[self.pos].clone();
            if t.string {
                self.pos += 1;
                Expr::Str(t.value)
            } else if let Ok(n) = t.value.parse::<i64>() {
                self.pos += 1;
                Expr::Int(n)
            } else if self.eat("true") {
                Expr::Bool(true)
            } else if self.eat("false") {
                Expr::Bool(false)
            } else if self.eat("nil") {
                Expr::Nil
            } else {
                let mut name = self.ident()?;
                if self.eat(".") {
                    name.push('.');
                    name.push_str(&self.ident()?);
                }
                if self.eat("(") {
                    let mut args = vec![];
                    if !self.at(")") {
                        loop {
                            args.push(self.expr(0)?);
                            if !self.eat(",") {
                                break;
                            }
                        }
                    }
                    self.expect(")")?;
                    Expr::Call(name, args)
                } else {
                    Expr::Var(name)
                }
            }
        };
        self.check_expr_depth(&left)?;
        loop {
            let op = self.tokens[self.pos].value.clone();
            let precedence = match op.as_str() {
                "or" => 1,
                "and" => 2,
                "==" | "~=" | "<" | ">" | "<=" | ">=" => 3,
                ".." => 4,
                "+" | "-" => 5,
                "*" | "/" | "%" => 6,
                _ => 0,
            };
            if self.tokens[self.pos].string || precedence == 0 || precedence < min {
                break;
            }
            self.pos += 1;
            let right = self.expr(precedence + 1)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
            // Flat operator chains grow the AST without increasing Pratt parser recursion.
            self.check_expr_depth(&left)?;
        }
        self.depth -= 1;
        Ok(left)
    }
    fn check_expr_depth(&self, expr: &Expr) -> Result<()> {
        let token = &self.tokens[self.pos];
        ensure!(
            expr.depth() <= 128,
            "{}:{}: expression tree depth exceeds 128",
            token.line,
            token.col
        );
        Ok(())
    }
}
