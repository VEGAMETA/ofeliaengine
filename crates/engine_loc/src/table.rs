//! A locale's messages, compiled (ADR 0102): what the cook writes into the pack and the
//! runtime formats from.
//!
//! Every message and term is a pattern: a span of operations in one flat list. Literal text
//! is one string the operations point into; select expressions' variants are spans of their
//! own. Identifiers, attribute names, variable names and variant keys are 64-bit hashes
//! ([`crate::id::hash`]); names are kept beside them for diagnostics.

/// A range of a table's list (operations, variants, text, term arguments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Span {
    /// First element.
    pub(crate) start: u32,
    /// Number of elements.
    pub(crate) len: u32,
}

impl Span {
    pub(crate) fn range(self) -> core::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// How a number is formatted (Fluent's `NUMBER` options that the runtime honours).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumberFormat {
    /// Fraction digits always shown (padded with zeros).
    pub min_fraction: u8,
    /// Fraction digits at most (rounded).
    pub max_fraction: u8,
    /// Grouping separators (`1 234`).
    pub grouping: bool,
    /// Select by ordinal categories (`1st`, `2nd`) instead of cardinal ones.
    pub ordinal: bool,
}

impl Default for NumberFormat {
    fn default() -> Self {
        Self {
            min_fraction: 0,
            max_fraction: 3,
            grouping: true,
            ordinal: false,
        }
    }
}

/// What a select expression chooses by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Selector {
    /// A variable: a number by exact value then plural category, text by key.
    Variable {
        /// Its name's hash.
        name: u64,
        /// How a number is formatted before it is selected by.
        format: NumberFormat,
    },
    /// An attribute of a term (`{ -brand.gender -> ... }`), selected by key.
    TermAttribute {
        /// The term's hash.
        term: u64,
        /// The attribute's hash.
        attribute: u64,
    },
    /// A literal: text by key, a number by value then category.
    Text(Span),
    /// A number literal.
    Number(f64),
}

/// A variant's key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Key {
    /// A CLDR plural category: zero, one, two, few, many, other (0 to 5).
    Category(u8),
    /// An exact number (`[0]`).
    Number(f64),
    /// Any other identifier (`[masculine]`), by hash.
    Name(u64),
}

/// A variant of a select expression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Variant {
    /// What selects it.
    pub(crate) key: Key,
    /// Its pattern.
    pub(crate) pattern: Span,
}

/// A literal argument passed to a term (`-brand(case: "genitive")`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TermArgument {
    /// The argument's name, hashed.
    pub(crate) name: u64,
    /// Its value: text or a number.
    pub(crate) value: TermValue,
}

/// A term argument's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TermValue {
    /// Text (a span of the table's text).
    Text(Span),
    /// A number.
    Number(f64),
}

/// An operation of a pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Op {
    /// Literal text (a span of the table's text).
    Text(Span),
    /// A variable, formatted (numbers by the locale and `format`).
    Variable {
        /// Its name's hash.
        name: u64,
        /// Its name, for a missing variable (a span of the text).
        written: Span,
        /// How a number is formatted.
        format: NumberFormat,
    },
    /// Another message's (or term's) pattern.
    Reference {
        /// The message's or term's hash.
        target: u64,
        /// The attribute's hash; 0 for the value.
        attribute: u64,
        /// A term (its own arguments) rather than a message (the caller's).
        term: bool,
        /// The term's arguments (a span of the table's term arguments).
        arguments: Span,
        /// Its identifier, for a missing reference (a span of the text).
        written: Span,
    },
    /// A select expression.
    Select {
        /// What chooses.
        selector: Selector,
        /// The variants (a span of the table's variants).
        variants: Span,
        /// The default variant's index within them.
        default: u32,
    },
}

/// A message, a term or one of their attributes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Entry {
    /// The identifier's hash.
    pub(crate) hash: u64,
    /// The attribute's hash; 0 for the value.
    pub(crate) attribute: u64,
    /// A term (`-brand`) rather than a message.
    pub(crate) term: bool,
    /// Its pattern (a span of the operations).
    pub(crate) pattern: Span,
    /// Its identifier as written (a span of the text).
    pub(crate) name: Span,
}

/// A locale's compiled messages.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Strings {
    /// The locale (a BCP 47 tag: `ru`, `pt-BR`, `zh-Hans`).
    pub locale: String,
    /// Messages, terms and attributes, sorted by (term, hash, attribute).
    pub(crate) entries: Vec<Entry>,
    pub(crate) ops: Vec<Op>,
    pub(crate) variants: Vec<Variant>,
    pub(crate) arguments: Vec<TermArgument>,
    pub(crate) text: String,
}

