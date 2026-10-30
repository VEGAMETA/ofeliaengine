//! Transition conditions (ADR 0124's E12-5): an expression over a graph's parameters
//! (`speed > 0.1 and grounded`), compiled once into a few stack operations and checked every
//! tick without allocating or looking a name up.
//!
//! - **Values:** numbers (`0.1`, `.5`, `1e-3`), parameters by name, `true` and `false`.
//! - **Arithmetic:** `+ - * /` and a unary `-`.
//! - **Comparisons:** `< <= > >= ==`, and `~=` or `!=`, once each: `a < b < c` is refused
//!   (join two with `and`).
//! - **Logic:** `and`, `or`, `not` (also `&&`, `||`, `!`). `not` binds looser than a
//!   comparison, so `not speed > 1` is `not (speed > 1)`; `and` binds tighter than `or`.
//! - Parentheses group.
//!
//! A flag or a trigger alone holds when it is not 0. The triggers a condition names are used
//! up when its transition goes ([`Condition::triggers`]); a number is true when not 0.
//!
//! ```
//! use engine_anim::Condition;
//!
//! let slot = |name: &str| match name {
//!     "speed" => Some((0, false)),
//!     "grounded" => Some((1, false)),
//!     _ => None,
//! };
//! let c = Condition::compile("speed > 0.1 and grounded", slot).unwrap();
//! assert!(c.holds(&[0.5, 1.0]));
//! assert!(!c.holds(&[0.5, 0.0]));
//! ```

use std::fmt;

/// The deepest a condition stacks values: deeper ones are refused.
pub const MAX_DEPTH: usize = 32;

/// What a name in a condition is: its parameter's slot, and whether it is a trigger.
pub type ParamSlot = (u16, bool);

/// One step of a compiled condition, on a stack of values.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    /// A parameter's value, by slot.
    Value(u16),
    Const(f32),
    Neg,
    Not,
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

/// A compiled condition: [`Condition::holds`] checks it against the parameters' values.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Condition {
    ops: Box<[Op]>,
    triggers: Box<[u16]>,
}

/// Why a condition does not compile: where in its text (a byte offset and a length) and
/// what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionError {
    /// The byte where the fault begins.
    pub at: usize,
    /// How many bytes it spans (0 at the end of the text).
    pub len: usize,
    /// What is wrong, in a few words.
    pub message: String,
}

impl fmt::Display for ConditionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at {})", self.message, self.at + 1)
    }
}

impl std::error::Error for ConditionError {}

impl Condition {
    /// The condition that always holds (what an empty text compiles to).
    pub fn always() -> Self {
        Self::default()
    }

    /// Whether it always holds (no text).
    pub fn is_always(&self) -> bool {
        self.ops.is_empty()
    }

    /// Condition `text` compiled, `slot` naming each parameter it reads (none: no such
    /// parameter). Blank text always holds.
    ///
    /// # Errors
    ///
    /// A word that is no parameter, a character a condition has no use for, a missing value
    /// or parenthesis, a chained comparison, or more than [`MAX_DEPTH`] values pending; with
    /// where it is.
    pub fn compile(
        text: &str,
        slot: impl FnMut(&str) -> Option<ParamSlot>,
    ) -> Result<Self, ConditionError> {
        let tokens = tokens(text)?;
        if tokens.len() == 1 {
            return Ok(Self::always());
        }
        let mut parser = Parser {
            tokens: &tokens,
            at: 0,
            ops: Vec::new(),
            triggers: Vec::new(),
            slot,
        };
        parser.or()?;
        let next = parser.peek();
        if next.kind != Kind::End {
            return Err(fault(next, "the condition ended before this"));
        }
        let Parser {
            ops, mut triggers, ..
        } = parser;
        // How deep the stack goes.
        let mut depth = 0usize;
        let mut deepest = 0usize;
        for op in &ops {
            match op {
                Op::Value(_) | Op::Const(_) => depth += 1,
                Op::Neg | Op::Not => {}
                _ => depth = depth.saturating_sub(1),
            }
            deepest = deepest.max(depth);
        }
        if deepest > MAX_DEPTH {
            return Err(ConditionError {
                at: 0,
                len: text.len(),
                message: format!("too deep: more than {MAX_DEPTH} values pending at once"),
            });
        }
        triggers.sort_unstable();
        triggers.dedup();
        Ok(Self {
            ops: ops.into_boxed_slice(),
            triggers: triggers.into_boxed_slice(),
        })
    }

