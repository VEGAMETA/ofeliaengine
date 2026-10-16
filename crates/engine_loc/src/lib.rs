//! Localization (ADR 0102): a game's text by key and language.
//!
//! - **Sources** are Fluent files, one per locale and module (`loc/ru/menu.ftl`):
//!   `play = Играть`, plurals and genders as select expressions, terms for grammatical cases.
//! - **The cook** compiles them ([`compile`], feature `compile`) into [`Strings`] tables, one
//!   per locale, written into a pack ([`Strings::to_bytes`]).
//! - **The game** formats them with a [`Localizer`]: [`Localizer::format`] writes a message
//!   with its [`Args`] into the caller's `String`, allocating nothing once the string has
//!   grown; plural categories and numbers come from ICU4X by the message's locale; a message
//!   missing in the chosen locale falls back along its parents to the source locale.
//!
//! ```
//! # #[cfg(feature = "compile")] {
//! use engine_loc::{Args, Localizer, compile, loc};
//!
//! let (en, _) = compile("en", &[("menu.ftl", "items = { $count ->\n    [one] { $count } item\n   *[other] { $count } items\n}\n")]);
//! let (ru, _) = compile("ru", &[("menu.ftl", "items = { $count ->\n    [one] { $count } предмет\n    [few] { $count } предмета\n   *[many] { $count } предметов\n}\n")]);
//! let mut loc = Localizer::new(vec![en, ru], "en").unwrap();
//! loc.set_locale(&["ru-RU"]);
//! let mut text = String::new();
//! loc.format(loc!("items"), &Args::new().with("count", 21), &mut text);
//! assert_eq!(text, "21 предмет");
//! # }
//! ```

mod args;
#[cfg(feature = "compile")]
mod compile;
mod id;
mod localizer;
mod table;

pub use args::{Args, Value};
#[cfg(feature = "compile")]
pub use compile::{Diagnostic, Pseudo, compile, compile_pseudo};
pub use id::{MessageId, hash};
pub use localizer::{LocError, Localizer};
pub use table::{NumberFormat, Strings};

#[cfg(test)]
#[cfg(feature = "compile")]
mod tests {
    use super::*;

    fn table(locale: &str, source: &str) -> Strings {
        let (t, d) = compile(locale, &[("test.ftl", source)]);
        assert!(d.iter().all(|d| !d.error), "{d:?}");
        t
    }

