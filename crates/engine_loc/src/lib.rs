//! Localization (ADR 0102): a game's text by key and language.
//!
//! - **Sources** are Fluent files, one per locale and module (`loc/ru/menu.ftl`):
//!   `play = Играть`, plurals and genders as select expressions, terms for grammatical cases.
//! - **The cook** compiles them ([`compile`]; a whole `loc/` folder: [`compile_sources`]) into
//!   [`Strings`] tables, one per locale, written into a pack ([`Strings::to_bytes`]); a check
//!   ([`check`]) compares the locales with the source and with the code's [`loc!`] uses.
//! - **The game** formats them with a [`Localizer`]: [`Localizer::format`] writes a message
//!   with its [`Args`] into the caller's `String`, allocating nothing once the string has
//!   grown; plural categories and numbers come from ICU4X by the message's locale; a message
//!   missing in the chosen locale falls back along its parents to the source locale.
//!
//! ```
//! use engine_loc::{Args, Localizer, compile, loc};
//!
//! let (en, _) = compile("en", &[("menu.ftl", "items = { $count ->\n    [one] { $count } item\n   *[other] { $count } items\n}\n")]);
//! let (ru, _) = compile("ru", &[("menu.ftl", "items = { $count ->\n    [one] { $count } предмет\n    [few] { $count } предмета\n   *[many] { $count } предметов\n}\n")]);
//! let mut loc = Localizer::new(vec![en, ru], "en").unwrap();
//! loc.set_locale(&["ru-RU"]);
//! let mut text = String::new();
//! loc.format(loc!("items"), &Args::new().with("count", 21), &mut text);
//! assert_eq!(text, "21 предмет");
//! ```

mod args;
mod compile;
mod id;
mod localizer;
mod project;
mod table;

pub use args::{Args, Value};
pub use compile::{Diagnostic, Pseudo, compile, compile_pseudo, compile_translation};
pub use id::{MessageId, hash};
pub use localizer::{LocError, Localizer};
pub use project::{
    Compiled, LocaleSources, Use, check, compile_sources, find_uses, read_locale, read_sources,
};
pub use table::{NumberFormat, Strings};