    /// The trigger parameters it names, by slot: used up when its transition goes.
    pub fn triggers(&self) -> &[u16] {
        &self.triggers
    }

    /// Whether it holds for the parameters' `values`, by slot (a slot past them reads 0).
    pub fn holds(&self, values: &[f32]) -> bool {
        if self.ops.is_empty() {
            return true;
        }
        let mut stack = [0.0f32; MAX_DEPTH];
        let mut top = 0usize;
        let truth = |v: f32| v != 0.0;
        let flag = |b: bool| if b { 1.0 } else { 0.0 };
        for &op in &self.ops {
            match op {
                Op::Value(slot) => {
                    let Some(v) = stack.get_mut(top) else {
                        return false;
                    };
                    *v = values.get(usize::from(slot)).copied().unwrap_or(0.0);
                    top += 1;
                }
                Op::Const(c) => {
                    let Some(v) = stack.get_mut(top) else {
                        return false;
                    };
                    *v = c;
                    top += 1;
                }
                Op::Neg | Op::Not => {
                    let Some(v) = top.checked_sub(1).and_then(|t| stack.get_mut(t)) else {
                        return false;
                    };
                    *v = if op == Op::Neg { -*v } else { flag(!truth(*v)) };
                }
                _ => {
                    if top < 2 {
                        return false;
                    }
                    let (a, b) = (stack[top - 2], stack[top - 1]);
                    stack[top - 2] = match op {
                        Op::Add => a + b,
                        Op::Sub => a - b,
                        Op::Mul => a * b,
                        Op::Div => a / b,
                        Op::Lt => flag(a < b),
                        Op::Le => flag(a <= b),
                        Op::Gt => flag(a > b),
                        Op::Ge => flag(a >= b),
                        Op::Eq => flag(a == b),
                        Op::Ne => flag(a != b),
                        Op::And => flag(truth(a) && truth(b)),
                        _ => flag(truth(a) || truth(b)),
                    };
                    top -= 1;
                }
            }
        }
        top == 1 && truth(stack[0])
    }
}

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Number(f32),
    Name,
    True,
    False,
    And,
    Or,
    Not,
    Plus,
    Minus,
    Star,
    Slash,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Equal,
    NotEqual,
    Open,
    Close,
    End,
}

/// A token and where it lies in the text.
#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    at: usize,
}

fn fault(t: Token<'_>, message: &str) -> ConditionError {
    ConditionError {
        at: t.at,
        len: t.text.len(),
        message: message.to_owned(),
    }
}

/// The text's tokens, the last one its end.
fn tokens(text: &str) -> Result<Vec<Token<'_>>, ConditionError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let kind = if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            match &text[start..i] {
                "and" => Kind::And,
                "or" => Kind::Or,
                "not" => Kind::Not,
                "true" => Kind::True,
                "false" => Kind::False,
                _ => Kind::Name,
            }
        } else if c.is_ascii_digit()
            || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            // An exponent: `1e-3`.
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            match text[start..i].parse::<f32>() {
                Ok(v) => Kind::Number(v),
                Err(_) => {
                    return Err(ConditionError {
                        at: start,
                        len: i - start,
                        message: format!("`{}` is not a number", &text[start..i]),
                    });
                }
            }
        } else {
            let two = text.get(i..i + 2).unwrap_or("");
            let (kind, len) = match two {
                "<=" => (Kind::LessEqual, 2),
                ">=" => (Kind::GreaterEqual, 2),
                "==" => (Kind::Equal, 2),
                "~=" | "!=" => (Kind::NotEqual, 2),
                "&&" => (Kind::And, 2),
                "||" => (Kind::Or, 2),
                _ => match c {
                    b'<' => (Kind::Less, 1),
                    b'>' => (Kind::Greater, 1),
                    b'+' => (Kind::Plus, 1),
                    b'-' => (Kind::Minus, 1),
                    b'*' => (Kind::Star, 1),
                    b'/' => (Kind::Slash, 1),
                    b'!' => (Kind::Not, 1),
                    b'(' => (Kind::Open, 1),
                    b')' => (Kind::Close, 1),
                    _ => {
                        let len = text[i..].chars().next().map_or(1, char::len_utf8);
                        return Err(ConditionError {
                            at: i,
                            len,
                            message: format!("`{}` is not part of a condition", &text[i..i + len]),
                        });
                    }
                },
            };
            i += len;
            kind
        };
        out.push(Token {
            kind,
            text: &text[start..i],
            at: start,
        });
    }
    out.push(Token {
        kind: Kind::End,
        text: "",
        at: text.len(),
    });
    Ok(out)
}

