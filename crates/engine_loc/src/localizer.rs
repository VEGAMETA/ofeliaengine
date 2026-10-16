//! The runtime (ADR 0102): the loaded locales, the chain a message is looked up along, and
//! formatting into the caller's buffer.

use crate::args::{Args, Value};
use crate::id::MessageId;
use crate::table::{CATEGORIES, Key, NumberFormat, Op, Selector, Span, Strings, TermValue};
use core::fmt::Write as _;
use icu_decimal::DecimalFormatter;
use icu_decimal::input::Decimal;
use icu_decimal::options::{DecimalFormatterOptions, GroupingStrategy};
use icu_locale::fallback::{LocaleFallbackConfig, LocaleFallbacker};
use icu_locale::subtags::Language;
use icu_locale::{DataLocale, Locale};
use icu_plurals::{PluralCategory, PluralRules};

/// What can go wrong setting up a [`Localizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocError {
    /// A table's locale is not a BCP 47 tag.
    BadLocale(String),
    /// No table has the source locale.
    NoSource(String),
    /// Two tables have the same locale.
    Duplicate(String),
    /// ICU4X has no data for a locale's plural rules or numbers.
    NoData(String),
}

impl core::fmt::Display for LocError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadLocale(l) => write!(f, "`{l}` is not a locale tag"),
            Self::NoSource(l) => write!(f, "no table for the source locale `{l}`"),
            Self::Duplicate(l) => write!(f, "two tables for `{l}`"),
            Self::NoData(l) => write!(f, "no plural or number data for `{l}`"),
        }
    }
}

impl std::error::Error for LocError {}

/// A locale's rules: plural categories and number formatting.
struct Rules {
    cardinal: PluralRules,
    ordinal: PluralRules,
    grouped: DecimalFormatter,
    plain: DecimalFormatter,
}

impl Rules {
    fn new(locale: &Locale) -> Option<Self> {
        Some(Self {
            cardinal: PluralRules::try_new_cardinal(locale.into()).ok()?,
            ordinal: PluralRules::try_new_ordinal(locale.into()).ok()?,
            grouped: DecimalFormatter::try_new(locale.into(), DecimalFormatterOptions::default())
                .ok()?,
            plain: DecimalFormatter::try_new(
                locale.into(),
                DecimalFormatterOptions::from(GroupingStrategy::Never),
            )
            .ok()?,
        })
    }
}

/// Where references resolve their variables: the message's arguments, or a term's own.
#[derive(Clone, Copy)]
enum Scope<'s, 'a> {
    Message(&'s Args<'a>),
    Term { table: usize, arguments: Span },
}

/// References nest at most this deep (the compiler rejects cycles; this bounds the rest).
const MAX_DEPTH: u32 = 16;

/// The loaded locales and the current chain (see the crate's documentation).
pub struct Localizer {
    tables: Vec<Strings>,
    /// Each table's locale, normalized as the fallback chain yields it.
    locales: Vec<DataLocale>,
    rules: Vec<Rules>,
    /// Each table's language.
    languages: Vec<Language>,
    /// The rules of the locale asked for, where it is more specific than its table's (Egypt's
    /// Arabic digits from an `ar` table): used for the tables of its language.
    requested: Option<(Language, Rules)>,
    source: usize,
    /// The tables a message is looked up in, in order: the chosen locale, its parents that
    /// have tables, the source locale.
    chain: Vec<usize>,
    /// The languages last asked for ([`Self::set_locale`]), negotiated again when the tables
    /// change.
    preferred: Vec<String>,
    generation: u64,
}

