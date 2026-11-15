//! The GitHub Actions expression language (`${{ ... }}`): literals, context access, operators,
//! object filters and the built-in functions, following GitHub's coercion rules.
//!
//! Differences from GitHub:
//! - GitHub treats two arrays/objects as equal only when they are the same instance. Here they
//!   compare structurally, so two equal-looking arrays are `==`.
//! - Object filters (`obj.*`) yield values in key order, because serde_json maps are sorted;
//!   GitHub keeps the object's original order.

use anyhow::{Context as _, Result, anyhow, bail};
use std::path::Path;

pub use serde_json::Value;

/// Job/step status used by success()/failure()/always()/cancelled().
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusCtx {
    pub failed: bool,
    pub cancelled: bool,
}

/// Computes hashFiles(patterns) somewhere other than a local directory.
pub type Hasher = dyn Fn(&[String]) -> Result<String> + Send + Sync;

pub struct Ctx<'a> {
    /// Top-level contexts by name: github, env, vars, job, jobs, steps, runner, secrets, strategy, matrix, needs, inputs.
    /// Missing names evaluate to null.
    pub contexts: &'a serde_json::Map<String, Value>,
    pub status: StatusCtx,
    /// Workspace root for hashFiles(); None makes hashFiles return "".
    pub workspace: Option<&'a Path>,
    /// Computes hashFiles() when the workspace isn't on this machine (e.g. in a Docker volume).
    pub hasher: Option<&'a Hasher>,
}

// ---------- lexer ----------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Lit(Value),
    Ident(String),
    Dot,
    Star,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Not,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

fn num_value(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let c: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        let next = c.get(i + 1).copied();
        let tok = match ch {
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match c.get(i) {
                        None => bail!("unterminated string literal"),
                        Some('\'') if c.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&x) => {
                            s.push(x);
                            i += 1;
                        }
                    }
                }
                toks.push(Tok::Lit(Value::String(s)));
                continue;
            }
            _ if ch.is_ascii_digit()
                || (ch == '-' && next.is_some_and(|n| n.is_ascii_digit() || n == '.')) =>
            {
                let start = i;
                if ch == '-' {
                    i += 1;
                }
                let neg = ch == '-';
                if c.get(i) == Some(&'0') && matches!(c.get(i + 1), Some('x' | 'X')) {
                    i += 2;
                    let hs = i;
                    while c.get(i).is_some_and(|x| x.is_ascii_hexdigit()) {
                        i += 1;
                    }
                    let hex: String = c[hs..i].iter().collect();
                    let v =
                        i64::from_str_radix(&hex, 16).map_err(|_| anyhow!("invalid hex number"))?;
                    toks.push(Tok::Lit(Value::from(if neg { -v } else { v })));
                    continue;
                }
                while c.get(i).is_some_and(|x| x.is_ascii_digit()) {
                    i += 1;
                }
                if c.get(i) == Some(&'.') && c.get(i + 1).is_some_and(|x| x.is_ascii_digit()) {
                    i += 1;
                    while c.get(i).is_some_and(|x| x.is_ascii_digit()) {
                        i += 1;
                    }
                }
                if matches!(c.get(i), Some('e' | 'E')) {
                    let mut j = i + 1;
                    if matches!(c.get(j), Some('+' | '-')) {
                        j += 1;
                    }
                    if c.get(j).is_some_and(|x| x.is_ascii_digit()) {
                        i = j;
                        while c.get(i).is_some_and(|x| x.is_ascii_digit()) {
                            i += 1;
                        }
                    }
                }
                let text: String = c[start..i].iter().collect();
                let f: f64 = text
                    .parse()
                    .map_err(|_| anyhow!("invalid number {text:?}"))?;
                toks.push(Tok::Lit(num_value(f)));
                continue;
            }
            _ if ch.is_ascii_alphabetic() || ch == '_' => {
                let start = i;
                while c
                    .get(i)
                    .is_some_and(|x| x.is_ascii_alphanumeric() || *x == '_' || *x == '-')
                {
                    i += 1;
                }
                let word: String = c[start..i].iter().collect();
                toks.push(match word.as_str() {
                    "null" => Tok::Lit(Value::Null),
                    "true" => Tok::Lit(Value::Bool(true)),
                    "false" => Tok::Lit(Value::Bool(false)),
                    _ => Tok::Ident(word),
                });
                continue;
            }
            '.' => Tok::Dot,
            '*' => Tok::Star,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            '!' if next == Some('=') => {
                i += 1;
                Tok::Ne
            }
            '!' => Tok::Not,
            '=' if next == Some('=') => {
                i += 1;
                Tok::Eq
            }
            '<' if next == Some('=') => {
                i += 1;
                Tok::Le
            }
            '<' => Tok::Lt,
            '>' if next == Some('=') => {
                i += 1;
                Tok::Ge
            }
            '>' => Tok::Gt,
            '&' if next == Some('&') => {
                i += 1;
                Tok::And
            }
            '|' if next == Some('|') => {
                i += 1;
                Tok::Or
            }
            other => bail!("unexpected character {other:?}"),
        };
        toks.push(tok);
        i += 1;
    }
    Ok(toks)
}