/// A recursive descent over the tokens, writing the operations in the order they run.
struct Parser<'t, 'a, F> {
    tokens: &'t [Token<'a>],
    at: usize,
    ops: Vec<Op>,
    triggers: Vec<u16>,
    slot: F,
}

impl<'a, F: FnMut(&str) -> Option<ParamSlot>> Parser<'_, 'a, F> {
    fn peek(&self) -> Token<'a> {
        self.tokens[self.at.min(self.tokens.len() - 1)]
    }

    fn take(&mut self) -> Token<'a> {
        let t = self.peek();
        if t.kind != Kind::End {
            self.at += 1;
        }
        t
    }

    /// `and` terms joined by `or`.
    fn or(&mut self) -> Result<(), ConditionError> {
        self.and()?;
        while self.peek().kind == Kind::Or {
            self.take();
            self.and()?;
            self.ops.push(Op::Or);
        }
        Ok(())
    }

    /// `not` terms joined by `and`.
    fn and(&mut self) -> Result<(), ConditionError> {
        self.not()?;
        while self.peek().kind == Kind::And {
            self.take();
            self.not()?;
            self.ops.push(Op::And);
        }
        Ok(())
    }

    /// A comparison, or `not` before one.
    fn not(&mut self) -> Result<(), ConditionError> {
        if self.peek().kind == Kind::Not {
            self.take();
            self.not()?;
            self.ops.push(Op::Not);
            return Ok(());
        }
        self.compare()
    }

    /// A sum, compared once with another.
    fn compare(&mut self) -> Result<(), ConditionError> {
        self.sum()?;
        let op = match self.peek().kind {
            Kind::Less => Op::Lt,
            Kind::LessEqual => Op::Le,
            Kind::Greater => Op::Gt,
            Kind::GreaterEqual => Op::Ge,
            Kind::Equal => Op::Eq,
            Kind::NotEqual => Op::Ne,
            _ => return Ok(()),
        };
        self.take();
        self.sum()?;
        self.ops.push(op);
        let next = self.peek();
        if matches!(
            next.kind,
            Kind::Less
                | Kind::LessEqual
                | Kind::Greater
                | Kind::GreaterEqual
                | Kind::Equal
                | Kind::NotEqual
        ) {
            return Err(fault(next, "compare once: join two comparisons with `and`"));
        }
        Ok(())
    }

    /// Products joined by `+` and `-`.
    fn sum(&mut self) -> Result<(), ConditionError> {
        self.product()?;
        loop {
            let op = match self.peek().kind {
                Kind::Plus => Op::Add,
                Kind::Minus => Op::Sub,
                _ => return Ok(()),
            };
            self.take();
            self.product()?;
            self.ops.push(op);
        }
    }

    /// Values joined by `*` and `/`.
    fn product(&mut self) -> Result<(), ConditionError> {
        self.unary()?;
        loop {
            let op = match self.peek().kind {
                Kind::Star => Op::Mul,
                Kind::Slash => Op::Div,
                _ => return Ok(()),
            };
            self.take();
            self.unary()?;
            self.ops.push(op);
        }
    }

    /// A value, or `-` before one.
    fn unary(&mut self) -> Result<(), ConditionError> {
        if self.peek().kind == Kind::Minus {
            self.take();
            self.unary()?;
            self.ops.push(Op::Neg);
            return Ok(());
        }
        self.value()
    }

    /// A number, a parameter, `true`, `false` or a condition in parentheses.
    fn value(&mut self) -> Result<(), ConditionError> {
        let t = self.take();
        match t.kind {
            Kind::Number(v) => self.ops.push(Op::Const(v)),
            Kind::True => self.ops.push(Op::Const(1.0)),
            Kind::False => self.ops.push(Op::Const(0.0)),
            Kind::Name => {
                let Some((slot, trigger)) = (self.slot)(t.text) else {
                    return Err(fault(t, &format!("no parameter `{}`", t.text)));
                };
                if trigger {
                    self.triggers.push(slot);
                }
                self.ops.push(Op::Value(slot));
            }
            Kind::Open => {
                self.or()?;
                let close = self.take();
                if close.kind != Kind::Close {
                    return Err(fault(close, "expected `)`"));
                }
            }
            Kind::End => return Err(fault(t, "expected a value at the end")),
            _ => return Err(fault(t, &format!("expected a value, not `{}`", t.text))),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// speed, grounded, jump (a trigger), crouch.
    fn slot(name: &str) -> Option<ParamSlot> {
        match name {
            "speed" => Some((0, false)),
            "grounded" => Some((1, false)),
            "jump" => Some((2, true)),
            "crouch" => Some((3, false)),
            _ => None,
        }
    }

    fn holds(text: &str, values: &[f32]) -> bool {
        Condition::compile(text, slot).unwrap().holds(values)
    }

    /// Comparisons and logic over the parameters, the precedence as the module says.
    #[test]
    fn conditions_hold_as_their_text_says() {
        assert!(holds("speed > 0.1 and grounded", &[0.5, 1.0]));
        assert!(!holds("speed > 0.1 and grounded", &[0.5, 0.0]));
        assert!(!holds("speed > 0.1 and grounded", &[0.05, 1.0]));
        assert!(holds("speed <= 0.1 or not grounded", &[2.0, 0.0]));
        assert!(holds("1 + 2 * 3 == 7", &[]));
        assert!(holds("(1 + 2) * 3 == 9", &[]));
        assert!(holds("-speed < 0", &[2.0]));
        assert!(holds("speed - 1 >= .5", &[1.5]));
        assert!(holds("speed / 2 == 1e0", &[2.0]));
        // `not` binds looser than a comparison, `and` tighter than `or`.
        assert!(!holds("not speed > 1", &[2.0]));
        assert!(holds("grounded or crouch and false", &[0.0, 1.0, 0.0, 1.0]));
        assert!(!holds(
            "(grounded or crouch) and false",
            &[0.0, 1.0, 0.0, 1.0]
        ));
        // The other spellings.
        assert!(holds("grounded && !crouch", &[0.0, 1.0, 0.0, 0.0]));
        assert!(holds("speed != 0 || false", &[1.0]));
        assert!(holds("speed ~= 0", &[1.0]));
        assert!(holds("grounded == true", &[0.0, 1.0]));
        // A slot past the values reads 0; a blank text always holds.
        assert!(holds("crouch == 0", &[1.0]));
        assert!(Condition::compile("  ", slot).unwrap().is_always());
        assert!(holds("", &[]));
    }

    /// The triggers a condition names, once each, by slot.
    #[test]
    fn a_conditions_triggers_are_found() {
        let c = Condition::compile("jump and grounded or jump and speed > 2", slot).unwrap();
        assert_eq!(c.triggers(), &[2]);
        assert!(c.holds(&[0.0, 1.0, 1.0]));
        assert!(!c.holds(&[0.0, 1.0, 0.0]), "the trigger not fired");
        let plain = Condition::compile("speed > 1", slot).unwrap();
        assert!(plain.triggers().is_empty());
    }

    /// What does not compile, with where it is.
    #[test]
    fn faults_are_named_where_they_are() {
        let error = |text: &str| Condition::compile(text, slot).unwrap_err();
        let e = error("speeed > 1");
        assert_eq!((e.at, e.len), (0, 6));
        assert!(e.message.contains("no parameter `speeed`"), "{}", e.message);
        let e = error("speed >");
        assert_eq!(e.at, 7);
        assert!(e.message.contains("at the end"), "{}", e.message);
        let e = error("(speed > 1");
        assert!(e.message.contains("`)`"), "{}", e.message);
        let e = error("0 < speed < 2");
        assert_eq!(e.at, 10);
        assert!(e.message.contains("compare once"), "{}", e.message);
        let e = error("speed @ 2");
        assert_eq!((e.at, e.len), (6, 1));
        assert!(e.message.contains("`@`"), "{}", e.message);
        let e = error("speed > 1 grounded");
        assert_eq!(e.at, 10);
        assert!(e.message.contains("ended"), "{}", e.message);
        let e = error("speed > )");
        assert!(e.message.contains("not `)`"), "{}", e.message);
        // Unicode: the fault spans its character.
        let e = error("speed ≥ 1");
        assert_eq!((e.at, e.len), (6, 3));
    }

    /// A condition deeper than the stack is refused; one at the limit holds.
    #[test]
    fn a_condition_too_deep_is_refused() {
        let nested = |n: usize| format!("{}1{}", "(1 + ".repeat(n), ")".repeat(n));
        assert!(Condition::compile(&format!("{} > 0", nested(MAX_DEPTH - 2)), slot).is_ok());
        let e = Condition::compile(&format!("{} > 0", nested(MAX_DEPTH + 1)), slot).unwrap_err();
        assert!(e.message.contains("too deep"), "{}", e.message);
    }
}