impl Localizer {
    /// The tables, the one of `source` (the locale whose messages are complete) the last
    /// fallback; the source locale chosen.
    ///
    /// # Errors
    ///
    /// When a table's locale does not parse, two tables share one, none has `source`, or
    /// ICU4X has no rules for one.
    pub fn new(tables: Vec<Strings>, source: &str) -> Result<Self, LocError> {
        let fallbacker = LocaleFallbacker::new();
        let normalize = |tag: &str| -> Result<(Locale, DataLocale), LocError> {
            let locale =
                Locale::try_from_str(tag).map_err(|_| LocError::BadLocale(tag.to_owned()))?;
            let normalized = *fallbacker
                .for_config(LocaleFallbackConfig::default())
                .fallback_for(DataLocale::from(&locale))
                .get();
            Ok((locale, normalized))
        };
        let mut locales = Vec::with_capacity(tables.len());
        let mut rules = Vec::with_capacity(tables.len());
        let mut languages = Vec::with_capacity(tables.len());
        for t in &tables {
            let (locale, normalized) = normalize(&t.locale)?;
            if locales.contains(&normalized) {
                return Err(LocError::Duplicate(t.locale.clone()));
            }
            rules.push(Rules::new(&locale).ok_or_else(|| LocError::NoData(t.locale.clone()))?);
            languages.push(locale.id.language);
            locales.push(normalized);
        }
        let (_, source_locale) = normalize(source)?;
        let source = locales
            .iter()
            .position(|l| *l == source_locale)
            .ok_or_else(|| LocError::NoSource(source.to_owned()))?;
        Ok(Self {
            tables,
            locales,
            rules,
            languages,
            requested: None,
            source,
            chain: vec![source],
            preferred: Vec::new(),
            generation: 0,
        })
    }

