use bigdecimal::BigDecimal;
use std::str::FromStr;

pub const MAX_LEN: usize = 2000;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Dec(BigDecimal),
    Str(String),
    List(Vec<Expr>),
    /// `name` or `name.field.field`
    Path(Vec<String>),
    Call(String, Vec<Expr>),
    /// `call(...).field`
    Field(Box<Expr>, String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    TooLong,
    TooDeep,
    TooManyNodes,
    BadToken(usize),
    BadNumber(String),
    Unexpected(String),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(String),
    Str(String),
    Ident(String),
    Op(&'static str),
}

const OPS: [&str; 15] = [
    "==", "!=", "<=", ">=", "<", ">", "+", "-", "*", "/", "%", "(", ")", ",", ".",
];

fn lex(src: &str) -> Result<Vec<Tok>, ParseError> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < b.len() && b[i] == '.' && b[i + 1].is_ascii_digit() {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            out.push(Tok::Num(b[s..i].iter().collect()));
        } else if c == '\'' {
            i += 1;
            let mut s = String::new();
            loop {
                match b.get(i) {
                    None => return Err(ParseError::BadToken(i)),
                    Some('\'') if b.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(*ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c.is_ascii_lowercase() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_lowercase() || b[i].is_ascii_digit() || b[i] == '_')
            {
                i += 1;
            }
            out.push(Tok::Ident(b[s..i].iter().collect()));
        } else if c == '[' || c == ']' {
            out.push(Tok::Op(if c == '[' { "[" } else { "]" }));
            i += 1;
        } else {
            let rest: String = b[i..b.len().min(i + 2)].iter().collect();
            match OPS.iter().find(|o| rest.starts_with(**o)) {
                Some(o) => {
                    out.push(Tok::Op(o));
                    i += o.len();
                }
                None => return Err(ParseError::BadToken(i)),
            }
        }
    }
    Ok(out)
}

struct Parser {
    t: Vec<Tok>,
    i: usize,
    depth: usize,
    nodes: usize,
}

pub fn parse(src: &str) -> Result<Expr, ParseError> {
    if src.chars().count() > MAX_LEN {
        return Err(ParseError::TooLong);
    }
    let mut p = Parser {
        t: lex(src)?,
        i: 0,
        depth: 0,
        nodes: 0,
    };
    let e = p.or()?;
    if p.i != p.t.len() {
        return Err(ParseError::Unexpected(format!("{:?}", p.t[p.i])));
    }
    Ok(e)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn is_op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if *x == o)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == k)
    }
    fn node(&mut self) -> Result<(), ParseError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            Err(ParseError::TooManyNodes)
        } else {
            Ok(())
        }
    }
    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(ParseError::TooDeep)
        } else {
            Ok(())
        }
    }
    fn bin(&mut self, op: Op, a: Expr, b: Expr) -> Result<Expr, ParseError> {
        self.node()?;
        Ok(Expr::Bin(op, Box::new(a), Box::new(b)))
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        self.enter()?;
        let mut a = self.and()?;
        while self.is_kw("or") {
            self.i += 1;
            let b = self.and()?;
            a = self.bin(Op::Or, a, b)?;
        }
        self.depth -= 1;
        Ok(a)
    }
    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.not()?;
        while self.is_kw("and") {
            self.i += 1;
            let b = self.not()?;
            a = self.bin(Op::And, a, b)?;
        }
        Ok(a)
    }
    fn not(&mut self) -> Result<Expr, ParseError> {
        if self.is_kw("not") {
            self.i += 1;
            self.node()?;
            let e = self.cmp()?;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, ParseError> {
        let a = self.add()?;
        let op = match self.peek() {
            Some(Tok::Op("==")) => Some(Op::Eq),
            Some(Tok::Op("!=")) => Some(Op::Ne),
            Some(Tok::Op("<")) => Some(Op::Lt),
            Some(Tok::Op("<=")) => Some(Op::Le),
            Some(Tok::Op(">")) => Some(Op::Gt),
            Some(Tok::Op(">=")) => Some(Op::Ge),
            Some(Tok::Ident(k)) if k == "in" => Some(Op::In),
            _ => None,
        };
        match op {
            Some(op) => {
                self.i += 1;
                let b = self.add()?;
                self.bin(op, a, b)
            }
            None => Ok(a),
        }
    }
    fn add(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.mul()?;
        loop {
            let op = if self.is_op("+") {
                Op::Add
            } else if self.is_op("-") {
                Op::Sub
            } else {
                break;
            };
            self.i += 1;
            let b = self.mul()?;
            a = self.bin(op, a, b)?;
        }
        Ok(a)
    }
    fn mul(&mut self) -> Result<Expr, ParseError> {
        let mut a = self.unary()?;
        loop {
            let op = if self.is_op("*") {
                Op::Mul
            } else if self.is_op("/") {
                Op::Div
            } else if self.is_op("%") {
                Op::Rem
            } else {
                break;
            };
            self.i += 1;
            let b = self.unary()?;
            a = self.bin(op, a, b)?;
        }
        Ok(a)
    }
    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.is_op("-") {
            self.i += 1;
            self.node()?;
            self.enter()?;
            let e = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Neg(Box::new(e)));
        }
        self.primary()
    }
    fn args(&mut self, close: &str) -> Result<Vec<Expr>, ParseError> {
        let mut v = Vec::new();
        while !self.is_op(close) {
            v.push(self.or()?);
            if self.is_op(",") {
                self.i += 1;
            } else if !self.is_op(close) {
                return Err(ParseError::Unexpected("expected , or close".into()));
            }
        }
        self.i += 1;
        Ok(v)
    }
    fn primary(&mut self) -> Result<Expr, ParseError> {
        self.node()?;
        let t = self.t.get(self.i).cloned().ok_or(ParseError::Eof)?;
        self.i += 1;
        match t {
            Tok::Num(n) => {
                if n.contains('.') {
                    BigDecimal::from_str(&n)
                        .map(Expr::Dec)
                        .map_err(|_| ParseError::BadNumber(n))
                } else {
                    n.parse::<i64>()
                        .map(Expr::Int)
                        .map_err(|_| ParseError::BadNumber(n))
                }
            }
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Op("(") => {
                self.enter()?;
                let e = self.or()?;
                self.depth -= 1;
                if !self.is_op(")") {
                    return Err(ParseError::Unexpected("expected )".into()));
                }
                self.i += 1;
                Ok(e)
            }
            Tok::Op("[") => {
                self.enter()?;
                let items = self.args("]")?;
                self.depth -= 1;
                Ok(Expr::List(items))
            }
            Tok::Ident(id) => match id.as_str() {
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                "and" | "or" | "not" | "in" => Err(ParseError::Unexpected(id)),
                _ => {
                    if self.is_op("(") {
                        self.i += 1;
                        self.enter()?;
                        let a = self.args(")")?;
                        self.depth -= 1;
                        let mut e = Expr::Call(id, a);
                        while self.is_op(".") {
                            self.i += 1;
                            match self.t.get(self.i) {
                                Some(Tok::Ident(f)) => {
                                    e = Expr::Field(Box::new(e), f.clone());
                                    self.i += 1;
                                }
                                _ => return Err(ParseError::Unexpected("expected field".into())),
                            }
                        }
                        Ok(e)
                    } else {
                        let mut path = vec![id];
                        while self.is_op(".") {
                            self.i += 1;
                            match self.t.get(self.i) {
                                Some(Tok::Ident(f)) => {
                                    path.push(f.clone());
                                    self.i += 1;
                                }
                                _ => return Err(ParseError::Unexpected("expected field".into())),
                            }
                        }
                        Ok(Expr::Path(path))
                    }
                }
            },
            Tok::Op(o) => Err(ParseError::Unexpected(o.into())),
        }
    }
}
