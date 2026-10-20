//! A game's `main` (ADR 0107 §1): the player around the game, or with the game's `editor`
//! feature the editor around it.

use crate::{Game, Player, PlayerSettings};

/// What makes a game from the player once its scene is loaded ([`PlayerSettings::scene`]):
/// called by the player at launch, and by the editor at every Play (ADR 0107).
pub type Start<G> = fn(&mut Player) -> Result<G, Box<dyn std::error::Error>>;

/// Runs a game in the player until its window closes: the player made from `settings`, its
/// scene loaded, then the game made by `start`. What [`main!`](crate::main) runs without the
/// game's `editor` feature.
///
/// # Errors
///
/// When the player cannot start, `start` fails, or a frame fails.
pub fn run<G: Game>(
    settings: PlayerSettings,
    start: Start<G>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut player = Player::new(&settings)?;
    let mut game = start(&mut player)?;
    player.run(&mut game)?;
    Ok(())
}

/// A game's `main` (ADR 0107 §1): `engine_app::main!(settings, start);`, where `settings`
/// gives the player's settings and `start` makes the game from the player once its scene is
/// loaded ([`Start`]).
///
/// - **With the game's `editor` feature** (`editor = ["dep:engine_editor"]` beside an
///   optional `engine_editor` dependency: `ofeliaengine new` writes both, and
///   `ofeliaengine edit` runs the game with them), `main` runs the editor around the game.
/// - **Without it,** the player ([`run`]). A shipped game is built without the feature and
///   does not link the editor.
///
/// The macro expands in the game's crate, so the feature is the game's own.
///
/// ```ignore
/// fn settings() -> Result<engine_app::PlayerSettings, Box<dyn std::error::Error>> {
///     Ok(engine_app::PlayerSettings {
///         scene: Some("scenes/main.scene".into()),
///         ..engine_app::PlayerSettings::default()
///     })
/// }
/// fn start(_player: &mut engine_app::Player) -> Result<MyGame, Box<dyn std::error::Error>> {
///     Ok(MyGame)
/// }
/// engine_app::main!(settings, start);
/// ```
#[macro_export]
macro_rules! main {
    ($settings:path, $start:path $(,)?) => {
        // The game's own `editor` feature, if its manifest declares one.
        #[allow(unexpected_cfgs)]
        fn main() -> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error>> {
            let settings = $settings()?;
            // The editor builds the game's code while it runs: this crate's.
            #[cfg(feature = "editor")]
            {
                ::engine_editor::run_game(
                    settings,
                    $start,
                    ::engine_editor::GameCrate::new(env!("CARGO_MANIFEST_DIR")),
                )
            }
            #[cfg(not(feature = "editor"))]
            {
                $crate::run(settings, $start)
            }
        }
    };
}
