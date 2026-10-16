//! The game's text on the player (ADR 0102): a `loc/` folder compiled at start in the
//! player's language, the localizer a resource of the world, an edit showing in the running
//! game, a broken edit keeping the text and saying where.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use engine_app::{Args, Localizer, TextSource, loc};
use std::path::Path;

fn write(dir: &Path, locale: &str, text: &str) {
    std::fs::create_dir_all(dir.join(locale)).unwrap();
    std::fs::write(dir.join(locale).join("hud.ftl"), text).unwrap();
}

#[test]
fn a_game_s_text_follows_its_folder() {
    let dir = std::env::temp_dir().join(format!("engine_app_text_game_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        "en",
        "lives = { $n ->\n    [one] { $n } life\n   *[other] { $n } lives\n}\n",
    );
    write(
        &dir,
        "ru",
        "lives = { $n ->\n    [one] { $n } жизнь\n    [few] { $n } жизни\n   *[many] { $n } жизней\n}\n",
    );
    let source = TextSource::Folder {
        dir: dir.clone(),
        source: String::from("en"),
    };
    let Some((mut player, mut game)) = common::player_with(|s| {
        s.text = Some(source);
        s.languages = Some(vec![String::from("ru-RU")]);
    }) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    println!("start-up: {:?}", player.startup().notes);
    let lives = |l: &Localizer| l.text(loc!("lives"), &Args::new().with("n", 3));
    // The world's resource, as the game's hooks see it.
    let l = player.world_mut().resource::<Localizer>().unwrap();
    assert_eq!(l.locale(), "ru");
    assert_eq!(lives(l), "3 жизни");
    // An edit, while frames run.
    write(
        &dir,
        "ru",
        "lives = { $n ->\n    [one] { $n } попытка\n    [few] { $n } попытки\n   *[many] { $n } попыток\n}\n",
    );
    let mut frames = 0;
    while lives(player.localizer().unwrap()) != "3 попытки" && frames < 600 {
        player.frame(&mut game).unwrap();
        frames += 1;
    }
    println!("the edit showed after {frames} frames");
    assert_eq!(lives(player.localizer().unwrap()), "3 попытки");
    // A broken edit: the text stays, the error says where.
    write(&dir, "ru", "lives = { $n ->\n");
    frames = 0;
    while !player.text_diagnostics().iter().any(|d| d.error) && frames < 600 {
        player.frame(&mut game).unwrap();
        frames += 1;
    }
    let errors: Vec<String> = player
        .text_diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect();
    println!("{errors:?}");
    assert!(errors.iter().any(|e| e.contains("hud.ftl")), "{errors:?}");
    assert_eq!(lives(player.localizer().unwrap()), "3 попытки");
    // The game switches language.
    player.localizer_mut().unwrap().set_locale(&["en"]);
    assert_eq!(lives(player.localizer().unwrap()), "3 lives");
    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}