impl Strings {
    /// The entry of `hash` (a term's when `term`) and `attribute`.
    pub(crate) fn find(&self, term: bool, hash: u64, attribute: u64) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| (e.term, e.hash, e.attribute).cmp(&(term, hash, attribute)))
            .ok()
            .map(|k| &self.entries[k])
    }

    pub(crate) fn text(&self, span: Span) -> &str {
        self.text.get(span.range()).unwrap_or("")
    }

    /// Messages (not terms), their values and attributes counted once each.
    pub fn messages(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.term && e.attribute == 0)
            .count()
    }

    /// The identifiers of the messages (not terms), each once: what a check compares.
    pub fn message_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries
            .iter()
            .filter(|e| !e.term && e.attribute == 0)
            .map(|e| self.text(e.name))
    }

    /// Whether the message `name` (its value or any attribute) is here.
    pub fn has_message(&self, name: &str) -> bool {
        let h = crate::id::hash(name.as_bytes());
        self.entries.iter().any(|e| !e.term && e.hash == h)
    }

    /// The table as bytes (the pack's `Strings` entry).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(
            64 + self.text.len() + self.ops.len() * 32,
        ));
        w.0.extend_from_slice(MAGIC);
        w.str(&self.locale);
        w.u32(self.entries.len() as u32);
        for e in &self.entries {
            w.u64(e.hash);
            w.u64(e.attribute);
            w.u8(u8::from(e.term));
            w.span(e.pattern);
            w.span(e.name);
        }
        w.u32(self.ops.len() as u32);
        for op in &self.ops {
            w.op(op);
        }
        w.u32(self.variants.len() as u32);
        for v in &self.variants {
            w.key(v.key);
            w.span(v.pattern);
        }
        w.u32(self.arguments.len() as u32);
        for a in &self.arguments {
            w.u64(a.name);
            w.term_value(a.value);
        }
        w.str(&self.text);
        w.0
    }

    /// A table from [`Self::to_bytes`]; `None` if the bytes are not one (or point outside
    /// themselves).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes.strip_prefix(MAGIC)?);
        let locale = r.str()?.to_owned();
        let n = r.count(33)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push(Entry {
                hash: r.u64()?,
                attribute: r.u64()?,
                term: r.u8()? != 0,
                pattern: r.span()?,
                name: r.span()?,
            });
        }
        let n = r.count(9)?;
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            ops.push(r.op()?);
        }
        let n = r.count(17)?;
        let mut variants = Vec::with_capacity(n);
        for _ in 0..n {
            variants.push(Variant {
                key: r.key()?,
                pattern: r.span()?,
            });
        }
        let n = r.count(17)?;
        let mut arguments = Vec::with_capacity(n);
        for _ in 0..n {
            arguments.push(TermArgument {
                name: r.u64()?,
                value: r.term_value()?,
            });
        }
        let text = r.str()?.to_owned();
        if !r.0.is_empty() {
            return None;
        }
        let strings = Self {
            locale,
            entries,
            ops,
            variants,
            arguments,
            text,
        };
        strings.valid().then_some(strings)
    }

    /// Every span inside its list, every text span on character boundaries, entries sorted:
    /// a table read from bytes cannot make the runtime index out of bounds.
    fn valid(&self) -> bool {
        let text = |s: Span| {
            let r = s.range();
            r.end <= self.text.len()
                && self.text.is_char_boundary(r.start)
                && self.text.is_char_boundary(r.end)
        };
        let ops = |s: Span| s.range().end <= self.ops.len();
        let sorted = self.entries.windows(2).all(|w| {
            (w[0].term, w[0].hash, w[0].attribute) < (w[1].term, w[1].hash, w[1].attribute)
        });
        sorted
            && self.entries.iter().all(|e| ops(e.pattern) && text(e.name))
            && self.variants.iter().all(|v| ops(v.pattern))
            && self.arguments.iter().all(|a| match a.value {
                TermValue::Text(s) => text(s),
                TermValue::Number(_) => true,
            })
            && self.ops.iter().all(|op| match *op {
                Op::Text(s) => text(s),
                Op::Variable { written, .. } => text(written),
                Op::Reference {
                    arguments, written, ..
                } => arguments.range().end <= self.arguments.len() && text(written),
                Op::Select {
                    selector,
                    variants,
                    default,
                } => {
                    variants.range().end <= self.variants.len()
                        && default < variants.len
                        && match selector {
                            Selector::Text(s) => text(s),
                            _ => true,
                        }
                }
            })
    }
}