    /// The first of `preferred` (BCP 47 tags, the player's languages in order) that a table
    /// or one of its parents serves (`es-MX` → `es-419` → `es`; `zh-TW` → `zh-Hant`, never
    /// `zh`), else the source locale; returns the chosen table's locale. Text should be
    /// formatted again ([`Self::generation`] changes).
    pub fn set_locale(&mut self, preferred: &[&str]) -> &str {
        let fallbacker = LocaleFallbacker::new();
        let mut chain = Vec::new();
        let mut asked = None;
        'preferred: for tag in preferred {
            let Ok(locale) = Locale::try_from_str(tag) else {
                continue;
            };
            let mut it = fallbacker
                .for_config(LocaleFallbackConfig::default())
                .fallback_for(DataLocale::from(&locale));
            loop {
                if let Some(k) = self.locales.iter().position(|l| l == it.get())
                    && !chain.contains(&k)
                {
                    chain.push(k);
                }
                if it.get().is_unknown() {
                    break;
                }
                it.step();
            }
            if !chain.is_empty() {
                asked = Some(locale);
                break 'preferred;
            }
        }
        if !chain.contains(&self.source) {
            chain.push(self.source);
        }
        // The locale asked for, when more specific than its table's: its own rules.
        self.requested = asked
            .filter(|l| DataLocale::from(l) != self.locales[chain[0]])
            .and_then(|l| Rules::new(&l).map(|r| (l.id.language, r)));
        self.chain = chain;
        self.preferred = preferred.iter().map(|&p| p.to_owned()).collect();
        self.generation += 1;
        &self.tables[self.chain[0]].locale
    }

    /// The source locale: the one whose messages are complete, the last fallback.
    pub fn source(&self) -> &str {
        &self.tables[self.source].locale
    }

    /// The table of `locale`, if loaded.
    pub fn table(&self, locale: &str) -> Option<&Strings> {
        self.tables.iter().find(|t| t.locale == locale)
    }

    /// The chosen locale.
    pub fn locale(&self) -> &str {
        &self.tables[self.chain[0]].locale
    }

    /// The locales loaded.
    pub fn locales(&self) -> impl Iterator<Item = &str> + '_ {
        self.tables.iter().map(|t| t.locale.as_str())
    }

    /// Changes whenever text may format differently: the locale switched, a table replaced.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Replaces the table of `table`'s locale (a hot reload), or adds it; the locale is
    /// negotiated again from the languages last asked for.
    ///
    /// # Errors
    ///
    /// As [`Self::new`] for its locale; the localizer is then unchanged.
    pub fn replace(&mut self, table: Strings) -> Result<(), LocError> {
        let mut tables = self.tables.clone();
        match tables.iter().position(|t| t.locale == table.locale) {
            Some(k) => tables[k] = table,
            None => tables.push(table),
        }
        self.replace_all(tables)
    }

    /// Replaces every table (a hot reload of a whole folder), the source locale kept; the
    /// locale is negotiated again from the languages last asked for.
    ///
    /// # Errors
    ///
    /// As [`Self::new`]; the localizer is then unchanged.
    pub fn replace_all(&mut self, tables: Vec<Strings>) -> Result<(), LocError> {
        let mut next = Self::new(tables, self.source())?;
        let preferred: Vec<&str> = self.preferred.iter().map(String::as_str).collect();
        if !preferred.is_empty() {
            next.set_locale(&preferred);
        }
        next.generation = self.generation + 1;
        *self = next;
        Ok(())
    }

    /// Writes the message `id` with `args` into `out` (appending), from the first table of
    /// the chain that has it. Returns whether one did; a missing message writes its
    /// identifier. Allocates nothing once `out` has grown (numbers of more than eight
    /// significant digits excepted: ICU4X keeps eight inline).
    pub fn format(&self, id: MessageId, args: &Args<'_>, out: &mut String) -> bool {
        for &t in &self.chain {
            if let Some(e) = self.tables[t].find(false, id.hash, id.attribute) {
                let pattern = e.pattern;
                self.pattern(t, pattern, Scope::Message(args), 0, out);
                return true;
            }
        }
        out.push_str(id.name);
        false
    }

    /// [`Self::format`] into a new string (not for every frame).
    pub fn text(&self, id: MessageId, args: &Args<'_>) -> String {
        let mut out = String::new();
        self.format(id, args, &mut out);
        out
    }

    fn pattern(&self, t: usize, pattern: Span, scope: Scope<'_, '_>, depth: u32, out: &mut String) {
        let table = &self.tables[t];
        for op in &table.ops[pattern.range()] {
            match *op {
                Op::Text(s) => out.push_str(table.text(s)),
                Op::Variable {
                    name,
                    written,
                    format,
                } => match self.variable(scope, name) {
                    Some(value) => self.value(t, value, format, out),
                    None => {
                        out.push_str("{$");
                        out.push_str(table.text(written));
                        out.push('}');
                    }
                },
                Op::Reference {
                    target,
                    attribute,
                    term,
                    arguments,
                    written,
                } => {
                    let found = (depth < MAX_DEPTH)
                        .then(|| self.resolve(t, term, target, attribute))
                        .flatten();
                    match found {
                        Some((rt, p)) => {
                            let inner = if term {
                                Scope::Term {
                                    table: t,
                                    arguments,
                                }
                            } else {
                                scope
                            };
                            self.pattern(rt, p, inner, depth + 1, out);
                        }
                        None => {
                            out.push('{');
                            out.push_str(table.text(written));
                            out.push('}');
                        }
                    }
                }
                Op::Select {
                    selector,
                    variants,
                    default,
                } => {
                    let chosen = self.select(t, selector, variants, scope).unwrap_or(default);
                    let v = table.variants[variants.range()][chosen as usize];
                    self.pattern(t, v.pattern, scope, depth + 1, out);
                }
            }
        }
    }

    /// A reference's pattern: in the referring table first (terms and messages of the same
    /// locale), then along the chain.
    fn resolve(&self, t: usize, term: bool, target: u64, attribute: u64) -> Option<(usize, Span)> {
        core::iter::once(t)
            .chain(self.chain.iter().copied())
            .find_map(|k| {
                self.tables[k]
                    .find(term, target, attribute)
                    .map(|e| (k, e.pattern))
            })
    }

    /// A variable: the message's argument, or a term's own literal argument.
    fn variable<'x>(&'x self, scope: Scope<'x, 'x>, name: u64) -> Option<Value<'x>> {
        match scope {
            Scope::Message(args) => args.get(name),
            Scope::Term { table, arguments } => {
                let t = &self.tables[table];
                t.arguments[arguments.range()]
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| match a.value {
                        TermValue::Number(n) => Value::Float(n),
                        TermValue::Text(s) => Value::Str(t.text(s)),
                    })
            }
        }
    }

    fn value(&self, t: usize, value: Value<'_>, format: NumberFormat, out: &mut String) {
        match value {
            Value::Str(s) => out.push_str(s),
            Value::Int(_) | Value::Float(_) => {
                let d = decimal(value, format);
                let rules = self.rules_for(t);
                let f = if format.grouping {
                    &rules.grouped
                } else {
                    &rules.plain
                };
                let _ = write!(out, "{}", f.format(&d));
            }
        }
    }

    /// The variant `selector` chooses among `variants`: an exact number, then the plural
    /// category, for numbers; the key, for text (a category's name too: `"one"` is `[one]`).
    fn select(
        &self,
        t: usize,
        selector: Selector,
        variants: Span,
        scope: Scope<'_, '_>,
    ) -> Option<u32> {
        let table = &self.tables[t];
        let list = &table.variants[variants.range()];
        let by_text = |s: &str| {
            let h = crate::id::hash(s.as_bytes());
            list.iter().position(|v| match v.key {
                Key::Name(k) => k == h,
                Key::Category(c) => CATEGORIES[usize::from(c)] == s,
                Key::Number(_) => false,
            })
        };
        let by_number = |value: Value<'_>, format: NumberFormat| {
            let d = decimal(value, format);
            let x = match value {
                Value::Int(i) => i as f64,
                Value::Float(f) => f,
                Value::Str(_) => f64::NAN,
            };
            list.iter()
                .position(|v| matches!(v.key, Key::Number(n) if n == x))
                .or_else(|| {
                    let rules = self.rules_for(t);
                    let c = if format.ordinal {
                        rules.ordinal.category_for(&d)
                    } else {
                        rules.cardinal.category_for(&d)
                    };
                    list.iter()
                        .position(|v| v.key == Key::Category(category(c)))
                })
        };
        let chosen = match selector {
            Selector::Variable { name, format, .. } => match self.variable(scope, name)? {
                Value::Str(s) => by_text(s),
                number => by_number(number, format),
            },
            Selector::TermAttribute { term, attribute } => {
                let (rt, p) = self.resolve(t, true, term, attribute)?;
                // A term attribute selects by its text (`.gender = feminine`).
                let rtable = &self.tables[rt];
                let text = match rtable.ops.get(p.range()) {
                    Some([Op::Text(s)]) => rtable.text(*s),
                    _ => return None,
                };
                by_text(text)
            }
            Selector::Text(s) => by_text(table.text(s)),
            Selector::Number(n) => by_number(Value::Float(n), NumberFormat::default()),
        };
        chosen.map(|k| k as u32)
    }
}

