//! Font stacks per language (ADR 0102): a game's fonts for each of its locales.

use crate::font::FontId;

/// A font stack for each language a game shows text in: a stack's first font draws what it
/// has, the others what it lacks, in order. A locale takes the stack of its longest tag set
/// (`zh-Hant-TW` → `zh-Hant` → `zh`), else the default one.
///
/// ```
/// use engine_text::{FontStacks, Fonts};
///
/// let mut fonts = Fonts::new();
/// let latin = fonts.load(engine_text::fonts::NOTO_SANS, 0).unwrap();
/// let arabic = fonts.load(engine_text::fonts::NOTO_SANS_ARABIC, 0).unwrap();
/// let mut stacks = FontStacks::new(vec![latin]);
/// // Arabic text, with Latin words and digits from Noto Sans.
/// stacks.set("ar", vec![arabic, latin]);
/// assert_eq!(stacks.for_locale("ar-EG"), [arabic, latin]);
/// assert_eq!(stacks.for_locale("ru"), [latin]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FontStacks {
    default: Vec<FontId>,
    languages: Vec<(String, Vec<FontId>)>,
}

impl FontStacks {
    /// Stacks whose default is `default`.
    pub fn new(default: Vec<FontId>) -> Self {
        Self {
            default,
            languages: Vec::new(),
        }
    }

    /// Sets the stack of `language`: a BCP 47 tag or its start (`ar`, `he`, `zh-Hant`,
    /// `ja`), replacing one set before.
    pub fn set(&mut self, language: &str, stack: Vec<FontId>) {
        match self
            .languages
            .iter_mut()
            .find(|(tag, _)| same_tag(tag, language))
        {
            Some((_, s)) => *s = stack,
            None => self.languages.push((language.to_owned(), stack)),
        }
    }

    /// The default stack.
    pub fn default_stack(&self) -> &[FontId] {
        &self.default
    }

    /// The stack for `locale` (a BCP 47 tag, `-` or `_` between its parts; case ignored).
    pub fn for_locale(&self, locale: &str) -> &[FontId] {
        self.languages
            .iter()
            .filter(|(tag, _)| starts_with_tag(locale, tag))
            .max_by_key(|(tag, _)| tag.len())
            .map_or(&self.default[..], |(_, stack)| &stack[..])
    }
}

/// Two tags alike, case and separators aside.
fn same_tag(a: &str, b: &str) -> bool {
    a.len() == b.len() && starts_with_tag(a, b)
}

/// Whether `locale` starts with the whole subtags of `tag`.
fn starts_with_tag(locale: &str, tag: &str) -> bool {
    let (l, t) = (locale.as_bytes(), tag.as_bytes());
    if t.is_empty() || l.len() < t.len() {
        return false;
    }
    let sep = |c: u8| c == b'-' || c == b'_';
    let alike = l[..t.len()]
        .iter()
        .zip(t)
        .all(|(&a, &b)| a.eq_ignore_ascii_case(&b) || (sep(a) && sep(b)));
    alike && (l.len() == t.len() || sep(l[t.len()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fonts;

    #[test]
    fn a_locale_takes_its_longest_tag() {
        let mut fonts = Fonts::new();
        let ids: Vec<FontId> = [
            crate::fonts::GO_REGULAR,
            crate::fonts::NOTO_SANS,
            crate::fonts::NOTO_SANS_ARABIC,
            crate::fonts::NOTO_SANS_HEBREW,
        ]
        .iter()
        .map(|&f| fonts.load(f, 0).unwrap_or_else(|e| panic!("{e}")))
        .collect();
        let (go, noto, arabic, hebrew) = (ids[0], ids[1], ids[2], ids[3]);
        let mut s = FontStacks::new(vec![noto]);
        s.set("ar", vec![arabic, noto]);
        s.set("he", vec![hebrew, noto]);
        s.set("zh", vec![go]);
        s.set("zh-Hant", vec![go, noto]);
        assert_eq!(s.for_locale("ar-EG"), [arabic, noto]);
        assert_eq!(s.for_locale("he_IL"), [hebrew, noto]);
        assert_eq!(s.for_locale("zh-Hant-TW"), [go, noto]);
        assert_eq!(s.for_locale("ZH-hant"), [go, noto]);
        assert_eq!(s.for_locale("zh-CN"), [go]);
        // A tag is matched by whole parts: `zhx` is not `zh`.
        assert_eq!(s.for_locale("zhx"), [noto]);
        assert_eq!(s.for_locale("fr-CA"), [noto]);
        // Setting a tag again replaces its stack.
        s.set("AR", vec![arabic]);
        assert_eq!(s.for_locale("ar"), [arabic]);
        assert_eq!(s.default_stack(), [noto]);
    }
}