/// First bytes of a table; the digits are its version.
const MAGIC: &[u8; 8] = b"ENGLOC01";

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
    fn span(&mut self, s: Span) {
        self.u32(s.start);
        self.u32(s.len);
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn format(&mut self, f: NumberFormat) {
        self.u8(f.min_fraction);
        self.u8(f.max_fraction);
        self.u8(u8::from(f.grouping) | (u8::from(f.ordinal) << 1));
    }
    fn key(&mut self, k: Key) {
        match k {
            Key::Category(c) => {
                self.u8(0);
                self.u64(u64::from(c));
            }
            Key::Number(n) => {
                self.u8(1);
                self.f64(n);
            }
            Key::Name(h) => {
                self.u8(2);
                self.u64(h);
            }
        }
    }
    fn term_value(&mut self, v: TermValue) {
        match v {
            TermValue::Text(s) => {
                self.u8(0);
                self.span(s);
            }
            TermValue::Number(n) => {
                self.u8(1);
                self.f64(n);
            }
        }
    }
    fn op(&mut self, op: &Op) {
        match *op {
            Op::Text(s) => {
                self.u8(0);
                self.span(s);
            }
            Op::Variable {
                name,
                written,
                format,
            } => {
                self.u8(1);
                self.u64(name);
                self.span(written);
                self.format(format);
            }
            Op::Reference {
                target,
                attribute,
                term,
                arguments,
                written,
            } => {
                self.u8(2);
                self.u64(target);
                self.u64(attribute);
                self.u8(u8::from(term));
                self.span(arguments);
                self.span(written);
            }
            Op::Select {
                selector,
                variants,
                default,
            } => {
                self.u8(3);
                match selector {
                    Selector::Variable { name, format } => {
                        self.u8(0);
                        self.u64(name);
                        self.format(format);
                    }
                    Selector::TermAttribute { term, attribute } => {
                        self.u8(1);
                        self.u64(term);
                        self.u64(attribute);
                    }
                    Selector::Text(s) => {
                        self.u8(2);
                        self.span(s);
                    }
                    Selector::Number(n) => {
                        self.u8(3);
                        self.f64(n);
                    }
                }
                self.span(variants);
                self.u32(default);
            }
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(self.u64()?))
    }
    /// A count of elements at least `size` bytes each, which the rest must be able to hold.
    fn count(&mut self, size: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n.checked_mul(size)? <= self.0.len()).then_some(n)
    }
    fn span(&mut self) -> Option<Span> {
        Some(Span {
            start: self.u32()?,
            len: self.u32()?,
        })
    }
    fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).ok()
    }
    fn format(&mut self) -> Option<NumberFormat> {
        let (min_fraction, max_fraction, flags) = (self.u8()?, self.u8()?, self.u8()?);
        Some(NumberFormat {
            min_fraction,
            max_fraction,
            grouping: flags & 1 != 0,
            ordinal: flags & 2 != 0,
        })
    }
    fn key(&mut self) -> Option<Key> {
        Some(match self.u8()? {
            0 => Key::Category(u8::try_from(self.u64()?).ok().filter(|&c| c < 6)?),
            1 => Key::Number(self.f64()?),
            2 => Key::Name(self.u64()?),
            _ => return None,
        })
    }
    fn term_value(&mut self) -> Option<TermValue> {
        Some(match self.u8()? {
            0 => TermValue::Text(self.span()?),
            1 => TermValue::Number(self.f64()?),
            _ => return None,
        })
    }
    fn op(&mut self) -> Option<Op> {
        Some(match self.u8()? {
            0 => Op::Text(self.span()?),
            1 => Op::Variable {
                name: self.u64()?,
                written: self.span()?,
                format: self.format()?,
            },
            2 => Op::Reference {
                target: self.u64()?,
                attribute: self.u64()?,
                term: self.u8()? != 0,
                arguments: self.span()?,
                written: self.span()?,
            },
            3 => {
                let selector = match self.u8()? {
                    0 => Selector::Variable {
                        name: self.u64()?,
                        format: self.format()?,
                    },
                    1 => Selector::TermAttribute {
                        term: self.u64()?,
                        attribute: self.u64()?,
                    },
                    2 => Selector::Text(self.span()?),
                    3 => Selector::Number(self.f64()?),
                    _ => return None,
                };
                Op::Select {
                    selector,
                    variants: self.span()?,
                    default: self.u32()?,
                }
            }
            _ => return None,
        })
    }
}
