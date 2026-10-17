//! Values typed into number fields (`docs/research/ux/inspector.md`): arithmetic,
//! constants, and edits relative to the field's current value.
//!
//! ```
//! use engine_editor::expr::evaluate;
//!
//! assert_eq!(evaluate("2 * (1 + 0.5)", 0.0), Ok(3.0));
//! assert_eq!(evaluate("+=0.5", 2.0), Ok(2.5));
//! assert_eq!(evaluate("*1.2", 10.0), Ok(12.0));
//! assert!((evaluate("pi/2 rad", 0.0).unwrap() - 90.0).abs() < 1e-9);
//! ```

/// The value `text` gives a field that holds `current`:
/// - a number or an expression: `2.5`, `-3e2`, `2*pi`, `(1 + 2) / 4`, `2^10`, `sqrt(2)`;
///   constants `pi`, `tau` and `e`; functions `sqrt`, `abs`, `floor`, `ceil`, `round`,
///   `sin`, `cos`, `tan` (radians), `min` and `max` (two arguments);
/// - relative to the current value: `+=0.5`, `-=1`, `*=2`, `/=4`, and `*2` or `/2` alone;
/// - a trailing `deg` or `°` changes nothing (angle fields show degrees), `rad` turns
///   radians into degrees.
///
/// # Errors
///
/// Text that is not an expression, or a result that is not a finite number (a division
/// by zero), with what went wrong.
pub fn evaluate(text: &str, current: f64) -> Result<f64, String> {
    let text = text.trim();
    let (op, rest) = if let Some(r) = text.strip_prefix("+=") {
        (Some('+'), r)
    } else if let Some(r) = text.strip_prefix("-=") {
        (Some('-'), r)
    } else if let Some(r) = text.strip_prefix("*=").or_else(|| text.strip_prefix('*')) {
        (Some('*'), r)
    } else if let Some(r) = text.strip_prefix("/=").or_else(|| text.strip_prefix('/')) {
        (Some('/'), r)
    } else {
        (None, text)
    };
    let (rest, scale) = unit(rest.trim());
    let value = Parser::new(rest).parse()? * scale;
    let value = match op {
        Some('+') => current + value,
        Some('-') => current - value,
        Some('*') => current * value,
        Some('/') => current / value,
        _ => value,
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("`{text}` is not a finite number"))
    }
}

/// A trailing unit: the text without it, and its factor into degrees.
fn unit(text: &str) -> (&str, f64) {
    if let Some(r) = text.strip_suffix("deg").or_else(|| text.strip_suffix('°')) {
        (r.trim_end(), 1.0)
    } else if let Some(r) = text.strip_suffix("rad") {
        (r.trim_end(), 180.0 / core::f64::consts::PI)
    } else {
        (text, 1.0)
    }
}