#[cfg(test)]
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
    fn a_localizer_can_be_shared_between_threads() {
        // A world's resource (the player keeps the game's localizer there).
        fn shared<T: Send + Sync>() {}
        shared::<Localizer>();
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
    fn text_selects_variants_named_like_categories() {
        let t = table(
            "en",
            "kind = { $k ->\n    [one] Single\n    [masculine] He\n   *[other] Many\n}\n",
        );
        let l = Localizer::new(vec![t], "en").unwrap();
        let kind = |k: Value<'_>| say(&l, loc!("kind"), &Args::new().with("k", k));
        assert_eq!(kind("one".into()), "Single");
        assert_eq!(kind("masculine".into()), "He");
        assert_eq!(kind("neuter".into()), "Many");
        assert_eq!(kind(1.into()), "Single");
    }

    #[test]
    fn a_check_sees_each_message_s_variables() {
        let en = table("en", EN);
        assert_eq!(en.variables("items"), ["count"]);
        assert_eq!(en.variables("place"), ["n"]);
        // A term's variables are its own arguments, not the message's.
        assert!(en.variables("about").is_empty());
        let t = table(
            "en",
            "a = { b } and { $x }\n    .hint = { $z }\nb = { NUMBER($y) } { $kind ->\n   *[one] One\n}\n",
        );
        assert_eq!(t.variables("a"), ["kind", "x", "y", "z"]);
        assert!(t.variables("nothing").is_empty());
    }

    #[test]
    fn a_translation_compiles_against_the_source() {
        let en = table(
            "en",
            "items = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\nhelp = See { items }\nmenu = Menu\n    .tooltip = Open\n",
        );
        // `$cnt` is a typo: the game passes `$count`. `help` references `items`, which only
        // the source has; `extra` is not in the source.
        let (ru, d) = compile_translation(
            "ru",
            &[(
                "ru/a.ftl",
                "\u{feff}items = { $cnt } предметов\nhelp = Смотри { items }\nextra = Лишнее\nmenu = Меню\n",
            )],
            &en,
        );
        let shown: Vec<String> = d.iter().map(ToString::to_string).collect();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(
            shown[0].starts_with("ru/a.ftl:1:1: error: `items` uses `$cnt`")
                && shown[0].contains("it passes `$count`"),
            "{shown:?}"
        );
        assert!(
            shown[1].starts_with("ru/a.ftl:3:1: warning: `extra` is not in the source"),
            "{shown:?}"
        );
        // The message in error is left out: the source's shows instead of `{$cnt}`.
        assert!(!ru.has_message("items") && ru.has_message("help"));
        let mut l = Localizer::new(vec![en, ru], "en").unwrap();
        l.set_locale(&["ru"]);
        assert_eq!(
            say(&l, loc!("help"), &Args::new().with("count", 3)),
            "Смотри 3 items"
        );
        // Attributes keep their names.
        let en = table("en", "menu = Menu\n    .tooltip = Open\n-brand = O\n");
        assert_eq!(
            en.entry_names().collect::<Vec<_>>(),
            ["menu", "menu.tooltip"]
        );
        assert_eq!(en.message_names().collect::<Vec<_>>(), ["menu"]);
        assert_eq!(en.messages(), 1);
    }

    #[test]
    fn only_the_wrong_messages_are_left_out() {
        let en = table(
            "en",
            "inner = { $y }\nouter = Uses { inner }\nplain = Plain\nbad = { $x }\nwrap = Wrap { bad }\n",
        );
        let (ru, d) = compile_translation(
            "ru",
            &[(
                "ru.ftl",
                // `plain` reaches `$y` through the source's `inner`; `bad` misspells `$x`;
                // `wrap` is right once `bad` is left out (it then resolves the source's).
                "plain = Просто { inner }\nbad = { $z }\nwrap = Обёртка { bad }\nouter = Через { inner }\n",
            )],
            &en,
        );
        let errors: Vec<String> = d
            .iter()
            .filter(|d| d.error)
            .map(|d| format!("{}:{}", d.line, d.message.split(',').next().unwrap()))
            .collect();
        assert_eq!(
            errors,
            ["1:`plain` uses `$y`", "2:`bad` uses `$z`"],
            "{d:?}"
        );
        assert!(ru.has_message("wrap") && ru.has_message("outer"));
        let mut l = Localizer::new(vec![en, ru], "en").unwrap();
        l.set_locale(&["ru"]);
        let args = Args::new().with("x", 1).with("y", 2);
        assert_eq!(say(&l, loc!("wrap"), &args), "Обёртка 1");
        assert_eq!(say(&l, loc!("plain"), &args), "Plain");
        assert_eq!(say(&l, loc!("outer"), &args), "Через 2");
    }

    #[test]
    fn uses_are_found_in_rust_and_luau() {
        let rust = "let a = loc!(\"play\");\n    // loc!(\"commented\")\nf(engine_loc::loc!( \"menu\" , \"tooltip\" ), myloc!(\"no\"));\n";
        let found: Vec<(String, Option<String>, u32, u32)> = find_uses("src/hud.rs", rust)
            .into_iter()
            .map(|u| (u.id, u.attribute, u.line, u.column))
            .collect();
        assert_eq!(
            found,
            [
                ("play".into(), None, 1, 9),
                ("menu".into(), Some("tooltip".into()), 3, 15)
            ]
        );
        let luau = "local t = loc(\"items\", 'tooltip')\n-- loc(\"commented\")\nprint(loc \"play\", Ui.loc('menu'), aloc(\"no\"))\n";
        let found: Vec<String> = find_uses("ui/hud.luau", luau)
            .into_iter()
            .map(|u| match u.attribute {
                Some(a) => format!("{}.{a}@{}:{}", u.id, u.line, u.column),
                None => format!("{}@{}:{}", u.id, u.line, u.column),
            })
            .collect();
        assert_eq!(found, ["items.tooltip@1:11", "play@3:7", "menu@3:22"]);
    }

    #[test]
    fn a_check_reports_missing_undefined_and_unused() {
        let locales = [
            LocaleSources {
                locale: "en".into(),
                files: vec![(
                    "loc/en/m.ftl".into(),
                    "play = Play\nquit = Quit\n    .tooltip = Leave\nabout = About { credits }\ncredits = Credits\nold = Old\n".into(),
                )],
            },
            LocaleSources {
                locale: "ru".into(),
                files: vec![("loc/ru/m.ftl".into(), "play = Играть\n".into())],
            },
        ];
        let compiled = compile_sources(&locales, "en", true);
        assert!(!compiled.has_errors(), "{:?}", compiled.diagnostics);
        let locales_built: Vec<&str> = compiled.tables.iter().map(|t| t.locale.as_str()).collect();
        assert_eq!(locales_built, ["en", "ru", "en-XA", "ar-XB"]);
        let uses = find_uses(
            "src/main.rs",
            "loc!(\"play\"); loc!(\"quit\"); loc!(\"about\"); loc!(\"quit\", \"hint\"); loc!(\"nope\");",
        );
        let found: Vec<String> = check(&compiled, Some(&uses))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            found,
            [
                "ru: warning: lacks 5 of the source's 6 messages and attributes (they show in `en`): `about`, `credits`, `old`, `quit`, `quit.tooltip`",
                "src/main.rs:1:44: error: `quit.hint` is not defined in the source locale `en`",
                "src/main.rs:1:66: error: `nope` is not defined in the source locale `en`",
                "en: warning: 1 of 5 messages are used neither by the code checked nor by another message: `old`",
            ]
        );
        // No source folder: an error, nothing compiled.
        let none = compile_sources(&locales[1..], "en", false);
        assert!(none.has_errors() && none.tables.is_empty());
    }

    #[test]
    fn a_folder_is_read_by_locale() {
        let dir = std::env::temp_dir().join(format!("engine_loc_folder_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (locale, file, text) in [
            ("en", "b.ftl", "b = B\n"),
            ("en", "a.ftl", "a = A\n"),
            ("ru", "a.ftl", "a = А\n"),
            ("ru", "notes.txt", "not Fluent"),
        ] {
            std::fs::create_dir_all(dir.join(locale)).unwrap();
            std::fs::write(dir.join(locale).join(file), text).unwrap();
        }
        // A folder without Fluent files is no locale.
        std::fs::create_dir_all(dir.join("art")).unwrap();
        let sources = read_sources(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let shown: Vec<(&str, Vec<&str>)> = sources
            .iter()
            .map(|l| {
                let names = l
                    .files
                    .iter()
                    .map(|(p, _)| {
                        std::path::Path::new(p)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap()
                    })
                    .collect();
                (l.locale.as_str(), names)
            })
            .collect();
        assert_eq!(
            shown,
            [("en", vec!["a.ftl", "b.ftl"]), ("ru", vec!["a.ftl"])]
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
        l.set_locale(&["fr-CA", "ru"]);
        let before = l.generation();
        l.replace(table("ru", "menu = Главное меню\n")).unwrap();
        assert!(l.generation() > before);
        assert_eq!(l.locale(), "ru");
        assert_eq!(say(&l, loc!("menu"), &Args::new()), "Главное меню");
        // Messages the new table lacks fall back to the source.
        assert_eq!(say(&l, loc!("about"), &Args::new()), "About Ofelia");
        // A table added for a language asked for first: chosen now.
        l.replace(table("fr", "menu = Menu principal\n")).unwrap();
        assert_eq!(l.locale(), "fr");
        // A table that cannot be used leaves the localizer as it was.
        let before = l.generation();
        assert!(l.replace(table("not a tag", "menu = ?\n")).is_err());
        assert_eq!((l.locale(), l.generation()), ("fr", before));
        assert_eq!(l.source(), "en");
    }
}