// ---------- parser ----------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

#[derive(Debug)]
enum Expr {
    Lit(Value),
    Context(String),
    Prop(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Filter(Box<Expr>),
    Not(Box<Expr>),
    Cmp(Op, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            bail!("expected {t:?}, found {:?}", self.peek())
        }
    }

    fn or(&mut self) -> Result<Expr> {
        let mut l = self.and()?;
        while self.eat(&Tok::Or) {
            l = Expr::Or(Box::new(l), Box::new(self.and()?));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut l = self.eq()?;
        while self.eat(&Tok::And) {
            l = Expr::And(Box::new(l), Box::new(self.eq()?));
        }
        Ok(l)
    }

    fn eq(&mut self) -> Result<Expr> {
        let mut l = self.cmp()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Eq) => Op::Eq,
                Some(Tok::Ne) => Op::Ne,
                _ => return Ok(l),
            };
            self.pos += 1;
            l = Expr::Cmp(op, Box::new(l), Box::new(self.cmp()?));
        }
    }

    fn cmp(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => Op::Lt,
                Some(Tok::Le) => Op::Le,
                Some(Tok::Gt) => Op::Gt,
                Some(Tok::Ge) => Op::Ge,
                _ => return Ok(l),
            };
            self.pos += 1;
            l = Expr::Cmp(op, Box::new(l), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Not) {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.eat(&Tok::Dot) {
                e = match self.peek().cloned() {
                    Some(Tok::Star) => {
                        self.pos += 1;
                        Expr::Filter(Box::new(e))
                    }
                    Some(Tok::Ident(name)) => {
                        self.pos += 1;
                        Expr::Prop(Box::new(e), name)
                    }
                    // `a.true`, `a.null`: keywords are valid property names.
                    Some(Tok::Lit(v @ (Value::Null | Value::Bool(_)))) => {
                        self.pos += 1;
                        Expr::Prop(
                            Box::new(e),
                            if v.is_null() {
                                "null".into()
                            } else {
                                v.to_string()
                            },
                        )
                    }
                    other => bail!("expected a property name after '.', found {other:?}"),
                };
            } else if self.eat(&Tok::LBracket) {
                if self.eat(&Tok::Star) {
                    self.expect(&Tok::RBracket)?;
                    e = Expr::Filter(Box::new(e));
                } else {
                    let idx = self.or()?;
                    self.expect(&Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().cloned() {
            Some(Tok::Lit(v)) => {
                self.pos += 1;
                Ok(Expr::Lit(v))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.eat(&Tok::LParen) {
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.or()?);
                            if self.eat(&Tok::RParen) {
                                break;
                            }
                            self.expect(&Tok::Comma)?;
                        }
                    }
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Context(name))
                }
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let e = self.or()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            other => bail!("unexpected token {other:?}"),
        }
    }
}

fn parse(src: &str) -> Result<Expr> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    if p.toks.is_empty() {
        bail!("empty expression");
    }
    let e = p.or()?;
    if p.pos != p.toks.len() {
        bail!("unexpected {:?} after expression", p.toks[p.pos]);
    }
    Ok(e)
}

// ---------- coercion ----------