/// A recursive descent parser over the text's bytes.
struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn parse(mut self) -> Result<f64, String> {
        if self.text.is_empty() {
            return Err(String::from("an empty value"));
        }
        let v = self.sum()?;
        self.space();
        if self.at < self.text.len() {
            return Err(format!(
                "`{}` is not understood in `{}`",
                &self.text[self.at..],
                self.text
            ));
        }
        Ok(v)
    }

    fn space(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        self.space();
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// `term (('+' | '-') term)*`
    fn sum(&mut self) -> Result<f64, String> {
        let mut v = self.product()?;
        loop {
            if self.eat(b'+') {
                v += self.product()?;
            } else if self.eat(b'-') {
                v -= self.product()?;
            } else {
                return Ok(v);
            }
        }
    }

    /// `unary (('*' | '/' | '%') unary)*`
    fn product(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            if self.eat(b'*') {
                v *= self.unary()?;
            } else if self.eat(b'/') {
                v /= self.unary()?;
            } else if self.eat(b'%') {
                v %= self.unary()?;
            } else {
                return Ok(v);
            }
        }
    }

    /// `('-' | '+') unary | power`: a sign binds looser than `^` (`-2^2` is -4).
    fn unary(&mut self) -> Result<f64, String> {
        if self.eat(b'-') {
            Ok(-self.unary()?)
        } else if self.eat(b'+') {
            self.unary()
        } else {
            self.power()
        }
    }

    /// `primary ('^' unary)?`, right to left (`2^-1`, `2^3^2`).
    fn power(&mut self) -> Result<f64, String> {
        let base = self.primary()?;
        if self.eat(b'^') {
            Ok(base.powf(self.unary()?))
        } else {
            Ok(base)
        }
    }

    /// A number, a constant, a function call or a parenthesized sum.
    fn primary(&mut self) -> Result<f64, String> {
        self.space();
        if self.eat(b'(') {
            let v = self.sum()?;
            if !self.eat(b')') {
                return Err(format!("a `(` without its `)` in `{}`", self.text));
            }
            return Ok(v);
        }
        let start = self.at;
        match self.peek() {
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                while self.peek().is_some_and(|c| c.is_ascii_digit() || c == b'.') {
                    self.at += 1;
                }
                // An exponent: `e` followed by a digit or a sign and a digit.
                if matches!(self.peek(), Some(b'e' | b'E')) {
                    let bytes = self.text.as_bytes();
                    let next = bytes.get(self.at + 1).copied();
                    let after = bytes.get(self.at + 2).copied();
                    if next.is_some_and(|c| c.is_ascii_digit())
                        || (matches!(next, Some(b'+' | b'-'))
                            && after.is_some_and(|c| c.is_ascii_digit()))
                    {
                        self.at += 2;
                        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            self.at += 1;
                        }
                    }
                }
                let number = &self.text[start..self.at];
                number
                    .parse()
                    .map_err(|_| format!("`{number}` is not a number"))
            }
            Some(c) if c.is_ascii_alphabetic() => {
                while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                    self.at += 1;
                }
                let name = &self.text[start..self.at];
                match name {
                    "pi" => Ok(core::f64::consts::PI),
                    "tau" => Ok(core::f64::consts::TAU),
                    "e" => Ok(core::f64::consts::E),
                    _ => self.call(name),
                }
            }
            _ => Err(format!("expected a number in `{}`", self.text)),
        }
    }

    /// `name(sum)` or `name(sum, sum)`.
    fn call(&mut self, name: &str) -> Result<f64, String> {
        if !self.eat(b'(') {
            return Err(format!("`{name}` is not a constant or a function"));
        }
        let a = self.sum()?;
        let b = if self.eat(b',') {
            Some(self.sum()?)
        } else {
            None
        };
        if !self.eat(b')') {
            return Err(format!("`{name}(` without its `)`"));
        }
        let one = |f: fn(f64) -> f64| match b {
            None => Ok(f(a)),
            Some(_) => Err(format!("`{name}` takes one value")),
        };
        match name {
            "sqrt" => one(f64::sqrt),
            "abs" => one(f64::abs),
            "floor" => one(f64::floor),
            "ceil" => one(f64::ceil),
            "round" => one(f64::round),
            "sin" => one(f64::sin),
            "cos" => one(f64::cos),
            "tan" => one(f64::tan),
            "min" | "max" => {
                let b = b.ok_or_else(|| format!("`{name}` takes two values"))?;
                Ok(if name == "min" { a.min(b) } else { a.max(b) })
            }
            _ => Err(format!("`{name}` is not a function")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_constants_and_functions() {
        let eval = |t| evaluate(t, 0.0).unwrap();
        assert_eq!(eval("1 + 2 * 3"), 7.0);
        assert_eq!(eval("(1 + 2) * 3"), 9.0);
        assert_eq!(eval("2^3^2"), 512.0);
        assert_eq!(eval("-2^2"), -4.0);
        assert_eq!(eval("2^-1"), 0.5);
        assert_eq!(eval("-3e2 + 1.5E-1"), -299.85);
        assert_eq!(eval("10 % 4"), 2.0);
        assert_eq!(eval("max(2, 3) + min(1, -1)"), 2.0);
        assert_eq!(eval("sqrt(16) + abs(-1)"), 5.0);
        assert!((eval("2*pi") - core::f64::consts::TAU).abs() < 1e-12);
        assert!((eval("e") - core::f64::consts::E).abs() < 1e-12);
        assert_eq!(eval(" .5 "), 0.5);
    }

    #[test]
    fn relative_edits_and_units() {
        assert_eq!(evaluate("+=0.5", 2.0), Ok(2.5));
        assert_eq!(evaluate("-=1", 2.0), Ok(1.0));
        assert_eq!(evaluate("*=2", 3.0), Ok(6.0));
        assert_eq!(evaluate("*1.5", 4.0), Ok(6.0));
        assert_eq!(evaluate("/4", 2.0), Ok(0.5));
        assert_eq!(
            evaluate("-1", 5.0),
            Ok(-1.0),
            "a negative number, not relative"
        );
        assert_eq!(evaluate("45deg", 0.0), Ok(45.0));
        assert_eq!(evaluate("30 °", 0.0), Ok(30.0));
        assert!((evaluate("pi rad", 0.0).unwrap() - 180.0).abs() < 1e-9);
        assert!((evaluate("+=pi/4 rad", 10.0).unwrap() - 55.0).abs() < 1e-9);
    }

    #[test]
    fn bad_text_is_refused_with_a_reason() {
        for bad in [
            "",
            "abc",
            "1 +",
            "(1",
            "1 2",
            "foo(1)",
            "1/0",
            "sqrt(1, 2)",
            "max(1)",
        ] {
            assert!(evaluate(bad, 0.0).is_err(), "`{bad}` accepted");
        }
        assert!(evaluate("1/0", 0.0).unwrap_err().contains("finite"));
    }
}
