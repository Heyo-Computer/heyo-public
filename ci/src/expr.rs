//! `${{ … }}` expression evaluation.
//!
//! GitHub Actions' expression language, in the subset a workflow actually uses:
//! context lookups (`matrix.target`, `needs.build.outputs.sha`), string and
//! number literals, `== != > < >= <=`, `&& || !`, parentheses, and the handful
//! of functions worth having (`contains`, `startsWith`, `endsWith`, `format`,
//! `join`, `toJSON`, `fromJSON`, plus the `success()`/`failure()`/`always()`
//! status checks).
//!
//! **This is a real parser rather than a chain of `split_once`.** The version in
//! `heyo/cicd/src/runner.rs:6960` splits on the first `&&` and then the first
//! `||`, which gets precedence wrong and cannot represent parentheses or
//! negation at all: `a == 'x' || b == 'y' && c == 'z'` evaluates differently
//! there than in GitHub Actions, and silently — the condition just comes out the
//! wrong way and a job runs when it should not. Recursive descent is barely more
//! code and removes the whole class.
//!
//! ## One deliberate deviation from GitHub
//!
//! GitHub treats every non-empty string as truthy, so `${{ 'false' }}` is
//! **true** there. Here the strings `"false"` and `"0"` are falsey, following
//! the precedent already set in `heyo/cicd`. The reason is where these values
//! come from: a step output is produced by a shell, and `echo false` is how a
//! shell says no. Under GitHub's rule `if: steps.check.outputs.ok` would run the
//! step whatever `check` decided, which is the more damaging default.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// The named scopes an expression can read.
///
/// Backed by a JSON object so that `needs.build.outputs.sha` and
/// `steps.foo.outputs.bar` are the same walk, rather than each path prefix
/// needing its own match arm.
#[derive(Debug, Clone, Default)]
pub struct Context {
    root: BTreeMap<String, Value>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a whole scope, e.g. `set("matrix", json!({"target": "x86_64"}))`.
    pub fn set(&mut self, scope: &str, value: Value) -> &mut Self {
        self.root.insert(scope.to_string(), value);
        self
    }

    /// Set the job/step status the `success()` / `failure()` functions report.
    pub fn set_status(&mut self, status: &str) -> &mut Self {
        self.set("__status", Value::String(status.to_string()))
    }

    fn status(&self) -> &str {
        self.root
            .get("__status")
            .and_then(Value::as_str)
            .unwrap_or("success")
    }

    fn lookup(&self, path: &[String]) -> Value {
        let Some(first) = path.first() else {
            return Value::Null;
        };
        let root = match self.root.get(first) {
            Some(v) => v.clone(),
            None => return Value::Null,
        };
        walk(root, &path[1..])
    }