/// GitHub string coercion: null -> "", bool -> "true"/"false", numbers like JS (1 not 1.0), strings as-is, arrays/objects -> JSON (pretty, like toJSON).
pub fn to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                n.to_string()
            } else {
                let f = n.as_f64().unwrap_or(f64::NAN);
                if f.fract() == 0.0 && f.abs() < 1e21 {
                    format!("{f:.0}")
                } else {
                    f.to_string()
                }
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// GitHub truthiness: false, 0, -0, "", null (and NaN) are falsy; everything else (including empty arrays/objects) truthy.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn str_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let f = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_or(f64::NAN, |v| v as f64)
    } else if body == "Infinity" {
        f64::INFINITY
    } else if body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        body.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    };
    if neg { -f } else { f }
}

fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => str_to_number(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn same_kind(a: &Value, b: &Value) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    if same_kind(a, b) {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Number(_), Value::Number(_)) => to_number(a) == to_number(b),
            (Value::String(x), Value::String(y)) => x.to_lowercase() == y.to_lowercase(),
            // Structural, where GitHub uses instance identity (see module docs).
            _ => a == b,
        }
    } else {
        to_number(a) == to_number(b)
    }
}

fn compare(op: Op, a: &Value, b: &Value) -> bool {
    match op {
        Op::Eq => return loose_eq(a, b),
        Op::Ne => return !loose_eq(a, b),
        _ => {}
    }
    let ord = match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        _ if same_kind(a, b) && (a.is_array() || a.is_object()) => None,
        _ => to_number(a).partial_cmp(&to_number(b)),
    };
    let Some(ord) = ord else { return false };
    match op {
        Op::Lt => ord.is_lt(),
        Op::Le => ord.is_le(),
        Op::Gt => ord.is_gt(),
        Op::Ge => ord.is_ge(),
        Op::Eq | Op::Ne => unreachable!(),
    }
}

// ---------- evaluation ----------

/// An evaluated operand: a plain value, or the result of an object filter (`.*`), whose further
/// property/index accesses map over its elements.
enum Res {
    V(Value),
    F(Vec<Value>),
}

impl Res {
    fn value(self) -> Value {
        match self {
            Res::V(v) => v,
            Res::F(items) => Value::Array(items),
        }
    }
}

fn get_prop(v: &Value, name: &str) -> Value {
    match v {
        Value::Object(map) => map
            .get(name)
            .or_else(|| {
                map.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
            .cloned()
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn get_index(v: &Value, idx: &Value) -> Value {
    match v {
        Value::Array(items) => {
            let f = to_number(idx);
            if f.fract() == 0.0 && f >= 0.0 && (f as usize) < items.len() {
                items[f as usize].clone()
            } else {
                Value::Null
            }
        }
        Value::Object(_) => get_prop(v, &to_string(idx)),
        _ => Value::Null,
    }
}

fn elements(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        Value::Object(map) => map.into_iter().map(|(_, v)| v).collect(),
        _ => Vec::new(),
    }
}

fn map_access(base: Res, f: impl Fn(&Value) -> Value) -> Res {
    match base {
        Res::V(v) => Res::V(f(&v)),
        Res::F(items) => Res::F(items.iter().map(f).filter(|v| !v.is_null()).collect()),
    }
}

const STATUS_FNS: [&str; 4] = ["success", "failure", "always", "cancelled"];

fn uses_status(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) | Expr::Context(_) => false,
        Expr::Prop(a, _) | Expr::Filter(a) | Expr::Not(a) => uses_status(a),
        Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            uses_status(a) || uses_status(b)
        }
        Expr::Call(name, args) => {
            STATUS_FNS.iter().any(|f| f.eq_ignore_ascii_case(name)) || args.iter().any(uses_status)
        }
    }
}