impl Localizer {
    /// The rules numbers of table `t` are formatted and selected by: the locale asked for,
    /// for a table of its language; the table's own otherwise.
    fn rules_for(&self, t: usize) -> &Rules {
        match &self.requested {
            Some((language, rules)) if *language == self.languages[t] => rules,
            _ => &self.rules[t],
        }
    }
}

/// A plural category as the tables store it.
fn category(c: PluralCategory) -> u8 {
    match c {
        PluralCategory::Zero => 0,
        PluralCategory::One => 1,
        PluralCategory::Two => 2,
        PluralCategory::Few => 3,
        PluralCategory::Many => 4,
        PluralCategory::Other => 5,
    }
}

/// `value` as a decimal with `format`'s fraction digits: rounded to the maximum, trailing
/// zeros dropped, padded to the minimum. Built from an integer scaled by a power of ten (no
/// float parsing, nothing allocated for up to eight significant digits).
fn decimal(value: Value<'_>, format: NumberFormat) -> Decimal {
    match value {
        Value::Int(i) => {
            let mut d = Decimal::from(i);
            d.absolute.pad_end(-i16::from(format.min_fraction));
            d
        }
        Value::Float(f) if f.is_finite() => {
            let digits = format.max_fraction.min(9);
            let scaled = (f * 10f64.powi(i32::from(digits))).round();
            let clamped = scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
            let mut d = Decimal::from(clamped);
            d.multiply_pow10(-i16::from(digits));
            d.absolute.trim_end();
            d.absolute
                .pad_end(-i16::from(format.min_fraction.min(digits)));
            d
        }
        _ => Decimal::from(0i64),
    }
}
