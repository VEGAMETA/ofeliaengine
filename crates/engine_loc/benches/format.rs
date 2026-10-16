//! CPU cost of localized text (ADR 0102): formatting messages into a string that has grown
//! (plain text, a plural with a grouped number, a fraction, a term with a case, a fallback),
//! compiling Fluent sources, reading a cooked table. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_loc --bench format
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_loc::{Args, Localizer, MessageId, Strings, compile, loc};
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Instant;

/// Median of `runs` timings of `f`, nanoseconds per one of its `per` iterations.
fn median(runs: usize, per: usize, mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e9 / per as f64
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

const EN: &str = "\
play = Play
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}
time = { NUMBER($seconds, minimumFractionDigits: 1, maximumFractionDigits: 1) } s
-brand = Ofelia
title = { -brand }: level { $level }
only-en = Ammo: { $ammo }
";

const RU: &str = "\
play = Играть
items = { $count ->
    [one] { $count } предмет
    [few] { $count } предмета
   *[many] { $count } предметов
}
time = { NUMBER($seconds, minimumFractionDigits: 1, maximumFractionDigits: 1) } с
-brand = { $case ->
   *[nominative] Офелия
    [genitive] Офелии
}
title = Уровень { $level } { -brand(case: \"genitive\") }
";

/// A source of `n` messages, plain and with plurals.
fn large(n: usize) -> String {
    let mut s = String::new();
    for k in 0..n {
        if k % 4 == 0 {
            let _ = writeln!(
                s,
                "m{k} = {{ $count ->\n    [one] {{ $count }} thing {k}\n   *[other] {{ $count }} things {k}\n}}"
            );
        } else {
            let _ = writeln!(s, "m{k} = Message number {k}, with some words in it.");
        }
    }
    s
}

fn main() {
    let (en, _) = compile("en", &[("hud.ftl", EN)]);
    let (ru, _) = compile("ru", &[("hud.ftl", RU)]);
    let mut loc = Localizer::new(vec![en, ru], "en").unwrap();
    loc.set_locale(&["ru-RU"]);
    let mut out = String::with_capacity(256);
    const N: usize = 100_000;
    let mut row = |name: &str, id: MessageId, args: Args<'_>| {
        let ns = median(9, N, || {
            for _ in 0..N {
                out.clear();
                loc.format(black_box(id), black_box(&args), &mut out);
            }
        });
        println!("| {name} | {ns:.1} ns | `{out}` |");
    };
    println!("| message (ru) | per format | text |");
    println!("|---|---:|---|");
    row("plain", loc!("play"), Args::new());
    row(
        "plural, grouped number",
        loc!("items"),
        Args::new().with("count", 1_234_567),
    );
    row(
        "fraction",
        loc!("time"),
        Args::new().with("seconds", 12.345),
    );
    row(
        "term in the genitive",
        loc!("title"),
        Args::new().with("level", 3),
    );
    row(
        "English fallback",
        loc!("only-en"),
        Args::new().with("ammo", 250),
    );

    let source = large(10_000);
    let start = Instant::now();
    let (big, d) = compile("en", &[("big.ftl", &source)]);
    let compile_ms = start.elapsed().as_secs_f64() * 1e3;
    assert!(d.iter().all(|d| !d.error));
    let bytes = big.to_bytes();
    let read_ms = median(9, 1, || {
        black_box(Strings::from_bytes(black_box(&bytes)).unwrap());
    }) / 1e6;
    println!();
    println!(
        "10,000 messages ({} KB of source): compiled in {compile_ms:.1} ms, {} KB cooked, read in {read_ms:.2} ms",
        source.len() / 1024,
        bytes.len() / 1024
    );
}
