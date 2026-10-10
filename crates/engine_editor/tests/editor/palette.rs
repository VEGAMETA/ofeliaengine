//! Command palette ranking, highlights, recency, chords, conflicts and keymap text.

#![allow(clippy::unwrap_used)]

use engine_editor::{Chord, Commands, KeyContext, Keymap};

fn editor_commands() -> Commands {
    let mut c = Commands::new();
    for (name, cat, title) in [
        ("scene.duplicate", "Scene", "Duplicate Selection"),
        ("scene.delete", "Scene", "Delete Selection"),
        ("scene.select_all", "Scene", "Select All"),
        ("file.save", "File", "Save Scene"),
        ("file.save_as", "File", "Save Scene As"),
        ("view.toggle_grid", "View", "Toggle Grid"),
        ("go.definition", "Go", "Go To Definition"),
        ("debug.gpu_capture", "Debug", "Capture GPU Frame"),
        ("shaders.compile_all", "Shaders", "Compile All Shaders"),
    ] {
        c.register(name, cat, title);
    }
    c
}

fn top(c: &mut Commands, q: &str) -> String {
    let mut out = Vec::new();
    c.search(q, &mut out);
    out.first()
        .map_or(String::new(), |m| c.info(m.id).name.clone())
}

#[test]
fn ranking_prefers_word_starts_and_titles() {
    let mut c = editor_commands();
    assert_eq!(top(&mut c, "gtd"), "go.definition");
    assert_eq!(top(&mut c, "dup sel"), "scene.duplicate");
    assert_eq!(
        top(&mut c, "save"),
        "file.save",
        "shorter label wins the tie"
    );
    assert_eq!(top(&mut c, "gpu"), "debug.gpu_capture");
    assert_eq!(top(&mut c, "compile"), "shaders.compile_all");
    assert_eq!(top(&mut c, "grid"), "view.toggle_grid");
    let mut out = Vec::new();
    c.search("zzz", &mut out);
    assert!(out.is_empty());
    c.search("SCENE", &mut out);
    assert!(out.len() >= 5, "case-insensitive, category words match");
}

#[test]
fn highlights_mark_the_matched_characters() {
    let mut c = editor_commands();
    let mut out = Vec::new();
    c.search("gtd", &mut out);
    let label: Vec<char> = c.info(out[0].id).label().chars().collect();
    let marked: String = c
        .highlights(&out[0])
        .iter()
        .map(|&i| label[i as usize])
        .collect();
    assert_eq!(marked, "GTD");
}

#[test]
fn recently_run_commands_rise() {
    let mut c = editor_commands();
    let save_as = c.find("file.save_as").unwrap();
    assert_eq!(top(&mut c, "save"), "file.save");
    c.ran(save_as);
    assert_eq!(top(&mut c, "save"), "file.save_as");
    let mut out = Vec::new();
    c.search("", &mut out);
    assert_eq!(out[0].id, save_as, "empty query lists recent first");
    assert_eq!(out.len(), c.len());
}

#[test]
fn chords_parse_and_print() {
    let ch = Chord::parse("ctrl+shift+p").unwrap();
    assert!(ch.ctrl && ch.shift && !ch.alt);
    assert_eq!(ch.to_string(), "Ctrl+Shift+P");
    assert_eq!(Chord::parse("Alt+F4").unwrap().to_string(), "Alt+F4");
    assert_eq!(Chord::parse("Ctrl+1").unwrap().to_string(), "Ctrl+1");
    assert_eq!(Chord::parse("Delete").unwrap().to_string(), "Delete");
    assert!(Chord::parse("Ctrl+").is_none());
    assert!(Chord::parse("Ctrl+A+B").is_none());
    assert!(Chord::parse("Hyper+A").is_none());
}

#[test]
fn contexts_conflicts_and_text() {
    let c = editor_commands();
    let dup = c.find("scene.duplicate").unwrap();
    let del = c.find("scene.delete").unwrap();
    let save = c.find("file.save").unwrap();
    let ctrl_d = Chord::parse("Ctrl+D").unwrap();
    let mut k = Keymap::new();
    k.bind(ctrl_d, KeyContext::Viewport, dup).unwrap();
    k.bind(Chord::parse("Ctrl+S").unwrap(), KeyContext::Global, save)
        .unwrap();
    let conflict = k.bind(ctrl_d, KeyContext::Viewport, del).unwrap_err();
    assert_eq!(conflict.existing, dup);
    // The same chord in another context is not a conflict; the specific context wins.
    k.bind(ctrl_d, KeyContext::Global, del).unwrap();
    assert_eq!(k.lookup(ctrl_d, &[KeyContext::Viewport]), Some(dup));
    assert_eq!(k.lookup(ctrl_d, &[KeyContext::TextField]), Some(del));
    assert!(k.conflicts().is_empty());
    assert_eq!(
        k.chords_of(save).map(|c| c.to_string()).collect::<Vec<_>>(),
        ["Ctrl+S"]
    );

    let text = k.to_text(&c);
    assert_eq!(
        text,
        "file.save = Ctrl+S\nscene.delete = Ctrl+D\nscene.duplicate = Ctrl+D @viewport\n"
    );
    let edited = format!("{text}scene.select_all = Ctrl+S\nnope = Ctrl+Q\nfile.save = Ctrl+\n");
    let (k2, errors) = Keymap::from_text(&edited, &c);
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].0, 5);
    let conflicts = k2.conflicts();
    assert_eq!(
        conflicts.len(),
        1,
        "hand-edited conflicts are kept and reported"
    );
    assert_eq!(conflicts[0].chord.to_string(), "Ctrl+S");
}

#[test]
fn ten_thousand_commands_search_quickly() {
    let mut c = Commands::new();
    let words = [
        "Scene", "Asset", "Physics", "Render", "Audio", "Script", "View", "Debug",
    ];
    let verbs = [
        "Toggle", "Open", "Create", "Delete", "Rebuild", "Reload", "Show", "Reset",
    ];
    let nouns = [
        "Grid", "Material", "Collider", "Shadow", "Bus", "Module", "Camera", "Overlay",
    ];
    for i in 0..10_000 {
        let (w, v, n) = (words[i % 8], verbs[(i / 8) % 8], nouns[(i / 64) % 8]);
        c.register(&format!("cmd.{i}"), w, &format!("{v} {n} {i}"));
    }
    let mut out = Vec::new();
    c.search("tg", &mut out); // warm up the buffers
    let start = std::time::Instant::now();
    for q in ["tg", "rel mod", "phys col", "show over 9"] {
        c.search(q, &mut out);
        assert!(!out.is_empty(), "{q}");
    }
    let per_query = start.elapsed().as_secs_f64() * 1e3 / 4.0;
    println!("10k commands: {per_query:.2} ms per query");
    // Generous bound for unoptimized test builds on shared machines; the bench target is
    // well under 1 ms in release.
    assert!(per_query < 50.0, "{per_query} ms");
}