fn eval_expr(e: &Expr, ctx: &Ctx) -> Result<Res> {
    Ok(match e {
        Expr::Lit(v) => Res::V(v.clone()),
        Expr::Context(name) => Res::V(
            ctx.contexts
                .get(name)
                .or_else(|| {
                    ctx.contexts
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v)
                })
                .cloned()
                .unwrap_or(Value::Null),
        ),
        Expr::Prop(base, name) => map_access(eval_expr(base, ctx)?, |v| get_prop(v, name)),
        Expr::Index(base, idx) => {
            let idx = eval_expr(idx, ctx)?.value();
            map_access(eval_expr(base, ctx)?, |v| get_index(v, &idx))
        }
        Expr::Filter(base) => match eval_expr(base, ctx)? {
            Res::V(v) => Res::F(elements(v)),
            Res::F(items) => Res::F(items.into_iter().flat_map(elements).collect()),
        },
        Expr::Not(a) => Res::V(Value::Bool(!truthy(&eval_expr(a, ctx)?.value()))),
        Expr::Cmp(op, a, b) => {
            let (a, b) = (eval_expr(a, ctx)?.value(), eval_expr(b, ctx)?.value());
            Res::V(Value::Bool(compare(*op, &a, &b)))
        }
        Expr::And(a, b) => {
            let l = eval_expr(a, ctx)?.value();
            if truthy(&l) {
                eval_expr(b, ctx)?
            } else {
                Res::V(l)
            }
        }
        Expr::Or(a, b) => {
            let l = eval_expr(a, ctx)?.value();
            if truthy(&l) {
                Res::V(l)
            } else {
                eval_expr(b, ctx)?
            }
        }
        Expr::Call(name, args) => Res::V(call(name, args, ctx)?),
    })
}

fn call(name: &str, args: &[Expr], ctx: &Ctx) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    let arity = |min: usize, max: usize| -> Result<()> {
        if args.len() < min || args.len() > max {
            if min == max {
                bail!("{name}() takes {min} argument(s), got {}", args.len());
            }
            bail!(
                "{name}() takes {min} to {max} arguments, got {}",
                args.len()
            );
        }
        Ok(())
    };
    let vals = |ctx: &Ctx| -> Result<Vec<Value>> {
        args.iter()
            .map(|a| eval_expr(a, ctx).map(Res::value))
            .collect()
    };
    Ok(match lname.as_str() {
        "success" | "failure" | "always" | "cancelled" => {
            arity(0, 0)?;
            Value::Bool(match lname.as_str() {
                "success" => !ctx.status.failed && !ctx.status.cancelled,
                "failure" => ctx.status.failed,
                "cancelled" => ctx.status.cancelled,
                _ => true,
            })
        }
        "contains" => {
            arity(2, 2)?;
            let v = vals(ctx)?;
            Value::Bool(match &v[0] {
                Value::Array(items) => items.iter().any(|x| loose_eq(x, &v[1])),
                s => to_string(s)
                    .to_lowercase()
                    .contains(&to_string(&v[1]).to_lowercase()),
            })
        }
        "startswith" | "endswith" => {
            arity(2, 2)?;
            let v = vals(ctx)?;
            let (s, p) = (
                to_string(&v[0]).to_lowercase(),
                to_string(&v[1]).to_lowercase(),
            );
            Value::Bool(if lname == "startswith" {
                s.starts_with(&p)
            } else {
                s.ends_with(&p)
            })
        }
        "format" => {
            arity(1, usize::MAX)?;
            let v = vals(ctx)?;
            Value::String(format_str(&to_string(&v[0]), &v[1..])?)
        }
        "join" => {
            arity(1, 2)?;
            let v = vals(ctx)?;
            let sep = v.get(1).map_or_else(|| ",".to_string(), to_string);
            Value::String(match &v[0] {
                Value::Array(items) => items.iter().map(to_string).collect::<Vec<_>>().join(&sep),
                other => to_string(other),
            })
        }
        "tojson" => {
            arity(1, 1)?;
            Value::String(serde_json::to_string_pretty(&vals(ctx)?[0])?)
        }
        "fromjson" => {
            arity(1, 1)?;
            let s = to_string(&vals(ctx)?[0]);
            serde_json::from_str(s.trim())
                .with_context(|| format!("fromJSON: invalid JSON {s:?}"))?
        }
        "hashfiles" => {
            arity(1, usize::MAX)?;
            let patterns: Vec<String> = vals(ctx)?.iter().map(to_string).collect();
            match (ctx.hasher, ctx.workspace) {
                (Some(h), _) => Value::String(h(&patterns)?),
                (None, Some(root)) => Value::String(super::glob::hash_files(root, &patterns)?),
                (None, None) => Value::String(String::new()),
            }
        }
        _ => bail!("unknown function {name}()"),
    })
}