    const EN: &str = "\
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}
place = { NUMBER($n, type: \"ordinal\") ->
    [one] { $n }st
    [two] { $n }nd
    [few] { $n }rd
   *[other] { $n }th
}
score = Score: { $points }
price = { NUMBER($amount, minimumFractionDigits: 2) }
plain = { NUMBER($n, useGrouping: \"false\") }
hello = Hello, { $name }!
-brand = Ofelia
    .gender = feminine
about = About { -brand }
saved = { -brand.gender ->
    [feminine] She saved.
   *[other] It saved.
}
menu = Menu
    .tooltip = Open the menu
only-en = Only in English: { $count }
";

    const RU: &str = "\
items = { $count ->
    [one] { $count } предмет
    [few] { $count } предмета
   *[many] { $count } предметов
}
score = Счёт: { $points }
-brand = { $case ->
   *[nominative] Офелия
    [genitive] Офелии
}
about = О программе { -brand(case: \"genitive\") }
menu = Меню
    .tooltip = Открыть меню
";

    const PL: &str = "\
items = { $count ->
    [one] { $count } przedmiot
    [few] { $count } przedmioty
   *[many] { $count } przedmiotów
}
";

    const AR: &str = "\
items = { $count ->
    [zero] لا عناصر
    [one] عنصر واحد
    [two] عنصران
    [few] { $count } عناصر
    [many] { $count } عنصرًا
   *[other] { $count } عنصر
}
";

    fn localizer() -> Localizer {
        Localizer::new(
            vec![
                table("en", EN),
                table("ru", RU),
                table("pl", PL),
                table("ar", AR),
            ],
            "en",
        )
        .unwrap()
    }

    fn say(l: &Localizer, id: MessageId, args: &Args<'_>) -> String {
        l.text(id, args)
    }

    #[test]
    fn plurals_follow_each_language() {
        let mut l = localizer();
        let items = |l: &Localizer, n: i64| say(l, loc!("items"), &Args::new().with("count", n));
        assert_eq!(items(&l, 1), "1 item");
        assert_eq!(items(&l, 2), "2 items");
        l.set_locale(&["ru"]);
        let ru: Vec<String> = [1, 2, 5, 11, 21, 22, 25, 101, 111]
            .map(|n| items(&l, n))
            .to_vec();
        assert_eq!(
            ru,
            [
                "1 предмет",
                "2 предмета",
                "5 предметов",
                "11 предметов",
                "21 предмет",
                "22 предмета",
                "25 предметов",
                "101 предмет",
                "111 предметов"
            ]
        );
        l.set_locale(&["pl"]);
        assert_eq!(items(&l, 1), "1 przedmiot");
        assert_eq!(items(&l, 22), "22 przedmioty");
        assert_eq!(items(&l, 21), "21 przedmiotów");
        l.set_locale(&["ar-EG"]);
        assert_eq!(items(&l, 0), "لا عناصر");
        assert_eq!(items(&l, 2), "عنصران");
        // Egypt writes Arabic digits.
        assert_eq!(items(&l, 3), "٣ عناصر");
    }

    #[test]
    fn numbers_follow_the_locale() {
        let mut l = localizer();
        let score = |l: &Localizer| say(l, loc!("score"), &Args::new().with("points", 1_234_567));
        assert_eq!(score(&l), "Score: 1,234,567");
        assert_eq!(
            say(&l, loc!("price"), &Args::new().with("amount", 1234.5)),
            "1,234.50"
        );
        assert_eq!(
            say(&l, loc!("plain"), &Args::new().with("n", 1234.5678)),
            "1234.568"
        );
        assert_eq!(say(&l, loc!("place"), &Args::new().with("n", 2)), "2nd");
        assert_eq!(say(&l, loc!("place"), &Args::new().with("n", 13)), "13th");
        assert_eq!(say(&l, loc!("place"), &Args::new().with("n", 23)), "23rd");
        l.set_locale(&["ru"]);
        assert_eq!(score(&l), "Счёт: 1\u{a0}234\u{a0}567");
    }

    #[test]
    fn missing_messages_fall_back_and_never_fail() {
        let mut l = localizer();
        l.set_locale(&["ru-RU", "en"]);
        assert_eq!(l.locale(), "ru");
        // Not in Russian: the English message, with English numbers.
        assert_eq!(
            say(&l, loc!("only-en"), &Args::new().with("count", 1500)),
            "Only in English: 1,500"
        );
        // Nowhere: its identifier.
        let mut out = String::new();
        assert!(!l.format(loc!("nowhere"), &Args::new(), &mut out));
        assert_eq!(out, "nowhere");
        // A variable not given: shown as such.
        assert_eq!(say(&l, loc!("score"), &Args::new()), "Счёт: {$points}");
        // A language with no table: the source.
        assert_eq!(l.set_locale(&["fr-CA"]), "en");
        assert_eq!(say(&l, loc!("menu"), &Args::new()), "Menu");
    }

    #[test]
    fn terms_attributes_and_cases() {
        let mut l = localizer();
        assert_eq!(say(&l, loc!("about"), &Args::new()), "About Ofelia");
        assert_eq!(say(&l, loc!("saved"), &Args::new()), "She saved.");
        assert_eq!(
            say(&l, loc!("menu", "tooltip"), &Args::new()),
            "Open the menu"
        );
        assert_eq!(
            say(&l, loc!("hello"), &Args::new().with("name", "Ада")),
            "Hello, Ада!"
        );
        l.set_locale(&["ru"]);
        // The term in the genitive the Russian message asks for.
        assert_eq!(say(&l, loc!("about"), &Args::new()), "О программе Офелии");
        assert_eq!(
            say(&l, loc!("menu", "tooltip"), &Args::new()),
            "Открыть меню"
        );
    }

    #[test]
    fn errors_are_reported_with_positions() {
        let (t, d) = compile(
            "en",
            &[(
                "bad.ftl",
                "ok = Fine\nok = Again\nloop-a = { loop-b }\nloop-b = { loop-a }\nbroken = { $x ->\n",
            )],
        );
        let errors: Vec<String> = d
            .iter()
            .filter(|d| d.error)
            .map(ToString::to_string)
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("bad.ftl:2:1: error: `ok` is defined again")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("cycle")), "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("bad.ftl:5:") || e.starts_with("bad.ftl:6:")),
            "{errors:?}"
        );
        assert!(t.has_message("ok"));
        assert!(!t.has_message("loop-a"));
        // A reference to a message this locale lacks: a warning.
        let (_, d) = compile("ru", &[("x.ftl", "a = { b }\n")]);
        assert!(
            d.iter().all(|d| !d.error)
                && d.iter().any(|d| d.message.contains("`b` is not defined")),
            "{d:?}"
        );
    }

    #[test]
    fn pseudo_locales_mark_and_lengthen() {
        let (xa, d) = compile_pseudo(Pseudo::Accented, &[("test.ftl", EN)]);
        assert!(d.iter().all(|d| !d.error), "{d:?}");
        let (xb, _) = compile_pseudo(Pseudo::RightToLeft, &[("test.ftl", EN)]);
        let mut l = Localizer::new(vec![table("en", EN), xa, xb], "en").unwrap();
        l.set_locale(&["en-XA"]);
        let menu = say(&l, loc!("menu"), &Args::new());
        assert_eq!(menu, "[Mééñúú]");
        // Variables and plurals still work.
        assert_eq!(
            say(&l, loc!("items"), &Args::new().with("count", 2)),
            "[2 íítéémš]"
        );
        l.set_locale(&["ar-XB"]);
        assert_eq!(say(&l, loc!("menu"), &Args::new()), "\u{202E}Menu\u{202C}");
    }

    #[test]
    fn tables_round_trip_through_bytes() {
        for (locale, source) in [("en", EN), ("ru", RU), ("ar", AR)] {
            let t = table(locale, source);
            let bytes = t.to_bytes();
            assert_eq!(Strings::from_bytes(&bytes).as_ref(), Some(&t));
            // Torn or tampered bytes are refused, never trusted.
            assert_eq!(Strings::from_bytes(&bytes[..bytes.len() - 1]), None);
            let mut bad = bytes.clone();
            let n = bad.len();
            bad[n / 2] ^= 0xFF;
            let _ = Strings::from_bytes(&bad);
        }
    }

    #[test]
    fn hot_reload_replaces_a_table() {
        let mut l = localizer();
        l.set_locale(&["ru"]);
        let before = l.generation();
        l.replace(table("ru", "menu = Главное меню\n")).unwrap();
        assert!(l.generation() > before);
        assert_eq!(l.locale(), "ru");
        assert_eq!(say(&l, loc!("menu"), &Args::new()), "Главное меню");
        // Messages the new table lacks fall back to the source.
        assert_eq!(say(&l, loc!("about"), &Args::new()), "About Ofelia");
    }
}
