//! A message's arguments: up to [`Args::CAPACITY`] named values on the stack.

use crate::id::hash;

/// An argument's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    /// An integer: formatted by the locale (`1 234`), selected by plural category.
    Int(i64),
    /// A number with a fraction: formatted by the locale with up to three fraction digits
    /// unless the message's `NUMBER` says otherwise.
    Float(f64),
    /// Text, as it is (a player's name), selected by matching variant keys.
    Str(&'a str),
}

impl From<i64> for Value<'_> {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<i32> for Value<'_> {
    fn from(v: i32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<u32> for Value<'_> {
    fn from(v: u32) -> Self {
        Self::Int(i64::from(v))
    }
}
impl From<usize> for Value<'_> {
    fn from(v: usize) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<f64> for Value<'_> {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}
impl From<f32> for Value<'_> {
    fn from(v: f32) -> Self {
        Self::Float(f64::from(v))
    }
}
impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Self::Str(v)
    }
}

/// Named arguments, on the stack: building them never allocates.
///
/// ```
/// use engine_loc::{Args, Value};
///
/// let args = Args::new().with("count", 5).with("name", "Ofelia");
/// assert_eq!(args.get_named("count"), Some(Value::Int(5)));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Args<'a> {
    names: [u64; Args::CAPACITY],
    values: [Value<'a>; Args::CAPACITY],
    len: usize,
}

impl Default for Args<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Args<'a> {
    /// Most arguments a message takes; more are dropped (and reported in debug builds).
    pub const CAPACITY: usize = 8;

    /// No arguments.
    pub const fn new() -> Self {
        Self {
            names: [0; Self::CAPACITY],
            values: [Value::Int(0); Self::CAPACITY],
            len: 0,
        }
    }

    /// These arguments and `name` = `value` (replacing an argument of that name).
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<Value<'a>>) -> Self {
        self.set(name, value);
        self
    }

    /// Sets `name` to `value`.
    pub fn set(&mut self, name: &str, value: impl Into<Value<'a>>) {
        let h = hash(name.as_bytes());
        let value = value.into();
        if let Some(k) = self.names[..self.len].iter().position(|&n| n == h) {
            self.values[k] = value;
        } else if self.len < Self::CAPACITY {
            self.names[self.len] = h;
            self.values[self.len] = value;
            self.len += 1;
        } else {
            debug_assert!(false, "more than {} arguments", Self::CAPACITY);
        }
    }

    /// The argument whose name hashes to `name`.
    pub fn get(&self, name: u64) -> Option<Value<'a>> {
        self.names[..self.len]
            .iter()
            .position(|&n| n == name)
            .map(|k| self.values[k])
    }

    /// The argument `name`.
    pub fn get_named(&self, name: &str) -> Option<Value<'a>> {
        self.get(hash(name.as_bytes()))
    }

    /// How many arguments there are.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