    /// Replace every `${{ … }}` in `input` with its value.
    ///
    /// A malformed or unevaluable expression is left **verbatim** rather than
    /// blanked. A silently empty string in a `run:` turns `rm -rf /tmp/${{ x }}`
    /// into `rm -rf /tmp/`; leaving the text in place makes the mistake visible
    /// in the log and usually fails the step.
    pub fn substitute(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("${{") {
            out.push_str(&rest[..start]);
            let body = &rest[start + 3..];
            let Some(end) = find_close(body) else {
                // No closing `}}` at all: the rest is literal text.
                out.push_str(&rest[start..]);
                return out;
            };
            let expr = &body[..end];
            match self.eval(expr) {
                Ok(v) => out.push_str(&to_display(&v)),
                Err(_) => out.push_str(&rest[start..start + 3 + end + 2]),
            }
            rest = &body[end + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Evaluate an `if:` condition.
    ///
    /// An empty condition is true — that is the absent case, and a job with no
    /// `if:` runs. A condition that does not parse is **false**, and the error is
    /// the caller's to report: running a step whose guard could not be
    /// understood is the unsafe direction.
    pub fn eval_condition(&self, expr: &str) -> Result<bool, ExprError> {
        let expr = expr.trim();
        if expr.is_empty() {
            return Ok(true);
        }
        // `if: ${{ … }}` and a bare `if: …` are both legal in GitHub Actions.
        let expr = match expr.strip_prefix("${{") {
            Some(rest) => rest.strip_suffix("}}").unwrap_or(rest),
            None => expr,
        };
        Ok(truthy(&self.eval(expr)?))
    }

    /// Evaluate one expression body (the text between `${{` and `}}`).
    pub fn eval(&self, expr: &str) -> Result<Value, ExprError> {
        let tokens = tokenize(expr)?;
        let mut p = Parser {
            tokens: &tokens,
            pos: 0,
            ctx: self,
        };
        let v = p.parse_or()?;
        if p.pos != p.tokens.len() {
            return Err(ExprError::Trailing(format!("{:?}", p.tokens[p.pos])));
        }
        Ok(v)
    }
}

/// Walk property segments into a value.
///
/// Shared by context lookups and by postfix access on a call result, so
/// `fromJSON(x).items.0` resolves the same way `needs.build.outputs.sha` does.
fn walk(mut cur: Value, segments: &[String]) -> Value {
    for seg in segments {
        cur = match &cur {
            Value::Object(map) => map.get(seg).cloned().unwrap_or(Value::Null),
            // Numeric segments index an array, so `list.0` works.
            Value::Array(items) => seg
                .parse::<usize>()
                .ok()
                .and_then(|i| items.get(i).cloned())
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
    }
    cur
}

/// Turn a parsed number into JSON, preferring an integer.
///
/// The tokenizer works in `f64`, but `serde_json` renders `42.0` as `"42.0"`,
/// so `--jobs ${{ matrix.n }}` would produce `--jobs 2.0` and fail.
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return Value::Number((n as i64).into());
    }
    serde_json::Number::from_f64(n)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// Find the `}}` that closes an expression, ignoring any inside a string
/// literal. `${{ format('{0}}}', x) }}` would otherwise terminate early.
fn find_close(body: &str) -> Option<usize> {
    let b = body.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        match quote {
            Some(q) => {
                // '' is an escaped quote inside a single-quoted string.
                if b[i] == q {
                    if q == b'\'' && i + 1 < b.len() && b[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            None => {
                if b[i] == b'\'' || b[i] == b'"' {
                    quote = Some(b[i]);
                } else if b[i] == b'}' && i + 1 < b.len() && b[i + 1] == b'}' {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// How a value renders when substituted into text.
fn to_display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // Matches GitHub: an object or array interpolated into a string becomes
        // its JSON form.
        other => other.to_string(),
    }
}

/// See the module doc for the one deviation from GitHub here.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty() && s != "false" && s != "0",
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Loose equality, following GitHub: values of different types are compared by
/// their string forms, so `matrix.n == 1` works whether the matrix axis parsed
/// as a number or a string. This matters because YAML types are not stable
/// across the axis/`include` spellings of a matrix.
fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => to_display(a) == to_display(b),
    }
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    Dot,
    LParen,
    RParen,
    Comma,
    EqEq,
    NotEq,
    Gt,
    Ge,
    Lt,
    Le,
    And,
    Or,
    Not,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, ExprError> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b'=' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::EqEq);
                i += 2;
            }
            b'!' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::NotEq);
                i += 2;
            }
            b'!' => {
                out.push(Tok::Not);
                i += 1;
            }
            b'>' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Ge);
                i += 2;
            }
            b'>' => {
                out.push(Tok::Gt);
                i += 1;
            }
            b'<' if b.get(i + 1) == Some(&b'=') => {
                out.push(Tok::Le);
                i += 2;
            }
            b'<' => {
                out.push(Tok::Lt);
                i += 1;
            }
            b'&' if b.get(i + 1) == Some(&b'&') => {
                out.push(Tok::And);
                i += 2;
            }
            b'|' if b.get(i + 1) == Some(&b'|') => {
                out.push(Tok::Or);
                i += 2;
            }
            b'\'' | b'"' => {
                let quote = c;
                let mut lit = String::new();
                i += 1;
                loop {
                    if i >= b.len() {
                        return Err(ExprError::UnterminatedString);
                    }
                    if b[i] == quote {
                        // `''` inside a single-quoted string is one quote.
                        if quote == b'\'' && b.get(i + 1) == Some(&b'\'') {
                            lit.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    lit.push(b[i] as char);
                    i += 1;
                }
                out.push(Tok::Str(lit));
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    // A `.` only continues a number if a digit follows, so
                    // `steps.1.outputs` still tokenizes as a path.
                    if b[i] == b'.' && !b.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                        break;
                    }
                    i += 1;
                }
                let text = &s[start..i];
                out.push(Tok::Num(
                    text.parse()
                        .map_err(|_| ExprError::BadNumber(text.into()))?,
                ));
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'*' => {
                let start = i;
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'_'
                        || b[i] == b'-'
                        || b[i] == b'*')
                {
                    i += 1;
                }
                out.push(Tok::Ident(s[start..i].to_string()));
            }
            other => return Err(ExprError::UnexpectedChar(other as char)),
        }
    }
    Ok(out)
}

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
    ctx: &'a Context,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn parse_or(&mut self) -> Result<Value, ExprError> {
        let mut left = self.parse_and()?;
        while self.eat(&Tok::Or) {
            let right = self.parse_and()?;
            // GitHub's `||` yields the first truthy *operand*, not a boolean,
            // which is what makes `${{ x || 'default' }}` work.
            left = if truthy(&left) { left } else { right };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Value, ExprError> {
        let mut left = self.parse_not()?;
        while self.eat(&Tok::And) {
            let right = self.parse_not()?;
            left = if truthy(&left) { right } else { left };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Value, ExprError> {
        if self.eat(&Tok::Not) {
            let v = self.parse_not()?;
            return Ok(Value::Bool(!truthy(&v)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Value, ExprError> {
        let left = self.parse_primary()?;
        let op = match self.peek() {
            Some(Tok::EqEq) => Tok::EqEq,
            Some(Tok::NotEq) => Tok::NotEq,
            Some(Tok::Gt) => Tok::Gt,
            Some(Tok::Ge) => Tok::Ge,
            Some(Tok::Lt) => Tok::Lt,
            Some(Tok::Le) => Tok::Le,
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.parse_primary()?;
        let result = match op {
            Tok::EqEq => loose_eq(&left, &right),
            Tok::NotEq => !loose_eq(&left, &right),
            _ => {
                // Ordering comparisons are numeric only. A non-numeric operand
                // is false rather than an error, matching GitHub, where
                // comparing to NaN is always false.
                match (as_number(&left), as_number(&right)) {
                    (Some(a), Some(b)) => match op {
                        Tok::Gt => a > b,
                        Tok::Ge => a >= b,
                        Tok::Lt => a < b,
                        Tok::Le => a <= b,
                        _ => unreachable!(),
                    },
                    _ => false,
                }
            }
        };
        Ok(Value::Bool(result))
    }

    /// A primary plus any trailing `.property` access.
    ///
    /// The postfix loop is what makes `fromJSON(steps.x.outputs.json).version`
    /// work — without it a call result is terminal and the `.version` is a parse
    /// error at the top level.
    fn parse_primary(&mut self) -> Result<Value, ExprError> {
        let base = self.parse_atom()?;
        let mut segments = Vec::new();
        while self.peek() == Some(&Tok::Dot) {
            self.pos += 1;
            match self.peek().cloned() {
                Some(Tok::Ident(seg)) => {
                    self.pos += 1;
                    segments.push(seg);
                }
                Some(Tok::Num(n)) => {
                    self.pos += 1;
                    segments.push(to_display(&number(n)));
                }
                _ => return Err(ExprError::TrailingDot(segments.join("."))),
            }
        }
        Ok(walk(base, &segments))
    }

    fn parse_atom(&mut self) -> Result<Value, ExprError> {
        match self.peek().cloned() {
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.parse_or()?;
                if !self.eat(&Tok::RParen) {
                    return Err(ExprError::UnclosedParen);
                }
                Ok(v)
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Value::String(s))
            }
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(number(n))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.eat(&Tok::RParen) {
                        loop {
                            args.push(self.parse_or()?);
                            if self.eat(&Tok::Comma) {
                                continue;
                            }
                            if self.eat(&Tok::RParen) {
                                break;
                            }
                            return Err(ExprError::UnclosedParen);
                        }
                    }
                    return self.call(&name, &args);
                }
                // Bare keywords, then a context path.
                match name.as_str() {
                    "true" => return Ok(Value::Bool(true)),
                    "false" => return Ok(Value::Bool(false)),
                    "null" => return Ok(Value::Null),
                    _ => {}
                }
                // Just the scope. `parse_primary`'s postfix loop walks the rest,
                // so a path and a call result take the same route.
                Ok(self.ctx.lookup(&[name]))
            }
            other => Err(ExprError::Unexpected(format!("{other:?}"))),
        }
    }

    fn call(&self, name: &str, args: &[Value]) -> Result<Value, ExprError> {
        let s = |i: usize| args.get(i).map(to_display).unwrap_or_default();
        match name {
            "contains" => Ok(Value::Bool(match args.first() {
                // `contains(array, item)` is a membership test; on a string it
                // is a substring test. GitHub overloads it the same way.
                Some(Value::Array(items)) => items
                    .iter()
                    .any(|i| loose_eq(i, args.get(1).unwrap_or(&Value::Null))),
                _ => s(0).contains(&s(1)),
            })),
            "startsWith" => Ok(Value::Bool(s(0).starts_with(&s(1)))),
            "endsWith" => Ok(Value::Bool(s(0).ends_with(&s(1)))),
            "format" => {
                let mut out = s(0);
                for (i, a) in args.iter().skip(1).enumerate() {
                    out = out.replace(&format!("{{{i}}}"), &to_display(a));
                }
                Ok(Value::String(out))
            }
            "join" => {
                let sep = if args.len() > 1 {
                    s(1)
                } else {
                    ",".to_string()
                };
                Ok(Value::String(match args.first() {
                    Some(Value::Array(items)) => {
                        items.iter().map(to_display).collect::<Vec<_>>().join(&sep)
                    }
                    _ => s(0),
                }))
            }
            "toJSON" => Ok(Value::String(
                args.first().unwrap_or(&Value::Null).to_string(),
            )),
            "fromJSON" => Ok(serde_json::from_str(&s(0)).unwrap_or(Value::Null)),
            // `changed('packages/api/**')` — the monorepo gate, at job level.
            //
            // Not expressible with `contains(ci.changed_files, …)`: `contains`
            // on an array is an equality test, so it would need the exact path
            // of every file somebody might touch. This runs the same glob
            // matcher the `on: submit: paths:` filter uses, so a job's `if:` and
            // a workflow's filter cannot disagree about what a pattern covers.
            //
            // **True when the change set is unknown**, matching the filters and
            // for the same reason: a tarball submit, a root commit and a
            // `--dirty` submit have no diff to read, and a job that silently
            // skips on those is a green tick on work nothing did.
            "changed" => {
                let known = truthy(&self.ctx.lookup(&["ci".into(), "changes_known".into()]));
                if !known {
                    return Ok(Value::Bool(true));
                }
                let files = self.ctx.lookup(&["ci".into(), "changed_files".into()]);
                let Value::Array(files) = files else {
                    return Ok(Value::Bool(true));
                };
                // Every argument is a pattern, so `changed('a/**', 'b/**')` is
                // the or of the two — the same shape a `paths:` list has.
                Ok(Value::Bool(args.iter().any(|pat| {
                    let pat = to_display(pat);
                    files
                        .iter()
                        .any(|f| crate::paths::matches(&pat, &to_display(f)))
                })))
            }
            // Status checks read the status the caller installed. `always()` is
            // the one that must stay true after a failure, which is how cleanup
            // steps run at all.
            "success" => Ok(Value::Bool(self.ctx.status() == "success")),
            "failure" => Ok(Value::Bool(self.ctx.status() == "failure")),
            "cancelled" => Ok(Value::Bool(self.ctx.status() == "cancelled")),
            "always" => Ok(Value::Bool(true)),
            other => Err(ExprError::UnknownFunction(other.to_string())),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ExprError {
    UnexpectedChar(char),
    UnterminatedString,
    BadNumber(String),
    UnclosedParen,
    TrailingDot(String),
    Unexpected(String),
    Trailing(String),
    UnknownFunction(String),
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedChar(c) => write!(f, "unexpected character {c:?} in an expression"),
            Self::UnterminatedString => write!(f, "a string literal was never closed"),
            Self::BadNumber(s) => write!(f, "{s:?} is not a number"),
            Self::UnclosedParen => write!(f, "a `(` was never closed"),
            Self::TrailingDot(p) => write!(f, "{p:?} ends with a `.` and no property"),
            Self::Unexpected(t) => write!(f, "unexpected {t} in an expression"),
            Self::Trailing(t) => write!(f, "unexpected {t} after the end of an expression"),
            Self::UnknownFunction(n) => write!(
                f,
                "no such function {n}(); available: contains, startsWith, endsWith, \
                 format, join, toJSON, fromJSON, success, failure, cancelled, always"
            ),
        }
    }
}

impl std::error::Error for ExprError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Context {
        let mut c = Context::new();
        c.set("matrix", json!({"target": "x86_64", "n": 2}))
            .set("env", json!({"NAME": "build", "EMPTY": ""}))
            .set(
                "needs",
                json!({"build": {"result": "success", "outputs": {"sha": "abc123"}}}),
            )
            .set(
                "steps",
                json!({"check": {"outputs": {"ok": "true", "no": "false"}}}),
            )
            .set(
                "github",
                json!({"ref_name": "main", "event_name": "submit"}),
            )
            .set("secrets", json!({"TOKEN": "s3cr3t"}));
        c
    }

    /// A `ci` scope the way `Dispatcher::ci_scope` builds one.
    fn with_changes(files: &[&str], known: bool) -> Context {
        let mut c = ctx();
        c.set(
            "ci",
            json!({
                "sha": "9183de2",
                "branch": "main",
                "changed_files": files,
                "changes_known": known,
            }),
        );
        c
    }

    /// The job-level monorepo gate. `contains(ci.changed_files, …)` cannot do
    /// this — on an array it is an equality test, so it would need the exact
    /// path of every file somebody might touch.
    #[test]
    fn changed_matches_a_glob_against_the_run_s_diff() {
        let c = with_changes(&["packages/api/src/main.rs", "README.md"], true);

        assert!(c.eval_condition("changed('packages/api/**')").unwrap());
        assert!(c.eval_condition("changed('*.md')").unwrap());
        assert!(!c.eval_condition("changed('packages/web/**')").unwrap());
        // A single `*` must not cross a separator here either.
        assert!(!c.eval_condition("changed('packages/*.rs')").unwrap());

        // Several patterns are an or, the same shape a `paths:` list has.
        assert!(
            c.eval_condition("changed('packages/web/**', 'packages/api/**')")
                .unwrap()
        );
        assert!(!c.eval_condition("changed('a/**', 'b/**')").unwrap());

        // And it composes with everything else, which is the point of it being
        // a function rather than a second filter block.
        assert!(
            c.eval_condition("changed('packages/api/**') && ci.branch == 'main'")
                .unwrap()
        );
        assert!(!c.eval_condition("!changed('packages/api/**')").unwrap());
    }

    /// The same fallback the path filters take, and for the same reason: a
    /// submit with no readable diff must build, not skip.
    #[test]
    fn changed_is_true_when_the_diff_could_not_be_read() {
        let c = with_changes(&[], false);
        assert!(c.eval_condition("changed('packages/api/**')").unwrap());
        assert!(c.eval_condition("changed('nothing/like/this')").unwrap());

        // A run this build cannot read at all gets an empty `ci` scope, which
        // must land the same way rather than skipping every job.
        let plain = ctx();
        assert!(plain.eval_condition("changed('anything')").unwrap());

        // A *known* empty diff is a real answer and does not match.
        assert!(
            !with_changes(&[], true)
                .eval_condition("changed('**')")
                .unwrap()
        );
    }

    #[test]
    fn the_ci_scope_reads_like_any_other() {
        let c = with_changes(&["a.rs"], true);
        assert_eq!(c.eval("ci.sha").unwrap(), json!("9183de2"));
        assert_eq!(c.eval("ci.changed_files.0").unwrap(), json!("a.rs"));
        assert_eq!(c.substitute("build ${{ ci.branch }}"), "build main");
        // The careful spelling an author can write when they want the
        // build-when-unsure default made explicit.
        assert!(
            c.eval_condition("!ci.changes_known || changed('a*')")
                .unwrap()
        );
    }

    #[test]
    fn context_paths_resolve_at_any_depth() {
        let c = ctx();
        assert_eq!(c.eval("matrix.target").unwrap(), json!("x86_64"));
        assert_eq!(c.eval("needs.build.outputs.sha").unwrap(), json!("abc123"));
        assert_eq!(c.eval("steps.check.outputs.ok").unwrap(), json!("true"));
        assert_eq!(c.eval("nope.nothing").unwrap(), Value::Null);
    }

    #[test]
    fn substitution_replaces_only_the_expression() {
        let c = ctx();
        assert_eq!(
            c.substitute("cargo build --target ${{ matrix.target }} -v"),
            "cargo build --target x86_64 -v"
        );
        assert_eq!(c.substitute("no expressions here"), "no expressions here");
        assert_eq!(
            c.substitute("${{ github.ref_name }}-${{ needs.build.outputs.sha }}"),
            "main-abc123"
        );
    }

    /// This is the bug the parser exists to prevent. With `split_once("&&")`
    /// then `split_once("||")`, this reads as `a == 'x' || (b && c)` in one
    /// order and `(a || b) && c` in another; `&&` must bind tighter.
    #[test]
    fn and_binds_tighter_than_or() {
        let mut c = Context::new();
        c.set("v", json!({"a": "x", "b": "no", "c": "yes"}));
        // false || (false && true) => false
        assert!(
            !c.eval_condition("v.a == 'WRONG' || v.b == 'WRONG' && v.c == 'yes'")
                .unwrap()
        );
        // true || anything => true
        assert!(
            c.eval_condition("v.a == 'x' || v.b == 'WRONG' && v.c == 'WRONG'")
                .unwrap()
        );
        // Parenthesised, the grouping changes the answer — proving parens work.
        assert!(
            !c.eval_condition("(v.a == 'x' || v.b == 'WRONG') && v.c == 'WRONG'")
                .unwrap()
        );
        assert!(
            c.eval_condition("(v.a == 'x' || v.b == 'WRONG') && v.c == 'yes'")
                .unwrap()
        );
    }

    #[test]
    fn negation_is_supported() {
        let c = ctx();
        assert!(c.eval_condition("!(matrix.target == 'aarch64')").unwrap());
        assert!(!c.eval_condition("!(matrix.target == 'x86_64')").unwrap());
        assert!(c.eval_condition("!env.EMPTY").unwrap());
    }

    #[test]
    fn comparisons_work_on_both_types() {
        let c = ctx();
        assert!(c.eval_condition("matrix.n == 2").unwrap());
        assert!(
            c.eval_condition("matrix.n == '2'").unwrap(),
            "loose equality"
        );
        assert!(c.eval_condition("matrix.n > 1").unwrap());
        assert!(c.eval_condition("matrix.n <= 2").unwrap());
        assert!(!c.eval_condition("matrix.n > 5").unwrap());
        assert!(c.eval_condition("matrix.target != 'aarch64'").unwrap());
    }

    /// A step output is produced by a shell, and `echo false` is how a shell
    /// says no. Under GitHub's own rule this would be truthy.
    #[test]
    fn the_string_false_is_falsey() {
        let c = ctx();
        assert!(c.eval_condition("steps.check.outputs.ok").unwrap());
        assert!(
            !c.eval_condition("steps.check.outputs.no").unwrap(),
            "a step that printed `false` must not gate a step open"
        );
    }

    #[test]
    fn an_absent_condition_is_true() {
        assert!(ctx().eval_condition("").unwrap());
        assert!(ctx().eval_condition("   ").unwrap());
    }

    #[test]
    fn a_condition_may_be_wrapped_in_the_expression_delimiters() {
        let c = ctx();
        assert!(
            c.eval_condition("${{ needs.build.result == 'success' }}")
                .unwrap()
        );
        assert!(c.eval_condition("needs.build.result == 'success'").unwrap());
    }

    #[test]
    fn or_yields_the_first_truthy_operand_not_a_boolean() {
        let c = ctx();
        assert_eq!(c.substitute("${{ env.EMPTY || 'fallback' }}"), "fallback");
        assert_eq!(c.substitute("${{ env.NAME || 'fallback' }}"), "build");
    }

    #[test]
    fn functions_cover_the_useful_set() {
        let mut c = ctx();
        c.set("list", json!(["a", "b", "c"]));
        assert!(
            c.eval_condition("contains(github.ref_name, 'mai')")
                .unwrap()
        );
        assert!(c.eval_condition("contains(list, 'b')").unwrap());
        assert!(!c.eval_condition("contains(list, 'z')").unwrap());
        assert!(
            c.eval_condition("startsWith(github.ref_name, 'ma')")
                .unwrap()
        );
        assert!(c.eval_condition("endsWith(github.ref_name, 'in')").unwrap());
        assert_eq!(
            c.substitute("${{ format('{0}-{1}', matrix.target, matrix.n) }}"),
            "x86_64-2"
        );
        assert_eq!(c.substitute("${{ join(list, '+') }}"), "a+b+c");
        assert_eq!(c.substitute("${{ toJSON(list) }}"), r#"["a","b","c"]"#);
        assert_eq!(c.substitute(r#"${{ fromJSON('{"k":7}').k }}"#), "7");
    }

    #[test]
    fn status_functions_read_the_installed_status() {
        let mut c = ctx();
        assert!(c.eval_condition("success()").unwrap());
        assert!(!c.eval_condition("failure()").unwrap());
        assert!(c.eval_condition("always()").unwrap());

        c.set_status("failure");
        assert!(!c.eval_condition("success()").unwrap());
        assert!(c.eval_condition("failure()").unwrap());
        // The point of always(): cleanup steps must still run after a failure.
        assert!(c.eval_condition("always()").unwrap());
    }

    /// A blanked expression turns `rm -rf /tmp/${{ x }}` into `rm -rf /tmp/`.
    /// Leaving the text makes the mistake visible and usually fails the step.
    #[test]
    fn an_unparseable_expression_is_left_verbatim_rather_than_blanked() {
        let c = ctx();
        assert_eq!(c.substitute("${{ ((( }}"), "${{ ((( }}");
        assert_eq!(
            c.substitute("a ${{ nosuchfn() }} b"),
            "a ${{ nosuchfn() }} b"
        );
        assert_eq!(c.substitute("${{ unclosed"), "${{ unclosed");
    }

    /// An unknown path is legitimately empty (an unset secret, a job with no
    /// outputs), which is different from an expression that failed to parse.
    #[test]
    fn an_unknown_path_substitutes_empty() {
        assert_eq!(ctx().substitute("[${{ env.NOPE }}]"), "[]");
    }

    /// Regression: scanning for `}}` without respecting quotes terminates the
    /// expression inside a string literal.
    #[test]
    fn a_close_brace_inside_a_string_literal_does_not_end_the_expression() {
        let c = ctx();
        assert_eq!(c.substitute("${{ format('a}}b') }}"), "a}}b");
        assert_eq!(c.substitute("${{ 'has }} inside' }}"), "has }} inside");
    }

    #[test]
    fn a_doubled_quote_inside_a_single_quoted_string_is_one_quote() {
        let c = ctx();
        assert_eq!(c.substitute("${{ 'it''s' }}"), "it's");
    }

    #[test]
    fn literals_parse() {
        let c = ctx();
        assert!(c.eval_condition("true").unwrap());
        assert!(!c.eval_condition("false").unwrap());
        assert!(!c.eval_condition("null").unwrap());
        assert_eq!(c.substitute("${{ 42 }}"), "42");
        assert_eq!(c.substitute("${{ 'hi' }}"), "hi");
    }

    /// A numeric path segment must not be swallowed by the number tokenizer.
    #[test]
    fn a_numeric_path_segment_still_indexes() {
        let mut c = Context::new();
        c.set("list", json!(["zero", "one"]));
        assert_eq!(c.substitute("${{ list.1 }}"), "one");
    }

    #[test]
    fn multiple_expressions_in_one_string_all_resolve() {
        let c = ctx();
        assert_eq!(
            c.substitute("${{ matrix.target }}/${{ matrix.n }}/${{ env.NAME }}"),
            "x86_64/2/build"
        );
    }

    /// Secrets flow through the same machinery; the masking that keeps them out
    /// of logs is the log writer's job, not the evaluator's.
    #[test]
    fn secrets_resolve_like_any_other_scope() {
        assert_eq!(ctx().substitute("${{ secrets.TOKEN }}"), "s3cr3t");
    }
}