fn format_str(fmt: &str, args: &[Value]) -> Result<String> {
    let c: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '{' if c.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if c.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let close = c[i..].iter().position(|&x| x == '}').map(|p| p + i);
                let Some(close) = close else {
                    bail!("format: unclosed '{{' in {fmt:?}")
                };
                let digits: String = c[i + 1..close].iter().collect();
                let n: usize = digits
                    .parse()
                    .map_err(|_| anyhow!("format: invalid placeholder {{{digits}}} in {fmt:?}"))?;
                let arg = args
                    .get(n)
                    .ok_or_else(|| anyhow!("format: index {n} out of range in {fmt:?}"))?;
                out.push_str(&to_string(arg));
                i = close + 1;
            }
            '}' => bail!("format: unmatched '}}' in {fmt:?}"),
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Evaluate a bare expression (no `${{ }}`), e.g. `github.ref == 'refs/heads/main'`.
pub fn eval(expr: &str, ctx: &Ctx) -> Result<Value> {
    let run = || -> Result<Value> { Ok(eval_expr(&parse(expr)?, ctx)?.value()) };
    run().with_context(|| format!("in expression `{}`", expr.trim()))
}

/// Find the end of the expression starting at byte `start` (just after `${{`): the index of the
/// closing `}}`, skipping `}}` inside single-quoted strings.
fn find_close(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut in_str = false;
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'}' if !in_str && b.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split `s` into literal text and expressions.
fn segments(s: &str) -> Result<Vec<(bool, &str)>> {
    let mut out = Vec::new();
    let mut rest = 0;
    while let Some(off) = s[rest..].find("${{") {
        let open = rest + off;
        if open > rest {
            out.push((false, &s[rest..open]));
        }
        let close =
            find_close(s, open + 3).ok_or_else(|| anyhow!("unterminated `${{{{` in {s:?}"))?;
        out.push((true, &s[open + 3..close]));
        rest = close + 2;
    }
    if rest < s.len() {
        out.push((false, &s[rest..]));
    }
    Ok(out)
}

/// Evaluate an `if:` condition. Accepts either a bare expression or one wrapped in `${{ }}` (with surrounding whitespace).
/// Empty/missing condition means `success()`. If the expression does not call any of success()/failure()/always()/cancelled(),
/// it is implicitly `success() && (expr)`.
pub fn eval_if(cond: &str, ctx: &Ctx) -> Result<bool> {
    let t = cond.trim();
    let expr = if t.contains("${{") {
        match segments(t)?.as_slice() {
            [(true, inner)] => inner.trim().to_string(),
            // Text around the expression: GitHub evaluates the interpolated string's truthiness.
            _ => return Ok(truthy(&Value::String(interpolate_str(t, ctx)?))),
        }
    } else {
        t.to_string()
    };
    let default_success = !ctx.status.failed && !ctx.status.cancelled;
    if expr.is_empty() {
        return Ok(default_success);
    }
    let run = || -> Result<bool> {
        let ast = parse(&expr)?;
        if !uses_status(&ast) && !default_success {
            return Ok(false);
        }
        Ok(truthy(&eval_expr(&ast, ctx)?.value()))
    };
    run().with_context(|| format!("in condition `{expr}`"))
}

/// Replace every `${{ expr }}` in `s`. If `s` (trimmed) is exactly one `${{ expr }}`, return the expression's Value unchanged
/// (so `${{ fromJSON(x) }}` can yield an array/object); otherwise return Value::String with each expression coerced via to_string.
/// Strings without `${{` are returned as Value::String unchanged.
pub fn interpolate(s: &str, ctx: &Ctx) -> Result<Value> {
    if !s.contains("${{") {
        return Ok(Value::String(s.to_string()));
    }
    if let [(true, inner)] = segments(s.trim())?.as_slice() {
        return eval(inner, ctx);
    }
    let mut out = String::new();
    for (is_expr, part) in segments(s)? {
        if is_expr {
            out.push_str(&to_string(&eval(part, ctx)?));
        } else {
            out.push_str(part);
        }
    }
    Ok(Value::String(out))
}

/// Like interpolate but always a String (to_string coercion).
pub fn interpolate_str(s: &str, ctx: &Ctx) -> Result<String> {
    Ok(to_string(&interpolate(s, ctx)?))
}

/// Recursively interpolate every string inside arrays/objects (object keys too). Non-strings unchanged.
pub fn interpolate_value(v: &Value, ctx: &Ctx) -> Result<Value> {
    Ok(match v {
        Value::String(s) => interpolate(s, ctx)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|x| interpolate_value(x, ctx))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, x) in map {
                out.insert(interpolate_str(k, ctx)?, interpolate_value(x, ctx)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contexts() -> serde_json::Map<String, Value> {
        json!({
            "github": { "Ref": "refs/heads/main", "event": { "commits": [ {"message": "one"}, {"message": "two"} ] } },
            "matrix": { "os": "ubuntu", "versions": [18, 20] },
            "fruits": [ {"name": "apple"}, {"name": "orange"}, {"name": "pear"} ],
            "vegetables": {
                "scallions": { "ediblePortions": ["roots", "stems"] },
                "beets": { "ediblePortions": ["roots", "leaves"] },
                "artichokes": { "ediblePortions": ["hearts"] }
            },
            "steps": { "build-it": { "outputs": { "path": "out/bin" } } }
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn ev(expr: &str) -> Value {
        let c = contexts();
        eval(
            expr,
            &Ctx {
                contexts: &c,
                status: StatusCtx::default(),
                workspace: None,
                hasher: None,
            },
        )
        .unwrap()
    }

    fn ev_err(expr: &str) -> String {
        let c = contexts();
        format!(
            "{:#}",
            eval(
                expr,
                &Ctx {
                    contexts: &c,
                    status: StatusCtx::default(),
                    workspace: None,
                    hasher: None,
                }
            )
            .unwrap_err()
        )
    }

    fn cond(expr: &str, failed: bool, cancelled: bool) -> bool {
        let c = contexts();
        eval_if(
            expr,
            &Ctx {
                contexts: &c,
                status: StatusCtx { failed, cancelled },
                workspace: None,
                hasher: None,
            },
        )
        .unwrap()
    }

    fn interp(s: &str) -> Value {
        let c = contexts();
        interpolate(
            s,
            &Ctx {
                contexts: &c,
                status: StatusCtx::default(),
                workspace: None,
                hasher: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn literals() {
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("true"), json!(true));
        assert_eq!(ev("42"), json!(42));
        assert_eq!(ev("-1.5"), json!(-1.5));
        assert_eq!(ev("0xff"), json!(255));
        assert_eq!(ev("1e3"), json!(1000));
        assert_eq!(ev("2.5e-1"), json!(0.25));
        assert_eq!(ev("'it''s'"), json!("it's"));
    }

    #[test]
    fn operators_and_precedence() {
        assert_eq!(ev("true || false && false"), json!(true));
        assert_eq!(ev("!true == false"), json!(true));
        assert_eq!(ev("!(1 == 1)"), json!(false));
        assert_eq!(ev("1 < 2 == true"), json!(true));
        // && and || return operands.
        assert_eq!(ev("'a' && 'b'"), json!("b"));
        assert_eq!(ev("'' || 'x'"), json!("x"));
        assert_eq!(ev("null && missing.thing"), Value::Null);
        assert_eq!(
            ev("github.ref == 'refs/heads/main' && 'prod' || 'dev'"),
            json!("prod")
        );
        assert_eq!(
            ev("matrix.os == 'windows' && 'win' || 'other'"),
            json!("other")
        );
    }

    #[test]
    fn coercion_table() {
        assert_eq!(ev("'' == null"), json!(true));
        assert_eq!(ev("'0' == 0"), json!(true));
        assert_eq!(ev("' 12 ' == 12"), json!(true));
        assert_eq!(ev("'0x10' == 16"), json!(true));
        assert_eq!(ev("true == 1"), json!(true));
        assert_eq!(ev("null == false"), json!(true));
        assert_eq!(ev("null == 0"), json!(true));
        assert_eq!(ev("'abc' == 'ABC'"), json!(true));
        assert_eq!(ev("'a' == 0"), json!(false));
        assert_eq!(ev("'a' != 0"), json!(true));
        assert_eq!(ev("'a' < 1"), json!(false));
        assert_eq!(ev("'a' >= 1"), json!(false));
        assert_eq!(ev("1 < 2"), json!(true));
        assert_eq!(ev("2 <= 2"), json!(true));
        assert_eq!(ev("'a' < 'B'"), json!(true));
        assert_eq!(ev("'10' > 9"), json!(true));
        assert_eq!(ev("matrix.versions == 0"), json!(false));
        assert_eq!(ev("matrix.versions == fromJSON('[18,20]')"), json!(true));
    }

    #[test]
    fn truthiness_and_strings() {
        for (v, t) in [
            (json!(null), false),
            (json!(false), false),
            (json!(0), false),
            (json!(-0.0), false),
            (json!(""), false),
            (json!("0"), true),
            (json!([]), true),
            (json!({}), true),
            (json!(1.5), true),
        ] {
            assert_eq!(truthy(&v), t, "{v}");
        }
        assert_eq!(to_string(&json!(null)), "");
        assert_eq!(to_string(&json!(true)), "true");
        assert_eq!(to_string(&json!(1.0)), "1");
        assert_eq!(to_string(&json!(1.5)), "1.5");
        assert_eq!(to_string(&json!([1])), "[\n  1\n]");
    }

    #[test]
    fn property_access_is_case_insensitive() {
        assert_eq!(ev("github.ref"), json!("refs/heads/main"));
        assert_eq!(ev("GITHUB.REF"), json!("refs/heads/main"));
        assert_eq!(ev("github['ref']"), json!("refs/heads/main"));
        assert_eq!(ev("github.nope.deeper"), Value::Null);
        assert_eq!(ev("missing"), Value::Null);
        assert_eq!(ev("matrix.versions[1]"), json!(20));
        assert_eq!(ev("matrix.versions[5]"), Value::Null);
        assert_eq!(ev("steps.build-it.outputs.path"), json!("out/bin"));
    }

    #[test]
    fn object_filters() {
        assert_eq!(ev("fruits.*.name"), json!(["apple", "orange", "pear"]));
        assert_eq!(ev("fruits[*].name"), json!(["apple", "orange", "pear"]));
        assert_eq!(ev("github.event.commits.*.message"), json!(["one", "two"]));
        // Object values come out in insertion order, like GitHub (serde_json `preserve_order`).
        assert_eq!(
            ev("vegetables.*.ediblePortions"),
            json!([["roots", "stems"], ["roots", "leaves"], ["hearts"]])
        );
        assert_eq!(
            ev("vegetables.*.ediblePortions.*"),
            json!(["roots", "stems", "roots", "leaves", "hearts"])
        );
        assert_eq!(
            ev("contains(vegetables.*.ediblePortions.*, 'HEARTS')"),
            json!(true)
        );
        assert_eq!(ev("matrix.os.*"), json!([]));
    }

    #[test]
    fn functions() {
        assert_eq!(ev("contains('Hello world', 'WORLD')"), json!(true));
        assert_eq!(ev("contains(fromJSON('[\"a\",\"b\"]'), 'B')"), json!(true));
        assert_eq!(ev("contains(matrix.versions, '20')"), json!(true));
        assert_eq!(ev("startsWith(github.ref, 'REFS/heads/')"), json!(true));
        assert_eq!(ev("endsWith(github.ref, '/main')"), json!(true));
        assert_eq!(ev("format('{0} {{x}} {1}', 'a', 2)"), json!("a {x} 2"));
        assert_eq!(
            ev("format('Hello {0} {1} {0}', 'Mona', 'the')"),
            json!("Hello Mona the Mona")
        );
        assert_eq!(ev("join(matrix.versions)"), json!("18,20"));
        assert_eq!(ev("join(matrix.versions, ' - ')"), json!("18 - 20"));
        assert_eq!(ev("join('abc', '-')"), json!("abc"));
        assert_eq!(
            ev("toJSON(fromJSON('{\"a\":1}'))"),
            json!("{\n  \"a\": 1\n}")
        );
        assert_eq!(ev("fromJSON('true')"), json!(true));
        assert_eq!(ev("TOJSON(null)"), json!("null"));
        assert_eq!(ev("hashFiles('**/*.lock')"), json!(""));
        assert_eq!(ev("always()"), json!(true));
    }

    #[test]
    fn errors_name_the_expression() {
        assert!(ev_err("nope(1)").contains("unknown function nope()"));
        assert!(ev_err("nope(1)").contains("`nope(1)`"));
        assert!(ev_err("contains('a')").contains("takes 2 argument"));
        assert!(ev_err("format('{2}', 'a')").contains("out of range"));
        assert!(ev_err("format('{x}', 'a')").contains("invalid placeholder"));
        assert!(ev_err("format('}', 'a')").contains("unmatched"));
        assert!(ev_err("1 ==").contains("in expression"));
        assert!(ev_err("'open").contains("unterminated"));
        assert!(ev_err("a = b").contains("unexpected character"));
        assert!(ev_err("fromJSON('{')").contains("invalid JSON"));
    }

    #[test]
    fn conditions() {
        assert!(cond("", false, false));
        assert!(!cond("", true, false));
        assert!(cond("always()", true, false));
        assert!(cond("${{ failure() }}", true, false));
        assert!(!cond("failure()", false, false));
        assert!(cond("cancelled()", false, true));
        assert!(!cond("success()", false, true));
        // Implicit success() when no status function is used.
        assert!(cond("github.ref == 'refs/heads/main'", false, false));
        assert!(!cond("github.ref == 'refs/heads/main'", true, false));
        assert!(!cond("github.ref == 'refs/heads/dev'", false, false));
        assert!(cond(
            "  ${{ github.ref == 'refs/heads/main' }}  ",
            false,
            false
        ));
        assert!(cond("always() && github.ref != 'x'", true, false));
        // Text around an expression is a non-empty string, so truthy.
        assert!(cond("${{ false }} x", false, false));
    }

    #[test]
    fn interpolation() {
        assert_eq!(interp("${{ fromJSON('[1,2]') }}"), json!([1, 2]));
        assert_eq!(interp("  ${{ 1 }}  "), json!(1));
        assert_eq!(interp("v${{ 1 }}-${{ 'x' }}"), json!("v1-x"));
        assert_eq!(
            interp("os=${{ matrix.os }}, none=${{ missing }}."),
            json!("os=ubuntu, none=.")
        );
        assert_eq!(interp("no expressions here"), json!("no expressions here"));
        assert_eq!(interp("${{ format('{{x}}') }}"), json!("{x}"));
        assert_eq!(interp("a ${{ 'b}}c' }} d"), json!("a b}}c d"));
        let c = contexts();
        let ctx = Ctx {
            contexts: &c,
            status: StatusCtx::default(),
            workspace: None,
            hasher: None,
        };
        assert!(interpolate("${{ 1", &ctx).is_err());
        assert_eq!(
            interpolate_str("${{ matrix.versions[0] }}", &ctx).unwrap(),
            "18"
        );
        let v =
            json!({ "${{ matrix.os }}-key": ["${{ 1 }}", "x${{ 2 }}", 3, { "n": "${{ true }}" }] });
        assert_eq!(
            interpolate_value(&v, &ctx).unwrap(),
            json!({ "ubuntu-key": [1, "x2", 3, { "n": true }] })
        );
    }

    #[test]
    fn hash_files_uses_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), b"lock").unwrap();
        let c = contexts();
        let ctx = Ctx {
            contexts: &c,
            status: StatusCtx::default(),
            workspace: Some(dir.path()),
            hasher: None,
        };
        let h = to_string(&eval("hashFiles('**/Cargo.lock')", &ctx).unwrap());
        assert_eq!(h.len(), 64);
        assert_eq!(
            h,
            crate::gha::glob::hash_files(dir.path(), &["Cargo.lock".into()]).unwrap()
        );
        assert_eq!(eval("hashFiles('none/**')", &ctx).unwrap(), json!(""));
    }
}
