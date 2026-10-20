//! The editor around a game (ADR 0107): the player runs the game's world and draws its
//! scene into the viewport panel; the editor's shell (the team's Godot layout) is drawn over
//! the frame. Linked into the game's own build, so the game's components are native here.
//!
//! ```no_run
//! use engine_app::{Game, Player, PlayerSettings};
//! use engine_ecs::World;
//!
//! struct MyGame;
//! impl Game for MyGame {
//!     fn tick(&mut self, _world: &mut World, _dt: f32) {}
//! }
//! fn start(_player: &mut Player) -> Result<MyGame, Box<dyn std::error::Error>> {
//!     Ok(MyGame)
//! }
//! engine_editor::run(PlayerSettings::default(), start).unwrap();
//! ```
//!
//! - **Edit mode** (all there is until Play, ADR 0107 phase E3): the game is not made and
//!   nothing ticks; the scene's world is drawn from the editor's camera.
//! - **The viewport:** middle drag orbits, Shift + middle drag pans, right drag looks
//!   around, the wheel zooms (`docs/research/ux/viewport-navigation.md`).
//! - **The scene tree** shows the scene's entities (those with a `SceneId`) in their
//!   hierarchy and order, each with its kind's icon. A click selects, Ctrl+click adds or
//!   takes out, Shift+click selects a range; the last picked is the active one, which the
//!   inspector shows. After a click in it, the arrows move the selection (Shift extends
//!   it), Left and Right close and open rows. F2 or a double click renames in place, the
//!   filter box narrows it to matching names. Its `+` button adds an entity (a model, a
//!   light, a camera) where the camera looks, numbered when a sibling has its name.
//!   Delete and Ctrl+D act on the whole selection.
//! - **The inspector** edits the selected entities' components ([`crate::inspector`]):
//!   with several selected, the components they all have, a value that differs between
//!   them shown "—"; a typed value goes to all of them, a drag or a relative expression
//!   (`+=1`) moves each from its own value, as one undo step.
//!   numbers are dragged sideways ([`crate::Snapping`]: Shift inverts the default, Ctrl
//!   rounds to the grid) or clicked and typed (expressions, [`crate::expr`]); checkboxes
//!   and enums are clicked; strings are typed. Its filter box narrows it to matching
//!   fields. A changed field has a reset button (↺); over a field, Backspace resets it,
//!   and Ctrl+C and Ctrl+V copy and paste its value as scene text.
//! - **The command palette** (Ctrl+Shift+P) finds and runs every command, the add menu's
//!   kinds among them ("Add Cube"); after `@` (Ctrl+P) it finds the scene's objects by
//!   name and selects the one picked.
//! - **Every edit is undoable** ([`History`]). Commands and their shortcuts: Ctrl+Z undo,
//!   Ctrl+Y or Ctrl+Shift+Z redo, Ctrl+S save, Delete, Ctrl+D duplicate, Ctrl+A add, Y
//!   snapping by default, F2 rename.

use crate::build::{
    self as build, BUILD_VAR, BuildEvent, Builds, Built, GameCrate, Problem, RESTART_VAR, in_slot,
};
use crate::camera::OrbitCamera;
use crate::icons;
use crate::input::{Button, InputCollector};
use crate::inspector::{self, Defaults, Inspector, Kind, Property};
use crate::paint::{UiText, paint};
use crate::session::{self, Base, Document, EditorState, LastRun, Lock, SessionFile, View, Writer};
use crate::shell::{
    Field, FieldAction, FieldEvent, MenuItem, Palette, PaletteAction, PaletteRow, PaletteView,
    Section, ShellModel, TreeItem, Widget, build, dismissed,
};
use crate::snap::{Quantity, Snapping};
use crate::{Chord, CommandId, Commands, Fuzzy, History, KeyContext, Keymap, Match};
use engine_app::{
    AppError, Game, Model, Overlay, OverlayOutput, OverlayPass, Player, PlayerSettings, Setup,
    Window, builtin,
};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{ComponentId, Entity, GlobalTransform, QueryState, Transform, World};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind};
use engine_reflect::TypeKind;
use engine_render2d::{Canvas, GlyphTexture, QuadMode, Renderer2d, View2d};
use engine_runtime::{Camera, PointLight, SpotLight, Sun, look_rotation};
use engine_scene::{AssetId, AssetPaths, Name, RootOrder, SaveOptions, SceneId, SceneRegistry};
use engine_ui::{Theme, Ui, UiKey};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

type Error = Box<dyn std::error::Error>;

/// How the game is made when Play starts (ADR 0107, phase E3): given the player with the
/// scene loaded.
pub type Start<G> = fn(&mut Player) -> Result<G, Error>;

/// What the scene dock's add menu offers, in order.
const ADD: [&str; 9] = [
    "Empty",
    "Cube",
    "Sphere",
    "Plane",
    "Cylinder",
    "Sun",
    "Point Light",
    "Spot Light",
    "Camera",
];

/// The add menu's kinds' icons, in its order: the same in the menu, the tree and the
/// palette.
const ADD_ICONS: [&str; 9] = [
    icons::NODE_3D,
    icons::CUBE,
    icons::SPHERE,
    icons::PLANE,
    icons::CYLINDER,
    icons::SUN,
    icons::LIGHT,
    icons::SPOT_LIGHT,
    icons::CAMERA,
];

/// Where the scene tree's keys apply: after a click in it.
const TREE_KEYS: KeyContext = KeyContext::Panel(1);

/// What a part shows while the selected entities hold different values there.
const MIXED: &str = "—";

/// How long a message stays in the status bar, in seconds.
const MESSAGE_SECONDS: f32 = 4.0;

/// How many results the command palette shows at once (the list scrolls with the
/// highlight).
const PALETTE_ROWS: usize = 12;

/// Runs the editor around a game until its window closes. `settings` are the game's; its
/// scene (`PlayerSettings::scene`) is what the editor opens. A full-screen window becomes
/// a window of 1600 × 900.
///
/// # Errors
///
/// When the player cannot start or a frame fails.
pub fn run<G: Game>(settings: PlayerSettings, start: Start<G>) -> Result<(), Error> {
    run_frames(settings, start, None).map(|_| ())
}

/// Runs the editor around a game whose code it builds while it runs (ADR 0107 §1, E1-5c):
/// what `engine_app::main!` calls with the game's `editor` feature, `game` being the
/// game's crate.
///
/// - A change to the game's code (or to the engine's) is built in the background, at a
///   lower priority; the status bar shows the build and its errors ("Build: Show
///   Errors…").
/// - The new build restarts the editor: by itself while its window is in the background,
///   offered (Ctrl+R) while it is in front ("Editor: Restart Policy…"); the session comes
///   back as it was.
/// - It builds while it runs a slot's copy of the game (`.ofelia/run/`), as
///   `ofeliaengine edit` starts it: under `cargo run`, cargo's own file cannot be written
///   over while it runs.
///
/// Its messages go to stderr and `%LOCALAPPDATA%/ofeliaengine/logs/<game>-editor.log`.
///
/// # Errors
///
/// As [`run`].
pub fn run_game<G: Game>(
    settings: PlayerSettings,
    start: Start<G>,
    game: GameCrate,
) -> Result<(), Error> {
    let name = std::env::current_exe()
        .ok()
        .and_then(|e| e.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| String::from("game"));
    let _log = engine_log::init(&engine_log::LogConfig {
        file: Some(engine_log::report::log_file(&format!("{name}-editor"))),
        ..engine_log::LogConfig::default()
    });
    // Started as cargo's own file (`cargo run`): a copy in a slot runs instead, so builds
    // can write that file, and this process ends before it opens a window.
    if let Ok(exe) = std::env::current_exe()
        && !in_slot(&game, &exe)
    {
        let files = [exe.clone(), exe.with_extension("pdb")];
        match build::copy_to_slot(&game.run_dir(), &exe, &files, &exe) {
            Ok(copy) => {
                let mut command = std::process::Command::new(&copy);
                command
                    .args(std::env::args_os().skip(1))
                    .env(BUILD_VAR, &exe);
                if let Ok(dir) = std::env::current_dir() {
                    command.current_dir(dir);
                }
                command.spawn()?;
                return Ok(());
            }
            Err(e) => log::warn!(target: "engine_editor", "code changes are not built: {e}"),
        }
    }
    run_inner(settings, start, None, Some(game)).map(|_| ())
}

/// [`run`] for `frames` frames at most (smoke runs): how many ran.
///
/// # Errors
///
/// As [`run`].
pub fn run_frames<G: Game>(
    settings: PlayerSettings,
    start: Start<G>,
    frames: Option<u64>,
) -> Result<u64, Error> {
    run_inner(settings, start, frames, None)
}

fn run_inner<G: Game>(
    mut settings: PlayerSettings,
    start: Start<G>,
    frames: Option<u64>,
    game_crate: Option<GameCrate>,
) -> Result<u64, Error> {
    // The editor keeps Windows' power throttling (the team's decision, CLAUDE.md §5): it
    // saves power while its window is in the background.
    settings.process_setup = false;
    if !matches!(
        settings.window,
        Window::Windowed { .. } | Window::Offscreen { .. }
    ) {
        settings.window = Window::Windowed {
            width: 1600,
            height: 900,
        };
    }
    // A restart's new editor opens hidden where the last one's window was, and shows
    // its first frame over it (E1-5c-2); any start opens where the session's window was.
    let restarted = Restarted::from_env();
    let root = settings
        .project
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    if let Some(placement) = session::peek_window(&root.join(session::DIR)) {
        settings.window_placement = Some(placement);
    }
    settings.start_hidden = restarted.is_some();
    let mut player = Player::new(&settings)?;
    let (mut game, mut overlay) = editor(&mut player, &settings, start)?;
    // The scene's unsaved changes from the last run, back (ADR 0107 §4).
    game.begin_session(&mut player)?;
    let run_dir = game_crate.as_ref().map(GameCrate::run_dir);
    if let Some(game_crate) = game_crate {
        game.begin_builds(game_crate);
    }
    let mut count = 0;
    let mut result: Result<(), Error> = Ok(());
    loop {
        while !player.closing() && !game.quitting() && frames.is_none_or(|n| count < n) {
            if let Err(e) = player.frame_overlaid(&mut game, Some(&mut overlay)) {
                result = Err(e.into());
                break;
            }
            player.set_view_rect(game.view_rect());
            game.sync_assets(&mut player);
            count += 1;
            if count % 30 == 1 {
                game.note_window(player.window_placement());
            }
            // Its second frame drawn: shown, and the last editor told it can go.
            if count == 2
                && let Some(r) = &restarted
            {
                player.show_window(r.front);
                if let (Some(from), Some(run_dir)) = (r.from, &run_dir) {
                    let _ = std::fs::write(run_dir.join(format!("up-{from}")), b"");
                }
            }
        }
        // Nothing is asked at the exit: unsaved changes stay in the session.
        game.note_window(player.window_placement());
        game.end_session(player.world_mut());
        let (Some((exe, artifact)), Some(run_dir)) = (game.restart_with(), &run_dir) else {
            break;
        };
        // A restart: this window stays until the new build shows its first frame.
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        match hand_over(&exe, &args, artifact.as_deref(), game.focused(), run_dir) {
            Ok(took) => {
                log::info!(
                    target: "engine_editor",
                    "handed over to the new build in {:.2} s",
                    took.as_secs_f32()
                );
                break;
            }
            Err(why) => {
                // The new build failed at its start: this editor goes on.
                game.resume_session(&mut player)?;
                game.restart_failed(why);
            }
        }
    }
    overlay.destroy(&mut player)?;
    result.map(|()| count)
}

/// How long a restart's new editor may take to show its first frame.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(20);

/// What `ENGINE_EDITOR_RESTART` tells an editor a restart started: when (nanoseconds since
/// 1970), the editor it takes over from, and whether that one's window was in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Restarted {
    at: u128,
    from: Option<u32>,
    front: bool,
}

impl Restarted {
    fn from_env() -> Option<Self> {
        Self::parse(&std::env::var(RESTART_VAR).ok()?)
    }

    /// `<nanoseconds> <process id> <1: in front>`.
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let at = parts.next()?.parse().ok()?;
        let from = parts.next().and_then(|p| p.parse().ok());
        let front = parts.next() == Some("1");
        Some(Self { at, from, front })
    }
}

/// Starts the new build and waits until it shows its first frame (it writes `up-<this
/// process's id>` into `.ofelia/run/`): this editor's window stays on the screen until
/// then. Fails when the new one exits first or takes longer than `HANDOVER_TIMEOUT`
/// (ended then): how long it took, or why it failed.
fn hand_over(
    exe: &Path,
    args: &[std::ffi::OsString],
    artifact: Option<&Path>,
    front: bool,
    run_dir: &Path,
) -> Result<Duration, String> {
    let started = Instant::now();
    let up = run_dir.join(format!("up-{}", std::process::id()));
    let _ = std::fs::remove_file(&up);
    let mut child =
        spawn_restart(exe, args, artifact, front).map_err(|e| format!("{}: {e}", exe.display()))?;
    if front {
        // It brings its window to the front: this process is in the foreground.
        engine_platform::window::allow_foreground(child.id());
    }
    let timer = engine_platform::time::HighResTimer::new().map_err(|e| e.to_string())?;
    loop {
        if up.exists() {
            let took = started.elapsed();
            // Taken; its writer may still have it open for a moment.
            for _ in 0..40 {
                if std::fs::remove_file(&up).is_ok() || !up.exists() {
                    break;
                }
                let _ = timer.sleep(Duration::from_millis(5));
            }
            return Ok(took);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the new build stopped at its start ({status})"));
        }
        if started.elapsed() > HANDOVER_TIMEOUT {
            let _ = child.kill();
            return Err(format!(
                "the new build showed nothing in {} s",
                HANDOVER_TIMEOUT.as_secs()
            ));
        }
        let _ = timer.sleep(Duration::from_millis(5));
    }
}

/// Starts the editor again from a new build's slot copy, in this process's folder with its
/// arguments: `ENGINE_EDITOR_RESTART` says when, which editor it takes over from and
/// whether its window was in front; `ENGINE_EDITOR_BUILD` which build.
fn spawn_restart(
    exe: &Path,
    args: &[std::ffi::OsString],
    artifact: Option<&Path>,
    front: bool,
) -> std::io::Result<std::process::Child> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut command = std::process::Command::new(exe);
    command.args(args).env(
        RESTART_VAR,
        format!("{now} {} {}", std::process::id(), u8::from(front)),
    );
    match artifact {
        Some(a) => command.env(BUILD_VAR, a),
        None => command.env_remove(BUILD_VAR),
    };
    if let Ok(dir) = std::env::current_dir() {
        command.current_dir(dir);
    }
    command.spawn()
}

/// The editor's two halves for a player: the game it wraps (window events, the frame's
/// update) and the overlay that paints its shell. Give both to
/// `Player::frame_overlaid`, then `player.set_view_rect(game.view_rect())`.
///
/// # Errors
///
/// When the fonts or the overlay's GPU objects cannot be made.
pub fn editor<G: Game>(
    player: &mut Player,
    settings: &PlayerSettings,
    start: Start<G>,
) -> Result<(EditorGame<G>, EditorOverlay), Error> {
    let size = match settings.window {
        Window::Windowed { width, height } | Window::Offscreen { width, height } => [width, height],
        _ => [1600, 900],
    };
    let root = player.assets().root().to_path_buf();
    // The scene the player loaded, and its own id from its header: saved back the same.
    // Without one, a new random id (`AssetId::default()` is the empty id).
    let scene_path = settings.scene.clone();
    #[allow(clippy::unwrap_or_default)]
    let scene_id = scene_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(root.join(p)).ok())
        .and_then(|text| engine_scene::scene_header(&text).ok())
        .and_then(|(_, id)| id)
        .unwrap_or_else(AssetId::new);
    let editor = Editor::new(
        size,
        root,
        scene_path,
        scene_id,
        player.assets().paths().clone(),
    )?;
    // The panels are drawn in the frame's format at its white level: SDR, or HDR at
    // Windows' SDR white (ADR 0038); `output_changed` follows the display.
    let output = player.output();
    let setup = player.setup();
    let mut renderer = Renderer2d::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        output.format,
        QuadMode::for_device(setup.device),
    )?;
    renderer.set_white(output.white);
    let glyphs = GlyphTexture::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        &editor.text.atlas,
    )?;
    let shared = Rc::new(RefCell::new(editor));
    Ok((
        EditorGame {
            shared: Rc::clone(&shared),
            game: None,
            start,
        },
        EditorOverlay {
            shared,
            renderer,
            glyphs,
            canvas: Canvas::new(),
        },
    ))
}

/// A row of the scene tree.
#[derive(Debug, Clone, Copy)]
struct Row {
    entity: Entity,
    depth: u16,
    children: bool,
}

/// The editor's commands.
#[derive(Debug, Clone, Copy)]
struct CommandIds {
    undo: CommandId,
    redo: CommandId,
    save: CommandId,
    delete: CommandId,
    duplicate: CommandId,
    add: CommandId,
    snap: CommandId,
    rename: CommandId,
    palette: CommandId,
    find: CommandId,
    select_next: CommandId,
    select_previous: CommandId,
    extend_next: CommandId,
    extend_previous: CommandId,
    collapse: CommandId,
    expand: CommandId,
    expand_all: CommandId,
    /// After the scene's file changed on disk since its unsaved changes: take the file's
    /// version, or write the unsaved one over it.
    take_disk: CommandId,
    save_over_disk: CommandId,
    /// The scene's autosaves in the palette, to bring one back.
    restore_autosave: CommandId,
    /// The new build, now (Ctrl+R).
    restart: CommandId,
    /// The last build's errors in the palette, to open one in VS Code.
    build_errors: CommandId,
    build_now: CommandId,
    /// What a new build does: by itself in the background, always, or ask.
    restart_policy: CommandId,
    /// "Add Cube" and the rest: the add menu's kinds, in its order.
    add_kinds: [CommandId; ADD.len()],
}

/// The editor's commands, their default shortcuts, and the shortcut the palette shows
/// beside each command (its first).
fn commands() -> (Commands, Keymap, CommandIds, HashMap<CommandId, String>) {
    let mut c = Commands::new();
    let ids = CommandIds {
        undo: c.register("edit.undo", "Edit", "Undo"),
        redo: c.register("edit.redo", "Edit", "Redo"),
        save: c.register("file.save", "File", "Save Scene"),
        delete: c.register("scene.delete", "Scene", "Delete Selection"),
        duplicate: c.register("scene.duplicate", "Scene", "Duplicate Selection"),
        add: c.register("scene.add", "Scene", "Add Entity"),
        snap: c.register("edit.snap", "Edit", "Snap by Default"),
        rename: c.register("scene.rename", "Scene", "Rename Selection"),
        palette: c.register("editor.palette", "Editor", "Command Palette"),
        find: c.register("scene.find", "Scene", "Go to Object"),
        select_next: c.register("scene.select_next", "Scene", "Select Next Row"),
        select_previous: c.register("scene.select_previous", "Scene", "Select Previous Row"),
        extend_next: c.register("scene.extend_next", "Scene", "Extend Selection Down"),
        extend_previous: c.register("scene.extend_previous", "Scene", "Extend Selection Up"),
        collapse: c.register("scene.collapse", "Scene", "Close Row"),
        expand: c.register("scene.expand", "Scene", "Open Row"),
        expand_all: c.register("scene.expand_all", "Scene", "Open Branch"),
        take_disk: c.register("scene.take_disk_version", "Scene", "Take Disk Version"),
        save_over_disk: c.register(
            "scene.save_over_disk_version",
            "Scene",
            "Save Over Disk Version",
        ),
        restore_autosave: c.register(
            "scene.restore_autosave",
            "Scene",
            "Restore Unsaved Changes…",
        ),
        restart: c.register("editor.restart", "Editor", "Restart with the New Build"),
        build_errors: c.register("build.errors", "Build", "Show Errors…"),
        build_now: c.register("build.now", "Build", "Build the Game's Code"),
        restart_policy: c.register("editor.restart_policy", "Editor", "Restart Policy…"),
        add_kinds: ADD.map(|kind| {
            let name = format!("scene.add.{}", kind.to_lowercase().replace(' ', "_"));
            c.register(&name, "Scene", &format!("Add {kind}"))
        }),
    };
    let mut keymap = Keymap::new();
    let mut shortcuts = HashMap::new();
    let tree = [
        ("ArrowDown", ids.select_next),
        ("ArrowUp", ids.select_previous),
        ("Shift+ArrowDown", ids.extend_next),
        ("Shift+ArrowUp", ids.extend_previous),
        ("ArrowLeft", ids.collapse),
        ("ArrowRight", ids.expand),
        ("Shift+ArrowRight", ids.expand_all),
    ]
    .map(|(chord, id)| (chord, id, TREE_KEYS));
    let global = [
        ("Ctrl+Z", ids.undo),
        ("Ctrl+Y", ids.redo),
        ("Ctrl+Shift+Z", ids.redo),
        ("Ctrl+S", ids.save),
        ("Delete", ids.delete),
        ("Ctrl+D", ids.duplicate),
        ("Ctrl+A", ids.add),
        ("Y", ids.snap),
        ("F2", ids.rename),
        ("Ctrl+Shift+P", ids.palette),
        ("Ctrl+P", ids.find),
        ("Ctrl+R", ids.restart),
    ]
    .map(|(chord, id)| (chord, id, KeyContext::Global));
    for (chord, id, context) in global.into_iter().chain(tree) {
        if let Some(chord) = Chord::parse(chord) {
            let _ = keymap.bind(chord, context, id);
            shortcuts.entry(id).or_insert_with(|| chord.to_string());
        }
    }
    (c, keymap, ids, shortcuts)
}

/// Entity references read from text: by scene id, through an index of the world.
struct IndexRefs<'a> {
    index: &'a HashMap<SceneId, Entity>,
}

impl engine_scene::text::ReadRefs for IndexRefs<'_> {
    fn entity(&mut self, label: &str) -> Result<Entity, String> {
        let id = SceneId::parse(label).ok_or_else(|| format!("`@{label}` is not a scene id"))?;
        self.index
            .get(&id)
            .copied()
            .ok_or_else(|| format!("no entity has the scene id {id}"))
    }
}

/// A number being dragged in the inspector.
#[derive(Debug, Clone)]
struct Drag {
    property: Property,
    part: usize,
    /// Each dragged entity and its numbers when the drag started (degrees for angles):
    /// each moves from its own.
    starts: Vec<(Entity, [f64; 4])>,
    /// The drag's undo step: one per gesture.
    merge: u64,
    /// The step's name (`Edit Transform.translation`).
    name: String,
    /// Escape took the drag back: the rest of the gesture changes nothing.
    cancelled: bool,
    /// The pointer moved: a press and release without moving types the value instead.
    moved: bool,
}

/// An object the command palette found.
#[derive(Debug, Clone, Copy)]
struct Found {
    entity: Entity,
    score: i32,
    /// Its place in tree order: equal scores keep it.
    order: u32,
    /// Its matched characters: a range of the palette's `highlights`.
    matched: (u32, u32),
}

/// A list the palette offers in place of commands and objects: the autosaves
/// "Restore Unsaved Changes…" can bring back.
#[derive(Debug, Default)]
struct Choices {
    /// What the box says while nothing is typed.
    hint: &'static str,
    items: Vec<Choice>,
    /// The items matching the query, in their order: an item and its matched letters (a
    /// range of the palette's `highlights`).
    shown: Vec<(usize, (u32, u32))>,
}

/// One item of a palette's list.
#[derive(Debug)]
struct Choice {
    label: String,
    detail: String,
    pick: Pick,
}

/// What picking a palette list's item does.
#[derive(Debug, Clone)]
enum Pick {
    /// Brings back this autosave.
    Autosave(PathBuf),
    /// Opens the last build's error with this index in VS Code.
    Problem(usize),
    Policy(RestartPolicy),
}

/// What a new build does (team decision, 2026-10-20): by itself while the editor's window
/// is in the background (the developer is in the code editor), offered while it is in
/// front; or always by itself, or always asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RestartPolicy {
    #[default]
    Background,
    Always,
    Ask,
}

impl RestartPolicy {
    const ALL: [Self; 3] = [Self::Background, Self::Always, Self::Ask];

    /// Its name in the session file.
    fn name(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Always => "always",
            Self::Ask => "ask",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Background => "Restart by itself while the editor is in the background",
            Self::Always => "Always restart by itself",
            Self::Ask => "Always ask before restarting",
        }
    }
}

/// Where the game's code stands, for the status bar.
#[derive(Debug, Clone, Default)]
enum BuildState {
    #[default]
    Idle,
    Building(Instant),
    /// What the status bar says about it.
    Failed(String),
}

/// What the editor says when it does not build the game's code.
const NOT_BUILDING: &str = "Code changes are built while the editor runs from `ofeliaengine edit`";

/// The command palette while it is open: one box for every command and, after `@`, the
/// scene's objects (`docs/research/ux/command-palette.md`); or a list to pick from.
#[derive(Debug, Default)]
struct CommandPalette {
    /// What is typed.
    query: String,
    /// The highlighted result.
    selected: usize,
    /// The first result shown: the list scrolls to keep the highlight in view.
    first: usize,
    /// The commands matching the query, best first.
    commands: Vec<Match>,
    /// After `@`: the objects matching the query, best first.
    objects: Vec<Found>,
    highlights: Vec<u16>,
    fuzzy: Fuzzy,
    /// The scene's entities in tree order, and a stack for walking it (kept: searching
    /// allocates nothing once they have grown).
    order: Vec<Entity>,
    stack: Vec<Entity>,
    /// A list to pick from instead.
    choices: Option<Choices>,
}

impl CommandPalette {
    /// Whether the query looks for the scene's objects (`@`) rather than commands.
    fn finds_objects(&self) -> bool {
        self.choices.is_none() && self.query.trim_start().starts_with('@')
    }

    /// How many results there are.
    fn len(&self) -> usize {
        if let Some(choices) = &self.choices {
            choices.shown.len()
        } else if self.finds_objects() {
            self.objects.len()
        } else {
            self.commands.len()
        }
    }

    /// Highlights result `i`, scrolling the list to show it.
    fn select(&mut self, i: usize) {
        self.selected = i;
        if i < self.first {
            self.first = i;
        } else if i >= self.first + PALETTE_ROWS {
            self.first = i + 1 - PALETTE_ROWS;
        }
    }
}

/// The scene's entities (those with a scene id) in tree order, under closed rows too:
/// roots in spawn order (the scene file's), each followed by its children.
fn tree_order(
    world: &World,
    ids: &mut QueryState<(Entity, &'static SceneId)>,
    stack: &mut Vec<Entity>,
    out: &mut Vec<Entity>,
) {
    out.clear();
    stack.clear();
    stack.extend(ids.iter_readonly(world).map(|(e, _)| e).filter(|&e| {
        world
            .parent(e)
            .is_none_or(|p| world.get::<SceneId>(p).is_none())
    }));
    // The first root on top.
    stack.sort_unstable_by_key(|&e| core::cmp::Reverse(root_key(world, e)));
    while let Some(e) = stack.pop() {
        out.push(e);
        let at = stack.len();
        stack.extend(
            world
                .children(e)
                .filter(|&c| world.get::<SceneId>(c).is_some()),
        );
        stack[at..].reverse();
    }
}

/// What the editor keeps between frames, shared by its game and its overlay.
struct Editor {
    ui: Ui,
    text: UiText,
    input: InputCollector,
    palette: Palette,
    /// UI scale (DPI / 96).
    scale: f32,
    /// The output's size in pixels.
    size: [u32; 2],
    orbit: OrbitCamera,
    /// The editor's camera entity (runtime only: no scene id, never saved).
    camera: Option<Entity>,
    camera_drag: bool,
    /// The selected entities in the order they were picked: the last is the active one,
    /// which the inspector shows and F2 renames.
    selection: Vec<Entity>,
    /// Where a Shift+click or Shift+arrow range starts.
    anchor: Option<Entity>,
    /// The scene tree takes the arrows (a click went to it last).
    tree_focus: bool,
    /// The row to scroll into view in the next frame.
    reveal: Option<Entity>,
    /// The inspector's header: the active entity, and how many are selected.
    selection_label: String,
    collapsed: HashSet<Entity>,
    /// The viewport panel in pixels, for the next frame's scene.
    view_rect: Option<[u32; 4]>,
    /// The project's folder: the scene's path starts there.
    root: PathBuf,
    /// The scene's file, relative to `root`; none until an untitled scene is saved.
    scene_path: Option<PathBuf>,
    /// The scene's own id, written in its header.
    scene_id: AssetId,
    /// The history's head at the last save (or the start): changes are unsaved while it
    /// differs (`u64::MAX` after unsaved changes were restored).
    saved_head: u64,
    /// The session in the project's `.ofelia/` (ADR 0107 §4): none until
    /// `begin_session`, so editors that tests and tools make write nothing there.
    session: Option<Session>,
    /// The game's crate when the editor runs inside the game's build (`run_game`).
    game_crate: Option<GameCrate>,
    /// Builds of the game's code (E1-5c): none until `begin_builds`, nor while the
    /// editor runs cargo's own file.
    builds: Option<Builds>,
    /// The last build's errors, for "Build: Show Errors…".
    problems: Vec<Problem>,
    build_state: BuildState,
    /// A new build waiting for a restart: its slot's executable, and cargo's path of it.
    ready: Option<(PathBuf, Option<PathBuf>)>,
    /// The editor restarts with this build once its frame loop ends.
    restart_with: Option<(PathBuf, Option<PathBuf>)>,
    /// The window has the keyboard: a new build is offered, not taken.
    focused: bool,
    /// A restart started this editor then (nanoseconds since 1970): told at its first
    /// frame.
    restarted_at: Option<u128>,
    /// Where the window is (`note_window`), kept in the session for the next start.
    window: Option<engine_platform::window::WindowPlacement>,
    /// The session's state when it ended for a restart, to go on with it when the new
    /// build fails at its start: its base, autosave, what the autosave holds, the conflict.
    ended: Option<(Option<Base>, PathBuf, Option<u64>, bool)>,
    restart_policy: RestartPolicy,
    /// The scene tab: the file's name, then `*` with unsaved changes.
    scene_label: String,
    /// Where the player's assets are, for showing and saving references by path.
    assets: AssetPaths,
    /// Asset paths of a scene the editor loaded, for the player (`sync_assets`).
    pending_assets: AssetPaths,
    history: History,
    commands: Commands,
    keymap: Keymap,
    ids: CommandIds,
    /// The shortcut shown beside each command in the palette.
    shortcuts: HashMap<CommandId, String>,
    /// The command palette, while open.
    command_palette: Option<CommandPalette>,
    snapping: Snapping,
    inspector: Inspector,
    /// Each component type's default value (changed fields, reset).
    defaults: Defaults,
    drag: Option<Drag>,
    /// The number being typed (a click without a drag): its component, offset and part.
    typing: Option<(ComponentId, usize, u8)>,
    /// The entity being renamed in the tree.
    renaming: Option<Entity>,
    /// The last click on a tree row, for double clicks.
    last_click: Option<(Entity, Instant)>,
    /// What the tree and the inspector are filtered by.
    tree_filter: String,
    inspector_filter: String,
    /// The inspector's shown properties (indices into its properties) and the shown
    /// sections' ranges in them: what a field event's section and field point at.
    shown: Vec<usize>,
    shown_sections: Vec<(usize, usize)>,
    /// The enum whose list is open: its component and offset.
    open_choice: Option<(ComponentId, usize)>,
    /// The add menu is open.
    add_open: bool,
    /// Drag gestures so far (each its own undo step).
    gestures: u64,
    tools: [String; 4],
    active_tool: usize,
    status: String,
    /// The last command's message for the status bar, and when it came.
    message: Option<(String, Instant)>,
    rows: Vec<Row>,
    scene_ids: Option<QueryState<(Entity, &'static SceneId)>>,
    frames: u64,
    since: Instant,
    fps: f64,
}

/// The session while the editor runs (ADR 0107 §4): the project's lock, the writer, and
/// where the scene's unsaved changes are.
struct Session {
    /// `<project>/.ofelia`.
    dir: PathBuf,
    lock: Option<Lock>,
    writer: Writer,
    /// What the scene's file was when it was loaded or last saved.
    base: Option<Base>,
    /// The scene's autosave, `.ofelia/autosave/<stem>-<id>.scene`, its older ones beside
    /// it (`session::older`).
    autosave: PathBuf,
    /// The history's head the scene's autosave holds; none while it has none.
    autosaved: Option<u64>,
    /// When the changes not in an autosave are autosaved.
    due: Option<Instant>,
    /// The last key, button, pointer or wheel input.
    last_input: Instant,
    /// The scene's file changed on disk since its unsaved changes were made from it:
    /// neither is written over until a command picks one.
    conflict: bool,
    /// Until when the lock says `restoring`.
    restoring_until: Option<Instant>,
    /// The session file's text as last written: an unchanged one is not written again.
    last_text: String,
    /// When the view is written next if it changed: at most every
    /// `session::VIEW_EVERY`, at a quiet moment.
    view_due: Instant,
}

/// How long ago a time in nanoseconds since 1970 was, for messages.
fn ago(nanos: u64) -> String {
    let then = std::time::UNIX_EPOCH + Duration::from_nanos(nanos);
    let seconds = then.elapsed().map_or(0, |d| d.as_secs());
    match seconds {
        0..60 => String::from("just now"),
        60..3600 => format!("{} min ago", seconds / 60),
        _ => format!("{} h ago", seconds / 3600),
    }
}

/// An icon and a kind for an entity: its main component's.
fn kind_of(world: &World, e: Entity) -> (&'static str, &'static str) {
    if world.get::<Camera>(e).is_some() {
        (icons::CAMERA, "Camera")
    } else if world.get::<Sun>(e).is_some() {
        (icons::SUN, "Sun")
    } else if world.get::<SpotLight>(e).is_some() {
        (icons::SPOT_LIGHT, "Light")
    } else if world.get::<PointLight>(e).is_some() {
        (icons::LIGHT, "Light")
    } else if let Some(model) = world.get::<Model>(e) {
        (model_icon(model), "Model")
    } else {
        (icons::NODE_3D, "Entity")
    }
}

/// Where a root comes in the tree: by its order, those without one last in spawn order.
fn root_key(world: &World, e: Entity) -> (u32, u32) {
    (
        world.get::<RootOrder>(e).map_or(u32::MAX, |o| o.0),
        e.index(),
    )
}

/// A model's icon: its built-in shape's, or a model's.
fn model_icon(model: &Model) -> &'static str {
    match model.mesh {
        m if m == builtin::CUBE => icons::CUBE,
        m if m == builtin::SPHERE => icons::SPHERE,
        m if m == builtin::PLANE => icons::PLANE,
        m if m == builtin::CYLINDER => icons::CYLINDER,
        _ => icons::MODEL,
    }
}

/// `base`, or `base` numbered ("Cube 2") when a name in `taken` already has it; a number
/// `base` ends with is replaced ("Cube 2" gives "Cube 3").
fn unique_name(base: &str, taken: &HashSet<&str>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    let stem = match base.rsplit_once(' ') {
        Some((head, tail)) if tail.parse::<u32>().is_ok() => head,
        _ => base,
    };
    (2..)
        .map(|n| format!("{stem} {n}"))
        .find(|name| !taken.contains(name.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

/// A new entity of the add menu's kind `what` named `name`, at `at`.
fn spawn(world: &mut World, what: usize, name: &str, at: Vec3) -> Entity {
    let mut transform = Transform::from_translation(at);
    let e = world.spawn((SceneId::new(), Name::new(name), GlobalTransform::default()));
    let model = |mesh| Model::new(mesh, builtin::MATERIAL);
    match what {
        1 => {
            world.insert(e, model(builtin::CUBE));
        }
        2 => {
            world.insert(e, model(builtin::SPHERE));
        }
        3 => {
            world.insert(e, model(builtin::PLANE));
        }
        4 => {
            world.insert(e, model(builtin::CYLINDER));
        }
        5 => {
            transform.rotation = Quat::from_rotation_y(0.6) * Quat::from_rotation_x(-0.9);
            world.insert(
                e,
                Sun {
                    color: Vec3::splat(1.2),
                    ambient: Vec3::splat(0.04),
                },
            );
        }
        6 => {
            world.insert(e, PointLight::new(Vec3::splat(10.0), 8.0));
        }
        7 => {
            // Spots shine down their −Z axis: pointed at the floor.
            transform.rotation = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2);
            world.insert(e, SpotLight::new(Vec3::splat(20.0), 12.0, 0.35, 0.5));
        }
        8 => {
            world.insert(e, Camera::perspective(60f32.to_radians(), 0.1, 500.0));
        }
        _ => {}
    }
    world.insert(e, transform);
    e
}

/// The first `untitled.scene`, `untitled-2.scene`… not in `root`.
fn untitled(root: &Path) -> PathBuf {
    (1..)
        .map(|n| {
            PathBuf::from(if n == 1 {
                String::from("untitled.scene")
            } else {
                format!("untitled-{n}.scene")
            })
        })
        .find(|p| !root.join(p).exists())
        .unwrap_or_else(|| PathBuf::from("untitled.scene"))
}

impl Editor {
    fn new(
        size: [u32; 2],
        root: PathBuf,
        scene_path: Option<PathBuf>,
        scene_id: AssetId,
        assets: AssetPaths,
    ) -> Result<Self, Error> {
        let palette = Palette::default();
        let ui = Ui::new(Theme {
            text: palette.text,
            panel: palette.dock,
            ..Theme::default()
        });
        let (commands, keymap, ids, shortcuts) = commands();
        Ok(Self {
            ui,
            text: UiText::new()?,
            input: InputCollector::new(),
            palette,
            scale: 1.0,
            size,
            orbit: OrbitCamera {
                target: Vec3::ZERO,
                distance: 8.0,
                ..OrbitCamera::default()
            },
            camera: None,
            camera_drag: false,
            selection: Vec::new(),
            anchor: None,
            tree_focus: false,
            reveal: None,
            selection_label: String::new(),
            collapsed: HashSet::new(),
            view_rect: None,
            root,
            scene_path,
            scene_id,
            saved_head: 0,
            session: None,
            game_crate: None,
            builds: None,
            problems: Vec::new(),
            build_state: BuildState::Idle,
            ready: None,
            restart_with: None,
            focused: false,
            restarted_at: Restarted::from_env().map(|r| r.at),
            window: None,
            ended: None,
            restart_policy: RestartPolicy::Background,
            scene_label: String::new(),
            assets,
            pending_assets: AssetPaths::new(),
            history: History::new(),
            commands,
            keymap,
            ids,
            shortcuts,
            command_palette: None,
            snapping: Snapping::default(),
            inspector: Inspector::new(),
            defaults: Defaults::new(),
            drag: None,
            typing: None,
            renaming: None,
            last_click: None,
            tree_filter: String::new(),
            inspector_filter: String::new(),
            shown: Vec::new(),
            shown_sections: Vec::new(),
            open_choice: None,
            add_open: false,
            gestures: 0,
            tools: [
                format!("{} Select", icons::TOOL_SELECT),
                format!("{} Move", icons::TOOL_MOVE),
                format!("{} Rotate", icons::TOOL_ROTATE),
                format!("{} Scale", icons::TOOL_SCALE),
            ],
            active_tool: 0,
            status: String::new(),
            message: None,
            rows: Vec::new(),
            scene_ids: None,
            frames: 0,
            since: Instant::now(),
            fps: 0.0,
        })
    }

    fn event(&mut self, event: &WindowEvent) {
        let input = &mut self.input;
        match event.kind {
            WindowEventKind::Resized { width, height } if width > 0 && height > 0 => {
                self.size = [width, height];
            }
            WindowEventKind::ScaleChanged { dpi } => self.scale = dpi as f32 / 96.0,
            WindowEventKind::Focused(focused) => {
                if !focused {
                    input.focus_lost();
                }
                self.focused = focused;
            }
            WindowEventKind::CursorMoved { x, y } => input.cursor_moved(x as f32, y as f32),
            WindowEventKind::CursorLeft => input.cursor_left(),
            WindowEventKind::MouseButton { button, pressed } => {
                let button = match button {
                    MouseButton::Left => Some(Button::Left),
                    MouseButton::Right => Some(Button::Right),
                    MouseButton::Middle => Some(Button::Middle),
                    _ => None,
                };
                if let Some(button) = button {
                    input.button(button, pressed);
                }
            }
            WindowEventKind::Wheel { y, .. } => input.wheel(y),
            WindowEventKind::Key {
                virtual_key,
                scancode,
                extended,
                pressed,
                ..
            } => input.key(virtual_key, scancode, extended, pressed),
            WindowEventKind::Char(c) => input.char(c),
            _ => {}
        }
        if let Some(s) = &mut self.session
            && matches!(
                event.kind,
                WindowEventKind::CursorMoved { .. }
                    | WindowEventKind::MouseButton { .. }
                    | WindowEventKind::Wheel { .. }
                    | WindowEventKind::Key { .. }
                    | WindowEventKind::Char(_)
            )
        {
            s.last_input = event.time;
        }
    }

    /// The scene's entities in tree order, under open rows only.
    fn collect_rows(&mut self, world: &mut World) {
        // The roots by their order ([`RootOrder`]); those without one (a scene of format
        // 2, roots made by code) get one after the rest in spawn order, what the tree
        // showed, kept by the next save.
        let mut roots = self.roots(world, None);
        if roots
            .last()
            .is_some_and(|&e| world.get::<RootOrder>(e).is_none())
        {
            let mut next = roots
                .iter()
                .filter_map(|&e| world.get::<RootOrder>(e))
                .map(|o| o.0)
                .max()
                .unwrap_or(0);
            for &e in &roots {
                if world.get::<RootOrder>(e).is_none() {
                    next = next.saturating_add(RootOrder::STEP);
                    world.insert(e, RootOrder(next));
                }
            }
            roots = self.roots(world, None);
        }
        let world = &*world;
        self.rows.clear();
        // Filtered: the matching rows and their ancestors, open whatever was collapsed.
        let filter = self.tree_filter.trim().to_lowercase();
        let mut keep: HashSet<Entity> = HashSet::new();
        if !filter.is_empty() {
            let mut matches: Vec<Entity> = Vec::new();
            let mut stack: Vec<Entity> = roots.clone();
            while let Some(e) = stack.pop() {
                let name = world.get::<Name>(e).map_or_else(
                    || kind_of(world, e).1.to_lowercase(),
                    |n| n.as_str().to_lowercase(),
                );
                if name.contains(&filter) {
                    matches.push(e);
                }
                stack.extend(
                    world
                        .children(e)
                        .filter(|&c| world.get::<SceneId>(c).is_some()),
                );
            }
            for e in matches {
                let mut at = Some(e);
                while let Some(x) = at {
                    if !keep.insert(x) {
                        break;
                    }
                    at = world
                        .parent(x)
                        .filter(|&p| world.get::<SceneId>(p).is_some());
                }
            }
            roots.retain(|e| keep.contains(e));
        }
        let mut stack: Vec<(Entity, u16)> = roots.iter().rev().map(|&e| (e, 0)).collect();
        while let Some((e, depth)) = stack.pop() {
            let filtered = !filter.is_empty();
            let mut children = world
                .children(e)
                .filter(|&c| world.get::<SceneId>(c).is_some())
                .filter(|c| !filtered || keep.contains(c))
                .peekable();
            let has_children = children.peek().is_some();
            self.rows.push(Row {
                entity: e,
                depth,
                children: has_children,
            });
            if has_children && (filtered || !self.collapsed.contains(&e)) {
                let kids: Vec<Entity> = children.collect();
                stack.extend(kids.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
    }

    /// The editor's camera entity, made on first use.
    fn camera(&mut self, world: &mut World) -> Entity {
        if let Some(e) = self.camera.filter(|&e| world.get::<Camera>(e).is_some()) {
            return e;
        }
        let mut camera = Camera::perspective(self.orbit.fov_y, self.orbit.near, self.orbit.far);
        // Above every camera of the scene while editing.
        camera.priority = i32::MAX;
        camera.active = 1;
        let e = world.spawn((Transform::IDENTITY, camera));
        self.camera = Some(e);
        e
    }

    /// Shows `message` in the status bar for a few seconds.
    fn say(&mut self, message: String) {
        log::info!(target: "engine_editor", "{message}");
        self.message = Some((message, Instant::now()));
    }

    /// The frame's editing: the UI from the world, its edits and the shortcuts' commands
    /// into the world, the camera from the viewport's drags.
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, world: &mut World) {
        self.tick_session(world);
        self.tick_builds(world);
        let camera = self.camera(world);
        self.collect_rows(world);
        self.frames += 1;
        let elapsed = self.since.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            self.fps = self.frames as f64 / elapsed;
            self.frames = 0;
            self.since = Instant::now();
        }

        // The inspector: the active entity's components, field by field.
        self.selection
            .retain(|&e| world.get::<SceneId>(e).is_some());
        if self
            .anchor
            .is_some_and(|e| world.get::<SceneId>(e).is_none())
        {
            self.anchor = self.active();
        }
        match self.active() {
            Some(e) => {
                self.inspector
                    .build(world, e, &self.assets, &mut self.defaults);
                // The others selected: their shared components, "—" where they differ.
                let others = &self.selection[..self.selection.len() - 1];
                self.inspector
                    .compare(world, &self.assets, e, others, &mut self.defaults);
            }
            None => {
                self.inspector.clear();
                self.drag = None;
                self.open_choice = None;
            }
        }
        let properties = self.inspector.properties();
        let mut values: Vec<&str> = Vec::with_capacity(properties.len() * 3);
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(properties.len());
        for p in properties {
            let start = values.len();
            values.extend(
                self.inspector
                    .values(p)
                    .enumerate()
                    .map(|(i, v)| if p.mixed & 1 << i != 0 { MIXED } else { v }),
            );
            spans.push((start, values.len()));
        }
        let open = self.open_choice;
        let options: Vec<&'static str> = open
            .and_then(|(c, o)| {
                properties
                    .iter()
                    .find(|p| p.component == c && p.offset == o)
            })
            .map(|p| inspector::variants(p).collect())
            .unwrap_or_default();
        // The filter keeps the fields whose name matches, and every field of a component
        // whose name does.
        let filter = self.inspector_filter.trim().to_lowercase();
        self.shown.clear();
        self.shown_sections.clear();
        for section in self.inspector.sections() {
            let start = self.shown.len();
            let title_matches = section.title.to_lowercase().contains(&filter);
            for i in section.properties.clone() {
                let name = self.inspector.name(&properties[i]);
                if filter.is_empty() || title_matches || name.to_lowercase().contains(&filter) {
                    self.shown.push(i);
                }
            }
            if self.shown.len() > start {
                self.shown_sections.push((start, self.shown.len()));
            }
        }
        let typing = self.typing;
        let fields: Vec<Field<'_>> = self
            .shown
            .iter()
            .map(|&i| {
                let p = &properties[i];
                let (a, b) = spans[i];
                let shown = &values[a..b];
                let first = shown.first().copied().unwrap_or("");
                let widget = match p.kind {
                    Kind::Numbers { .. } | Kind::Rotation => Widget::Numbers {
                        values: shown,
                        labels: if shown.len() > 1 {
                            Inspector::labels(p)
                        } else {
                            &[]
                        },
                        typing: typing
                            .filter(|&(c, o, _)| c == p.component && o == p.offset)
                            .map(|(_, _, part)| part),
                    },
                    Kind::Bool => Widget::Check((p.mixed == 0).then_some(first == "true")),
                    Kind::Choice => Widget::Choice {
                        current: first,
                        open: (open == Some((p.component, p.offset))).then_some(options.as_slice()),
                    },
                    Kind::Text if matches!(p.ty.kind, engine_reflect::TypeKind::String) => {
                        Widget::Edit(first)
                    }
                    Kind::Text => Widget::Text(first),
                };
                Field {
                    name: self.inspector.name(p),
                    widget,
                    changed: p.changed,
                }
            })
            .collect();
        let section_list = self.inspector.sections();
        let sections: Vec<Section<'_>> = self
            .shown_sections
            .iter()
            .map(|&(a, b)| {
                let title = section_list
                    .iter()
                    .find(|s| s.properties.contains(&self.shown[a]))
                    .map_or("", |s| s.title);
                Section {
                    title,
                    fields: &fields[a..b],
                }
            })
            .collect();

        // The tree. A large selection is looked up through a set (one row at a time
        // would be rows times selected).
        let many = self.selection.len() > 16;
        let set: HashSet<Entity> = if many {
            self.selection.iter().copied().collect()
        } else {
            HashSet::new()
        };
        let in_selection = |e: Entity| {
            if many {
                set.contains(&e)
            } else {
                self.selection.contains(&e)
            }
        };
        let items: Vec<TreeItem<'_>> = self
            .rows
            .iter()
            .map(|row| {
                let (icon, kind) = kind_of(world, row.entity);
                TreeItem {
                    depth: row.depth,
                    icon,
                    name: world.get::<Name>(row.entity).map_or(kind, Name::as_str),
                    kind,
                    expanded: row.children.then(|| !self.collapsed.contains(&row.entity)),
                    selected: in_selection(row.entity),
                    active: Some(row.entity) == self.active(),
                    renaming: Some(row.entity) == self.renaming,
                }
            })
            .collect();
        self.selection_label.clear();
        if let Some(e) = self.active() {
            let name = world
                .get::<Name>(e)
                .map_or(kind_of(world, e).1, Name::as_str);
            self.selection_label.push_str(name);
            if self.selection.len() > 1 {
                let _ = write!(
                    self.selection_label,
                    "  ·  {} selected",
                    self.selection.len()
                );
            }
        }
        let reveal_row = self
            .reveal
            .take()
            .and_then(|e| self.rows.iter().position(|r| r.entity == e));
        let add_items: [MenuItem<'_>; ADD.len()] = core::array::from_fn(|i| MenuItem {
            icon: ADD_ICONS[i],
            label: ADD[i],
        });
        self.status.clear();
        match &self.build_state {
            BuildState::Building(since) => {
                let _ = write!(
                    self.status,
                    "Building the game's code… {:.1} s  ·  ",
                    since.elapsed().as_secs_f32()
                );
            }
            BuildState::Failed(text) if self.problems.is_empty() => {
                let _ = write!(self.status, "{text}  ·  ");
            }
            BuildState::Failed(text) => {
                let _ = write!(self.status, "{text} (Build: Show Errors…)  ·  ");
            }
            BuildState::Idle if self.ready.is_some() => {
                self.status
                    .push_str("The new build is ready: Ctrl+R restarts with it  ·  ");
            }
            BuildState::Idle => {}
        }
        if self.session.as_ref().is_some_and(|s| s.conflict) {
            let _ = write!(
                self.status,
                "{} changed on disk since your unsaved changes: Take Disk Version or Save \
                 Over Disk Version (Ctrl+Shift+P)  ·  ",
                self.file_name()
            );
        }
        if let Some((message, at)) = &self.message
            && at.elapsed().as_secs_f32() < MESSAGE_SECONDS
        {
            let _ = write!(self.status, "{message}  ·  ");
        }
        let _ = write!(
            self.status,
            "{} entities  ·  {:.0} FPS  ·  {}×{}",
            self.rows.len(),
            self.fps,
            self.size[0],
            self.size[1]
        );
        self.scene_label.clear();
        match self.scene_path.as_ref().and_then(|p| p.file_name()) {
            Some(name) => self.scene_label.push_str(&name.to_string_lossy()),
            None => self.scene_label.push_str("untitled"),
        }
        if self.history.head() != self.saved_head {
            self.scene_label.push('*');
        }
        if self.session.as_ref().is_some_and(|s| s.conflict) {
            self.scene_label.push(' ');
            self.scene_label.push_str(icons::ALERT);
        }
        // The command palette's results in view.
        let mut palette_rows: Vec<PaletteRow<'_>> = Vec::new();
        if let Some(cp) = &self.command_palette {
            let end = (cp.first + PALETTE_ROWS).min(cp.len());
            for i in cp.first..end {
                palette_rows.push(if let Some(choices) = &cp.choices {
                    let (item, matched) = choices.shown[i];
                    let c = &choices.items[item];
                    PaletteRow {
                        icon: icons::SCENE,
                        label: &c.label,
                        matched: &cp.highlights[matched.0 as usize..matched.1 as usize],
                        category: 0,
                        detail: &c.detail,
                    }
                } else if cp.finds_objects() {
                    let found = cp.objects[i];
                    let (icon, kind) = kind_of(world, found.entity);
                    PaletteRow {
                        icon,
                        label: world.get::<Name>(found.entity).map_or(kind, Name::as_str),
                        matched: &cp.highlights[found.matched.0 as usize..found.matched.1 as usize],
                        category: 0,
                        detail: kind,
                    }
                } else {
                    let m = &cp.commands[i];
                    let info = self.commands.info(m.id);
                    PaletteRow {
                        icon: self
                            .ids
                            .add_kinds
                            .iter()
                            .position(|&k| k == m.id)
                            .map_or("", |i| ADD_ICONS[i]),
                        label: info.label(),
                        matched: self.commands.highlights(m),
                        // "Scene: " is dimmed.
                        category: if info.category.is_empty() {
                            0
                        } else {
                            info.category.chars().count() + 2
                        },
                        detail: self.shortcuts.get(&m.id).map_or("", String::as_str),
                    }
                });
            }
        }
        let palette = self.command_palette.as_ref().map(|cp| PaletteView {
            query: &cp.query,
            hint: cp
                .choices
                .as_ref()
                .map_or("Type a command, or @ and an object's name", |c| c.hint),
            rows: &palette_rows,
            selected: cp.selected.saturating_sub(cp.first),
        });
        let scenes = [self.scene_label.as_str()];
        let tools = self.tools.each_ref().map(String::as_str);
        let model = ShellModel {
            scenes: &scenes,
            active_scene: 0,
            tools: &tools,
            active_tool: self.active_tool,
            tree: &items,
            selection: &self.selection_label,
            inspector: &sections,
            path: "res://",
            folders: &[],
            files: &[],
            bottom_tabs: &[
                "Output",
                "Debugger",
                "Audio",
                "Animation",
                "Shader",
                "Profiler",
            ],
            active_bottom: None,
            playing: false,
            status: &self.status,
            snap: self.snapping.on,
            add_menu: self.add_open.then_some(&add_items[..]),
            reveal_row,
            tree_filter: &self.tree_filter,
            inspector_filter: &self.inspector_filter,
            palette,
        };
        let scale = self.scale.max(0.5);
        let logical = [self.size[0] as f32 / scale, self.size[1] as f32 / scale];
        self.ui.begin_frame(self.input.ui_input(scale), logical);
        let response = build(&mut self.ui, &model, &self.palette);
        self.ui.end_frame(&mut self.text);

        if let Some(i) = response.tool {
            self.active_tool = i;
        }
        if response.snap {
            self.run(world, self.ids.snap);
        }
        // A press tells where the keys go: the tree's arrows after a press in it.
        if self.ui.input().mouse_pressed {
            self.tree_focus = response.tree_hovered;
        }
        if dismissed(&self.ui, &response) {
            self.add_open = false;
            self.open_choice = None;
            self.command_palette = None;
        }
        if response.add {
            self.add_open = !self.add_open;
        }
        if let Some(i) = response.add_item {
            self.add(world, i);
        }
        // What was copied goes to the clipboard.
        if let Some(text) = self.ui.take_copied() {
            self.input.clipboard.set_text(&text);
        }
        if let Some(row) = response.tree_toggle.and_then(|i| self.rows.get(i).copied())
            && !self.collapsed.remove(&row.entity)
        {
            self.collapsed.insert(row.entity);
        }
        if let Some(row) = response.tree_row.and_then(|i| self.rows.get(i).copied()) {
            if self.input.shift() {
                self.select_range(row.entity, self.input.ctrl());
                self.last_click = None;
            } else if self.input.ctrl() {
                self.toggle(row.entity);
                self.last_click = None;
            } else {
                // A second click on the same row soon after renames it.
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(e, at)| {
                    e == row.entity && now.duration_since(at).as_secs_f32() < 0.45
                });
                self.last_click = (!double).then_some((row.entity, now));
                self.select_only(row.entity);
                if double {
                    self.renaming = Some(row.entity);
                }
            }
        }
        if let Some(commit) = response.rename {
            if commit && let Some(e) = self.renaming {
                let name = self.ui.edited_text().trim().to_owned();
                self.rename(world, e, &name);
            }
            self.renaming = None;
        }
        if let Some(r) = response.tree_filter {
            if r.cancelled {
                self.tree_filter.clear();
            } else if r.started || r.changed {
                self.tree_filter.clear();
                self.tree_filter.push_str(self.ui.edited_text());
            }
        }
        if let Some(r) = response.inspector_filter {
            if r.cancelled {
                self.inspector_filter.clear();
            } else if r.started || r.changed {
                self.inspector_filter.clear();
                self.inspector_filter.push_str(self.ui.edited_text());
            }
        }
        if let Some(event) = response.field {
            self.edit(world, event);
        }
        // Over a field that is not being typed in: Backspace resets it, Ctrl+C and Ctrl+V
        // copy and paste its value (Blender).
        if let Some((section, field)) = response.hovered_field
            && self.ui.editing().is_none()
            && response.popup.is_none()
            && self.drag.is_none()
        {
            let keys = &self.ui.input().keys;
            let (reset, copy, paste) = (
                keys.contains(&UiKey::Backspace),
                keys.contains(&UiKey::Copy),
                keys.contains(&UiKey::Paste),
            );
            if reset {
                let action = FieldAction::Reset;
                self.edit(
                    world,
                    FieldEvent {
                        section,
                        field,
                        action,
                    },
                );
            }
            if copy {
                self.copy_value(world, section, field);
            }
            if paste {
                let text = self.ui.input().paste.clone();
                self.paste_value(world, section, field, &text);
            }
        }
        if let Some(action) = response.palette {
            self.palette_action(world, action);
        }
        // Escape during a drag puts the value back as it was.
        if self.ui.input().keys.contains(&UiKey::Escape)
            && let Some(drag) = self.drag.as_mut()
            && !drag.cancelled
        {
            drag.cancelled = true;
            let merge = drag.merge;
            self.history.revert(world, merge);
        }
        // The shortcuts; while a field is typed in, only saving (the keys are the field's).
        let typing_text = self.ui.editing().is_some();
        let contexts: &[KeyContext] = if self.tree_focus { &[TREE_KEYS] } else { &[] };
        for i in 0..self.input.chords().len() {
            let chord = self.input.chords()[i];
            if let Some(id) = self.keymap.lookup(chord, contexts)
                && (!typing_text || id == self.ids.save)
            {
                self.run(world, id);
            }
        }

        // The viewport: drags that start in it move the camera, the wheel over it zooms.
        let rect = self
            .ui
            .rect_of(response.viewport)
            .map(|r| [r.pos[0], r.pos[1], r.size[0], r.size[1]]);
        if let Some([x, y, w, h]) = rect {
            let over = self.input.cursor().is_some_and(|[cx, cy]| {
                let (cx, cy) = (cx / scale, cy / scale);
                cx >= x && cy >= y && cx < x + w && cy < y + h
            });
            if (self.input.pressed(Button::Right) || self.input.pressed(Button::Middle)) && over {
                self.camera_drag = true;
            }
            if !self.input.held(Button::Right) && !self.input.held(Button::Middle) {
                self.camera_drag = false;
            }
            if self.camera_drag {
                let [dx, dy] = self.input.drag();
                if self.input.held(Button::Middle) {
                    if self.input.shift() {
                        self.orbit.pan(dx, dy, h * scale);
                    } else {
                        self.orbit.orbit(dx, dy);
                    }
                } else {
                    self.orbit.look(dx, dy);
                }
            }
            if over {
                self.orbit.zoom(self.input.wheel_notches());
            }
            self.view_rect = Some([
                (x * scale).round() as u32,
                (y * scale).round() as u32,
                ((w * scale).round() as u32).max(1),
                ((h * scale).round() as u32).max(1),
            ]);
        }
        self.input.next_frame();
        let eye = self.orbit.eye();
        if let Some(t) = world.get_mut::<Transform>(camera) {
            *t = Transform::from_translation(eye).with_rotation(look_rotation(
                eye,
                self.orbit.target,
                Vec3::Y,
            ));
        }
    }

    /// The property shown as field `field` of shown section `section` (what a field event
    /// points at), with its component's name.
    fn shown_property(&self, section: usize, field: usize) -> Option<(Property, &'static str)> {
        let &(start, end) = self.shown_sections.get(section)?;
        let &index = self
            .shown
            .get(start + field)
            .filter(|_| start + field < end)?;
        let p = *self.inspector.properties().get(index)?;
        let component = self
            .inspector
            .sections()
            .iter()
            .find(|s| s.properties.contains(&index))?
            .title;
        Some((p, component))
    }

    /// A property's scene path: `Transform.translation`, or the component's name alone
    /// for one written whole (`Name`).
    fn path_of(&self, p: &Property, component: &str) -> String {
        if p.whole {
            component.to_owned()
        } else {
            format!("{component}.{}", self.inspector.name(p))
        }
    }

    /// The selected entities that have component `id`, the active one last: what an
    /// inspector edit applies to.
    fn targets(&self, world: &World, id: ComponentId) -> Vec<Entity> {
        self.selection
            .iter()
            .copied()
            .filter(|&e| world.get_raw(e, id).is_some())
            .collect()
    }

    /// Applies an inspector field's action to every selected entity, as one undoable step.
    fn edit(&mut self, world: &mut World, event: FieldEvent) {
        if self.active().is_none() {
            return;
        }
        let Some((p, component)) = self.shown_property(event.section, event.field) else {
            return;
        };
        let targets = self.targets(world, p.component);
        let title = format!("{component}.{}", self.inspector.name(&p));
        match event.action {
            FieldAction::Drag {
                part,
                travel,
                start,
            } => {
                if start || self.drag.is_none() {
                    let starts = targets
                        .iter()
                        .filter_map(|&t| {
                            let ptr = world.get_raw(t, p.component)?;
                            // SAFETY: the entity's live value of the property's component.
                            Some((t, unsafe { inspector::read_numbers(&p, ptr.as_ptr()) }))
                        })
                        .collect();
                    self.gestures += 1;
                    self.drag = Some(Drag {
                        property: p,
                        part: usize::from(part),
                        starts,
                        merge: self.gestures,
                        name: format!("Edit {title}"),
                        cancelled: false,
                        moved: false,
                    });
                    self.typing = None;
                    return;
                }
                if travel != 0.0
                    && let Some(drag) = self.drag.as_mut()
                {
                    drag.moved = true;
                }
                let Some(drag) = self.drag.as_ref().filter(|d| !d.cancelled) else {
                    return;
                };
                let quantity = match drag.property.kind {
                    Kind::Numbers { quantity, .. } => quantity,
                    Kind::Rotation => Quantity::Angle,
                    _ => return,
                };
                let (shift, ctrl) = (self.input.shift(), self.input.ctrl());
                let moved: Vec<(Entity, [f64; 4])> = drag
                    .starts
                    .iter()
                    .map(|&(t, start)| {
                        let mut values = start;
                        values[drag.part] =
                            self.snapping
                                .drag(quantity, start[drag.part], travel, shift, ctrl);
                        (t, values)
                    })
                    .collect();
                let (property, merge, name) = (drag.property, drag.merge, drag.name.clone());
                self.history.begin(&name, Some(merge));
                for (t, values) in moved {
                    self.history.touch(world, t);
                    if let Some(ptr) = world.get_raw_mut(t, property.component) {
                        // SAFETY: the entity's live value of the property's component.
                        unsafe { inspector::write_numbers(&property, ptr.as_ptr(), values) };
                    }
                }
                self.history.commit(world);
            }
            FieldAction::Release => {
                // A press and release without moving: type the value instead.
                if let Some(drag) = self.drag.take()
                    && !drag.moved
                    && !drag.cancelled
                {
                    self.typing = Some((
                        drag.property.component,
                        drag.property.offset,
                        drag.part as u8,
                    ));
                }
            }
            FieldAction::Typed { part, commit } => {
                self.typing = None;
                if !commit {
                    return;
                }
                let text = self.ui.edited_text().trim().to_owned();
                if text == MIXED {
                    // "—" kept as it was: each keeps its own.
                    return;
                }
                let part = usize::from(part).min(3);
                // An absolute value goes to every entity; a relative one (`+=1`) is worked
                // out from each one's own.
                self.history.begin(&format!("Set {title}"), None);
                for &t in &targets {
                    let Some(ptr) = world.get_raw(t, p.component) else {
                        continue;
                    };
                    // SAFETY: the entity's live value of the property's component.
                    let mut values = unsafe { inspector::read_numbers(&p, ptr.as_ptr()) };
                    match crate::expr::evaluate(&text, values[part]) {
                        Ok(value) => values[part] = value,
                        Err(reason) => {
                            self.history.cancel(world);
                            self.say(format!("{title}: {reason}"));
                            return;
                        }
                    }
                    self.history.touch(world, t);
                    if let Some(ptr) = world.get_raw_mut(t, p.component) {
                        // SAFETY: the entity's live value of the property's component.
                        unsafe { inspector::write_numbers(&p, ptr.as_ptr(), values) };
                    }
                }
                self.history.commit(world);
            }
            FieldAction::Text { commit } => {
                if !commit {
                    return;
                }
                if self.ui.edited_text().trim() == MIXED {
                    return;
                }
                let text = engine_scene::text::quote(self.ui.edited_text());
                let path = self.path_of(&p, component);
                if let Err(reason) = self.set_on(world, &targets, &path, &text) {
                    self.say(format!("{title}: {reason}"));
                }
            }
            FieldAction::Toggle => {
                // All on turns off; otherwise (off, or differing) all on.
                let on = |world: &World, t: Entity| {
                    world.get_raw(t, p.component).is_some_and(|ptr| {
                        // SAFETY: the entity's live value of the property's component.
                        unsafe { *ptr.as_ptr().add(p.offset) != 0 }
                    })
                };
                let to = !targets.iter().all(|&t| on(world, t));
                self.history.begin(&format!("Toggle {title}"), None);
                for &t in &targets {
                    if on(world, t) == to {
                        continue;
                    }
                    self.history.touch(world, t);
                    if let Some(ptr) = world.get_raw_mut(t, p.component) {
                        // SAFETY: the entity's live value of the property's component.
                        unsafe { inspector::toggle(&p, ptr.as_ptr()) };
                    }
                }
                self.history.commit(world);
            }
            FieldAction::Open => {
                let key = (p.component, p.offset);
                self.open_choice = (self.open_choice != Some(key)).then_some(key);
            }
            FieldAction::Choose(i) => {
                self.open_choice = None;
                self.history.begin(&format!("Set {title}"), None);
                for &t in &targets {
                    self.history.touch(world, t);
                    if let Some(ptr) = world.get_raw_mut(t, p.component) {
                        // SAFETY: the entity's live value of the property's component.
                        unsafe { inspector::choose(&p, ptr.as_ptr(), i) };
                    }
                }
                self.history.commit(world);
            }
            FieldAction::Reset => {
                let Some(default) = self.defaults.of(world, p.component).filter(|_| p.changed)
                else {
                    return;
                };
                self.history.begin(&format!("Reset {title}"), None);
                for &t in &targets {
                    self.history.touch(world, t);
                    if let Some(ptr) = world.get_raw_mut(t, p.component) {
                        // SAFETY: the entity's live value of the property's component, and
                        // the component type's default value.
                        unsafe { inspector::reset(&p, ptr.as_ptr(), default.as_ptr()) };
                    }
                }
                self.history.commit(world);
            }
        }
    }

    /// Puts a field's value on the clipboard: its scene text (`(0, 1.5, 0)`, `true`,
    /// `Run`), a string as it is.
    fn copy_value(&mut self, world: &World, section: usize, field: usize) {
        let Some(e) = self.active() else {
            return;
        };
        let Some((p, component)) = self.shown_property(section, field) else {
            return;
        };
        let Some(ptr) = world.get_raw(e, p.component) else {
            return;
        };
        let title = format!("{component}.{}", self.inspector.name(&p));
        let mut text = String::new();
        // SAFETY: the property's value, inside the entity's live component.
        unsafe {
            let at = ptr.as_ptr().cast_const().add(p.offset);
            if matches!(p.ty.kind, TypeKind::String) {
                text.push_str(engine_reflect::read_str(at));
            } else {
                // A single value is written ` = <text>`, with no path of its own.
                let ids = |x: Entity| world.get::<SceneId>(x).copied();
                let mut lines = String::new();
                engine_scene::write_lines_with(p.ty, at, &ids, Some(&self.assets), &mut lines);
                match lines.trim_end().strip_prefix(" = ") {
                    Some(value) if !value.contains('\n') => text.push_str(value),
                    _ => {
                        self.say(format!("{title}: not a single value, nothing copied"));
                        return;
                    }
                }
            }
        }
        if self.input.clipboard.set_text(&text) {
            self.say(format!("Copied {title}"));
        } else {
            self.say(String::from("The clipboard is busy: nothing copied"));
        }
    }

    /// Sets a field of every selected entity from pasted text, read as its copy is
    /// written (scene text, a string as it is), as one undoable step.
    fn paste_value(&mut self, world: &mut World, section: usize, field: usize, text: &str) {
        if self.active().is_none() {
            return;
        }
        let Some((p, component)) = self.shown_property(section, field) else {
            return;
        };
        let targets = self.targets(world, p.component);
        let title = format!("{component}.{}", self.inspector.name(&p));
        let value = if matches!(p.ty.kind, TypeKind::String) {
            // One line, as a text field pastes it.
            let line: String = text
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            engine_scene::text::quote(&line)
        } else {
            text.trim().to_owned()
        };
        let path = self.path_of(&p, component);
        match self.set_on(world, &targets, &path, &value) {
            Ok(()) => self.say(format!("Pasted {title}")),
            Err(reason) => self.say(format!("{title}: {reason}")),
        }
    }

    /// Opens the command palette with `query` typed (`@` finds the scene's objects).
    fn open_palette(&mut self, world: &mut World, query: &str) {
        self.add_open = false;
        self.open_choice = None;
        let cp = self
            .command_palette
            .get_or_insert_with(CommandPalette::default);
        cp.choices = None;
        cp.query.clear();
        cp.query.push_str(query);
        self.search_palette(world);
    }

    /// Finds what the palette's query names: commands, or after `@` the scene's objects,
    /// best first (in tree order among equals); the first highlighted.
    fn search_palette(&mut self, world: &mut World) {
        let Some(cp) = self.command_palette.as_mut() else {
            return;
        };
        cp.selected = 0;
        cp.first = 0;
        cp.objects.clear();
        cp.highlights.clear();
        let query = cp.query.trim_start();
        if let Some(choices) = &mut cp.choices {
            // A list keeps its own order; the query narrows it.
            choices.shown.clear();
            cp.fuzzy.set_query(query);
            for (i, item) in choices.items.iter().enumerate() {
                if cp.fuzzy.score(&item.label).is_some() {
                    let start = cp.highlights.len() as u32;
                    cp.highlights.extend_from_slice(cp.fuzzy.positions());
                    choices.shown.push((i, (start, cp.highlights.len() as u32)));
                }
            }
        } else if let Some(name) = query.strip_prefix('@') {
            cp.commands.clear();
            let ids = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
            let world = &*world;
            tree_order(world, ids, &mut cp.stack, &mut cp.order);
            cp.fuzzy.set_query(name);
            for (order, &e) in cp.order.iter().enumerate() {
                let label = world
                    .get::<Name>(e)
                    .map_or_else(|| kind_of(world, e).1, Name::as_str);
                if let Some(score) = cp.fuzzy.score(label) {
                    let start = cp.highlights.len() as u32;
                    cp.highlights.extend_from_slice(cp.fuzzy.positions());
                    cp.objects.push(Found {
                        entity: e,
                        score,
                        order: order as u32,
                        matched: (start, cp.highlights.len() as u32),
                    });
                }
            }
            cp.objects
                .sort_unstable_by_key(|f| (core::cmp::Reverse(f.score), f.order));
        } else {
            let query = query.strip_prefix('>').unwrap_or(query);
            self.commands.search(query, &mut cp.commands);
        }
    }

    /// Applies what the user did in the command palette.
    fn palette_action(&mut self, world: &mut World, action: PaletteAction) {
        match action {
            PaletteAction::Query => {
                if let Some(cp) = &mut self.command_palette {
                    cp.query.clear();
                    cp.query.push_str(self.ui.edited_text());
                }
                self.search_palette(world);
            }
            PaletteAction::Move(by) => {
                if let Some(cp) = &mut self.command_palette {
                    let n = cp.len() as i64;
                    if n > 0 {
                        let i = (cp.selected as i64 + i64::from(by)).rem_euclid(n);
                        cp.select(i as usize);
                    }
                }
            }
            PaletteAction::Run(row) => {
                let Some(cp) = self.command_palette.take() else {
                    return;
                };
                let i = cp.first + row;
                if let Some(choices) = &cp.choices {
                    if let Some(&(item, _)) = choices.shown.get(i) {
                        match choices.items[item].pick.clone() {
                            Pick::Autosave(path) => self.restore_autosave(world, &path),
                            Pick::Problem(p) => self.open_problem(p),
                            Pick::Policy(p) => self.set_restart_policy(world, p),
                        }
                    }
                } else if cp.finds_objects() {
                    if let Some(found) = cp.objects.get(i) {
                        self.show(world, found.entity);
                    }
                } else if let Some(m) = cp.commands.get(i) {
                    self.run(world, m.id);
                }
            }
            PaletteAction::Close => self.command_palette = None,
        }
    }

    /// The active entity: the last picked, which the inspector shows and F2 renames.
    fn active(&self) -> Option<Entity> {
        self.selection.last().copied()
    }

    /// Selects `e` alone, the anchor of the next range.
    fn select_only(&mut self, e: Entity) {
        let before = self.active();
        self.selection.clear();
        self.selection.push(e);
        self.anchor = Some(e);
        self.selection_changed(before);
    }

    /// Adds `e` to the selection, active, or takes it out (Ctrl+click).
    fn toggle(&mut self, e: Entity) {
        let before = self.active();
        if let Some(i) = self.selection.iter().position(|&s| s == e) {
            self.selection.remove(i);
        } else {
            self.selection.push(e);
        }
        self.anchor = Some(e);
        self.selection_changed(before);
    }

    /// Selects the rows from the anchor to `to` in the tree's order, `to` active
    /// (Shift+click, Shift+arrows); with `add`, besides what is selected (Ctrl+Shift).
    fn select_range(&mut self, to: Entity, add: bool) {
        let before = self.active();
        let anchor = self.anchor.unwrap_or(to);
        let rows = &self.rows;
        let at = |e: Entity| rows.iter().position(|r| r.entity == e);
        let Some(b) = at(to) else {
            return self.select_only(to);
        };
        let a = at(anchor).unwrap_or(b);
        let range: Vec<Entity> = if a <= b {
            (a..=b).map(|i| rows[i].entity).collect()
        } else {
            (b..=a).rev().map(|i| rows[i].entity).collect()
        };
        if !add {
            self.selection.clear();
        }
        for e in range {
            self.selection.retain(|&s| s != e);
            self.selection.push(e);
        }
        self.anchor = Some(anchor);
        self.selection_changed(before);
    }

    /// After the selection changed: what belonged to the active entity goes, and its row
    /// is scrolled into view.
    fn selection_changed(&mut self, before: Option<Entity>) {
        if self.active() != before {
            self.open_choice = None;
            self.typing = None;
            self.drag = None;
        }
        self.reveal = self.active();
    }

    /// Moves the active row by `by` rows in the tree's order; with `extend`, the selection
    /// becomes the range from the anchor to it.
    fn step(&mut self, by: i64, extend: bool) {
        let last = self.rows.len() as i64 - 1;
        if last < 0 {
            return;
        }
        let at = self
            .active()
            .and_then(|e| self.rows.iter().position(|r| r.entity == e));
        let next = match at {
            Some(i) => (i as i64 + by).clamp(0, last),
            None if by > 0 => 0,
            None => last,
        };
        let e = self.rows[next as usize].entity;
        if extend {
            self.select_range(e, false);
        } else {
            self.select_only(e);
        }
    }

    /// The selected entities none of whose ancestors is selected: what Delete and Ctrl+D
    /// act on (a child goes with its parent).
    fn selection_roots(&self, world: &World) -> Vec<Entity> {
        self.selection
            .iter()
            .copied()
            .filter(|&e| {
                let mut at = world.parent(e);
                while let Some(p) = at {
                    if self.selection.contains(&p) {
                        return false;
                    }
                    at = world.parent(p);
                }
                world.contains(e)
            })
            .collect()
    }

    /// The scene's roots (entities with a scene id under none), as the tree shows them:
    /// by their order, those without one last in spawn order; `except` left out.
    fn roots(&mut self, world: &mut World, except: Option<Entity>) -> Vec<Entity> {
        let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        let world = &*world;
        let mut roots: Vec<Entity> = q
            .iter_readonly(world)
            .map(|(e, _)| e)
            .filter(|&e| {
                Some(e) != except
                    && world
                        .parent(e)
                        .is_none_or(|p| world.get::<SceneId>(p).is_none())
            })
            .collect();
        roots.sort_unstable_by_key(|&e| root_key(world, e));
        roots
    }

    /// An order for a new root after every other.
    fn last_root_order(&mut self, world: &mut World) -> u32 {
        let last = self
            .roots(world, None)
            .iter()
            .filter_map(|&e| world.get::<RootOrder>(e))
            .map(|o| o.0)
            .max()
            .unwrap_or(0);
        last.saturating_add(RootOrder::STEP)
    }

    /// An order for root `copy` placed right after root `after`: the middle of the gap to
    /// the next root, or, when there is none left, after numbering every root by steps
    /// again (each touched, inside the open history step).
    fn order_after(&mut self, world: &mut World, after: Entity, copy: Entity) -> u32 {
        let roots = self.roots(world, Some(copy));
        let order = |e: Entity| world.get::<RootOrder>(e).map_or(u32::MAX, |o| o.0);
        let Some(i) = roots.iter().position(|&r| r == after) else {
            return self.last_root_order(world);
        };
        let at = order(roots[i]);
        match roots.get(i + 1).map(|&n| order(n)) {
            None => at.saturating_add(RootOrder::STEP),
            Some(next) if next.saturating_sub(at) >= 2 => at + (next - at) / 2,
            Some(_) => {
                for (k, &r) in roots.iter().enumerate() {
                    self.history.touch(world, r);
                    world.insert(r, RootOrder((k as u32 + 1).saturating_mul(RootOrder::STEP)));
                }
                (i as u32 + 1)
                    .saturating_mul(RootOrder::STEP)
                    .saturating_add(RootOrder::STEP / 2)
            }
        }
    }

    /// A name for an entity under `parent` (a root for none), numbered when a sibling other
    /// than `except` has it.
    fn sibling_name(
        &mut self,
        world: &mut World,
        parent: Option<Entity>,
        base: &str,
        except: Option<Entity>,
    ) -> String {
        let siblings: Vec<Entity> = match parent {
            Some(p) => world.children(p).collect(),
            None => self.roots(world, None),
        };
        let taken: HashSet<&str> = siblings
            .iter()
            .filter(|&&s| Some(s) != except)
            .filter_map(|&s| world.get::<Name>(s).map(Name::as_str))
            .collect();
        unique_name(base, &taken)
    }

    /// Selects `e` and opens its parents' rows, so the tree shows it.
    fn show(&mut self, world: &World, e: Entity) {
        self.select_only(e);
        let mut at = world.parent(e);
        while let Some(p) = at {
            self.collapsed.remove(&p);
            at = world.parent(p);
        }
    }

    /// Adds an entity of the add menu's kind `what` where the camera looks (on the grid),
    /// and selects it.
    fn add(&mut self, world: &mut World, what: usize) {
        self.add_open = false;
        let step = self.snapping.position as f32;
        let at = (self.orbit.target / step).round() * step;
        let name = self.sibling_name(world, None, ADD[what], None);
        let order = self.last_root_order(world);
        self.history.begin(&format!("Add {}", ADD[what]), None);
        let e = spawn(world, what, &name, at);
        world.insert(e, RootOrder(order));
        self.history.touch_new(world, e);
        self.history.commit(world);
        self.select_only(e);
    }

    /// Runs a command.
    fn run(&mut self, world: &mut World, id: CommandId) {
        self.commands.ran(id);
        let ids = self.ids;
        if id == ids.undo || id == ids.redo {
            self.drag = None;
            self.open_choice = None;
            let selected: Vec<SceneId> = self
                .selection
                .iter()
                .filter_map(|&e| world.get::<SceneId>(e).copied())
                .collect();
            let done = if id == ids.undo {
                self.history.undo(world).map(|name| format!("Undo: {name}"))
            } else {
                self.history.redo(world).map(|name| format!("Redo: {name}"))
            };
            if let Some(message) = done {
                self.say(message);
            }
            self.reselect(world, &selected);
        } else if id == ids.save || id == ids.save_over_disk {
            let conflict = self.session.as_ref().is_some_and(|s| s.conflict);
            if id == ids.save && conflict {
                self.say(format!(
                    "Not saved: {} changed on disk since your unsaved changes; Take Disk \
                     Version or Save Over Disk Version (Ctrl+Shift+P) picks one",
                    self.file_name()
                ));
            } else if id == ids.save_over_disk && !conflict {
                self.say(String::from(
                    "Nothing to save over: the file on disk is the one your changes started \
                     from",
                ));
            } else {
                match self.save(world) {
                    Ok(path) => self.say(format!("Saved {}", path.display())),
                    Err(e) => self.say(format!("Not saved: {e}")),
                }
            }
        } else if id == ids.take_disk {
            self.take_disk_version(world);
        } else if id == ids.restore_autosave {
            self.pick_autosave(world);
        } else if id == ids.restart {
            self.restart(world);
        } else if id == ids.build_errors {
            self.pick_problem(world);
        } else if id == ids.build_now {
            match &self.builds {
                Some(builds) => builds.build_now(),
                None => self.say(String::from(NOT_BUILDING)),
            }
        } else if id == ids.restart_policy {
            self.pick_policy(world);
        } else if id == ids.delete {
            let roots = self.selection_roots(world);
            if !roots.is_empty() {
                self.history.begin("Delete", None);
                for e in roots {
                    self.history.delete_subtree(world, e);
                }
                self.history.commit(world);
                self.selection.clear();
                self.anchor = None;
            }
        } else if id == ids.duplicate {
            let roots = self.selection_roots(world);
            if !roots.is_empty() {
                // Each copy right after its original, numbered among its siblings.
                self.history.begin("Duplicate", None);
                let mut copies = Vec::with_capacity(roots.len());
                for e in roots {
                    let Some(copy) = self.history.duplicate_subtree(world, e) else {
                        continue;
                    };
                    let parent = world.parent(copy);
                    if parent.is_none_or(|p| world.get::<SceneId>(p).is_none()) {
                        let order = self.order_after(world, e, copy);
                        world.insert(copy, RootOrder(order));
                    }
                    let base = world.get::<Name>(copy).map(|n| n.as_str().to_owned());
                    if let Some(base) = base {
                        let name = self.sibling_name(world, parent, &base, Some(copy));
                        world.insert(copy, Name::new(name));
                    }
                    copies.push(copy);
                }
                self.history.commit(world);
                if !copies.is_empty() {
                    let before = self.active();
                    self.selection = copies;
                    self.anchor = self.active();
                    self.selection_changed(before);
                }
            }
        } else if id == ids.rename {
            self.renaming = self.active();
        } else if id == ids.select_next || id == ids.select_previous {
            self.step(if id == ids.select_next { 1 } else { -1 }, false);
        } else if id == ids.extend_next || id == ids.extend_previous {
            self.step(if id == ids.extend_next { 1 } else { -1 }, true);
        } else if id == ids.collapse || id == ids.expand || id == ids.expand_all {
            self.open_or_close(world, id);
        } else if id == ids.add {
            self.add_open = !self.add_open;
        } else if id == ids.snap {
            self.snapping.on = !self.snapping.on;
            let mode = if self.snapping.on {
                "snapping by default (Shift: free)"
            } else {
                "free by default (Shift: snapping)"
            };
            self.say(format!("Dragged values: {mode}"));
        } else if id == ids.palette {
            self.open_palette(world, "");
        } else if id == ids.find {
            self.open_palette(world, "@");
        } else if let Some(kind) = ids.add_kinds.iter().position(|&k| k == id) {
            self.add(world, kind);
        }
    }

    /// Sets the value at a scene path (`Transform.translation`, `Model.hidden`) of
    /// `entity` from scene text (`(3, 0, 0)`, `true`, `@<scene id>`), as one undoable step.
    fn set(
        &mut self,
        world: &mut World,
        entity: Entity,
        path: &str,
        value: &str,
    ) -> Result<(), String> {
        self.set_on(world, &[entity], path, value)
    }

    /// [`Editor::set`] for every entity of `targets`, as one undoable step: when one
    /// refuses the text, none changes.
    fn set_on(
        &mut self,
        world: &mut World,
        targets: &[Entity],
        path: &str,
        value: &str,
    ) -> Result<(), String> {
        let (component, rest) = engine_scene::text::component_of(path);
        let segs = engine_scene::text::segments(rest)?;
        let index: HashMap<SceneId, Entity> = {
            let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
            q.iter_readonly(world).map(|(e, &id)| (id, e)).collect()
        };
        let mut refs = IndexRefs { index: &index };
        self.history.begin(&format!("Set {path}"), None);
        let mut result = Ok(());
        for &entity in targets {
            let found = world.component_ids_of(entity).and_then(|ids| {
                ids.iter().find_map(|&id| {
                    let info = world.components().type_info(id)?;
                    (info.name == component).then_some((id, info))
                })
            });
            let Some((id, info)) = found else {
                result = Err(format!("the entity has no `{component}`"));
                break;
            };
            self.history.touch(world, entity);
            let Some(ptr) = world.get_raw_mut(entity, id) else {
                result = Err(String::from("the component is gone"));
                break;
            };
            let ptr = ptr.as_ptr();
            // SAFETY: `ptr` is the entity's live value of the component `info` describes;
            // the path and the text are checked against that description.
            result = unsafe {
                engine_scene::text::resolve(&segs, info, ptr, &mut refs).and_then(|target| {
                    let (ty, attrs, at) =
                        target.ok_or_else(|| format!("`{component}` has no `{rest}`"))?;
                    engine_scene::text::set_text(ty, &attrs, at, value, &mut refs)
                })
            };
            if result.is_err() {
                break;
            }
        }
        match result {
            Ok(()) => {
                self.history.commit(world);
                Ok(())
            }
            Err(e) => {
                self.history.cancel(world);
                Err(e)
            }
        }
    }

    /// Renames `e` (its `Name`, added if it has none) as one undoable step; nothing for an
    /// empty or unchanged name.
    fn rename(&mut self, world: &mut World, e: Entity, name: &str) {
        if name.is_empty() || world.get::<Name>(e).is_some_and(|n| n.as_str() == name) {
            return;
        }
        self.history.begin("Rename", None);
        self.history.touch(world, e);
        world.insert(e, Name::new(name));
        self.history.commit(world);
    }

    /// Selects the entities with scene ids `ids` again, in their order (undo and redo may
    /// have made them anew); those gone are left out.
    fn reselect(&mut self, world: &mut World, ids: &[SceneId]) {
        let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        let index: HashMap<SceneId, Entity> =
            q.iter_readonly(world).map(|(e, &id)| (id, e)).collect();
        let before = self.active();
        self.selection = ids.iter().filter_map(|id| index.get(id).copied()).collect();
        if self.anchor.is_none_or(|a| !self.selection.contains(&a)) {
            self.anchor = self.active();
        }
        self.selection_changed(before);
    }

    /// Left, Right and Shift+Right in the tree: closes the active row (or goes to its
    /// parent), opens it (or goes to its first child), or opens its whole branch.
    fn open_or_close(&mut self, world: &World, id: CommandId) {
        let Some(e) = self.active() else {
            return;
        };
        let has_children = world.children(e).any(|c| world.get::<SceneId>(c).is_some());
        if id == self.ids.expand_all {
            let mut stack = vec![e];
            while let Some(x) = stack.pop() {
                self.collapsed.remove(&x);
                stack.extend(world.children(x));
            }
        } else if id == self.ids.collapse {
            if has_children && !self.collapsed.contains(&e) {
                self.collapsed.insert(e);
            } else if let Some(p) = world
                .parent(e)
                .filter(|&p| world.get::<SceneId>(p).is_some())
            {
                self.select_only(p);
            }
        } else if has_children && self.collapsed.remove(&e) {
            self.reveal = Some(e);
        } else if let Some(child) = world
            .children(e)
            .find(|&c| world.get::<SceneId>(c).is_some())
        {
            self.select_only(child);
        }
    }

    /// Saves the scene to its file (`untitled.scene` in the project for a new one): every
    /// entity with a scene id, under the scene's own id, asset references by path. Written
    /// beside the file, then moved over it, so a failure leaves the old file whole.
    fn save(&mut self, world: &mut World) -> Result<PathBuf, String> {
        let text = self.scene_text(world)?;
        let relative = match &self.scene_path {
            Some(p) => p.clone(),
            None => untitled(&self.root),
        };
        let path = self.root.join(&relative);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let partial = path.with_extension("scene.partial");
        std::fs::write(&partial, &text).map_err(|e| format!("{}: {e}", partial.display()))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.scene_path = Some(relative);
        self.saved_head = self.history.head();
        if let Some(s) = &mut self.session {
            s.base = session::modified(&path).map(|t| Base::of_bytes(text.as_bytes(), t));
            s.conflict = false;
            s.due = None;
            // A scene saved for the first time: its autosave had the untitled name.
            let named = s.dir.join(session::autosave_name(
                self.scene_path.as_deref(),
                self.scene_id,
            ));
            let old = std::mem::replace(&mut s.autosave, named);
            if s.autosaved.take().is_some() {
                s.writer
                    .remove(vec![session::older(&old, 1), session::older(&old, 2), old]);
            }
        }
        self.write_session(world);
        Ok(path)
    }

    /// The scene's file name, for messages: `untitled` before its first save.
    fn file_name(&self) -> String {
        self.scene_path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(
                || String::from("untitled"),
                |n| n.to_string_lossy().into_owned(),
            )
    }

    /// The scene's text, as Ctrl+S and an autosave write it: every entity with a scene id,
    /// under the scene's own id, asset references by path.
    fn scene_text(&self, world: &mut World) -> Result<String, String> {
        let registry = SceneRegistry::from_program();
        let mut text = String::new();
        let options = SaveOptions {
            id: Some(self.scene_id),
            assets: Some(&self.assets),
        };
        engine_scene::save_with(world, &registry, &options, &mut text)
            .map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Starts the session in the project's `.ofelia/`: takes its lock, then brings back the
    /// scene's unsaved changes from their autosave as the last run left them, at its exit
    /// or its crash, without asking. Returns the asset paths the restored scene named.
    fn begin_session(&mut self, world: &mut World) -> Result<AssetPaths, Error> {
        let dir = self.root.join(session::DIR);
        let mut tries = 0;
        let mut timer = None;
        let (mut lock, last) = loop {
            match Lock::take(&dir) {
                Ok(taken) => break taken,
                // A restart's new editor: the last one is letting go of the project.
                Err(e) if self.restarted_at.is_some() && e.contains("another editor") => {
                    tries += 1;
                    if tries > 200 {
                        return Err(e.into());
                    }
                    if timer.is_none() {
                        timer = engine_platform::time::HighResTimer::new().ok();
                    }
                    if let Some(t) = &timer {
                        let _ = t.sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        };
        let file = self.scene_path.as_ref().map(|p| self.root.join(p));
        let mut s = Session {
            lock: None,
            writer: Writer::new()?,
            base: file.as_deref().and_then(Base::of),
            autosave: PathBuf::new(),
            autosaved: None,
            due: None,
            last_input: Instant::now(),
            conflict: false,
            restoring_until: None,
            last_text: String::new(),
            view_due: Instant::now() + session::VIEW_EVERY,
            dir,
        };
        let mut learned = AssetPaths::new();
        let mut note = None;
        let previous = s.dir.join("session");
        let mut editor_state = None;
        let doc = match std::fs::read_to_string(&previous) {
            Err(_) => None,
            Ok(text) => match SessionFile::parse(&text) {
                Ok(f) => {
                    editor_state = Some(f.editor);
                    f.documents.into_iter().find(|d| d.path == self.scene_path)
                }
                Err(e) => {
                    let _ = std::fs::rename(&previous, s.dir.join("session.broken"));
                    note = Some(format!(
                        "The session could not be read ({e}); it is kept as \
                         .ofelia/session.broken"
                    ));
                    None
                }
            },
        };
        let autosave = doc
            .as_ref()
            .and_then(|d| d.autosave.as_ref())
            .map(|a| s.dir.join(a))
            .filter(|a| a.is_file());
        if let (Some(doc), Some(autosave)) = (&doc, autosave) {
            let name = self.file_name();
            if last == LastRun::CrashedRestoring {
                note = Some(format!(
                    "The last start stopped while it brought back the unsaved changes of \
                     {name}: it opened as saved, and they are kept in {}",
                    autosave.display()
                ));
            } else {
                lock.mark("restoring")?;
                s.restoring_until = Some(Instant::now() + session::RESTORING);
                // What the changes were made from, against the file now: none for a scene
                // never saved.
                let disk = file
                    .as_deref()
                    .map(|f| (f.is_file(), doc.base.and_then(|b| b.matches(f))));
                match self.restore(world, &autosave) {
                    Err(e) => {
                        note = Some(format!(
                            "The unsaved changes of {name} could not be read ({e}); they are \
                             kept in {}",
                            autosave.display()
                        ));
                    }
                    Ok(paths) => {
                        learned = paths;
                        self.saved_head = u64::MAX;
                        s.autosaved = Some(self.history.head());
                        let when = session::modified(&autosave)
                            .map_or_else(String::new, |t| format!(" from {}", ago(t)));
                        let restored = if last == LastRun::Crashed {
                            format!("After a crash: restored the unsaved changes of {name}{when}")
                        } else {
                            format!("Restored the unsaved changes of {name}{when}")
                        };
                        note = Some(match disk {
                            None | Some((true, Some(true))) => restored,
                            Some((false, _)) => {
                                format!(
                                    "{restored}; its file is gone from the disk, Ctrl+S writes it"
                                )
                            }
                            Some((true, _)) => {
                                s.conflict = true;
                                format!("{restored}; its file changed on disk since")
                            }
                        });
                    }
                }
            }
        }
        s.lock = Some(lock);
        // By the scene's id after a restore (an untitled scene's comes from its autosave).
        s.autosave = s.dir.join(session::autosave_name(
            self.scene_path.as_deref(),
            self.scene_id,
        ));
        self.session = Some(s);
        // How the editor and the scene were looked at: back too.
        if let Some(state) = &editor_state {
            self.apply_editor_state(state);
        }
        if let Some(doc) = &doc {
            self.apply_view(world, &doc.view);
        }
        self.write_session(world);
        if let Some(note) = note {
            self.say(note);
        }
        Ok(learned)
    }

    /// The editor's own state from a session: snapping, and the order of recently run
    /// commands (the palette ranks them first).
    fn apply_editor_state(&mut self, state: &EditorState) {
        if let Some((on, [position, scale, angle, number])) = state.snapping {
            self.snapping.on = on;
            self.snapping.position = position;
            self.snapping.scale = scale;
            self.snapping.angle = angle;
            self.snapping.number = number;
        }
        for name in &state.recent {
            if let Some(id) = self.commands.find(name) {
                self.commands.ran(id);
            }
        }
        if let Some(policy) = state.restart.as_deref().and_then(RestartPolicy::from_name) {
            self.restart_policy = policy;
        }
    }

    /// A scene's view from a session: the camera, the selection by scene ids (those gone
    /// left out), the tree's closed rows, the filters, the tool.
    fn apply_view(&mut self, world: &mut World, view: &View) {
        if let Some(([x, y, z], yaw, pitch, distance)) = view.camera {
            self.orbit.target = Vec3::new(x, y, z);
            self.orbit.yaw = yaw;
            self.orbit.pitch = pitch;
            self.orbit.distance = distance;
        }
        let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        let index: HashMap<SceneId, Entity> =
            q.iter_readonly(world).map(|(e, &id)| (id, e)).collect();
        self.collapsed = view
            .collapsed
            .iter()
            .filter_map(|id| index.get(id).copied())
            .collect();
        self.selection = view
            .selection
            .iter()
            .filter_map(|id| index.get(id).copied())
            .collect();
        self.anchor = view
            .anchor
            .and_then(|id| index.get(&id).copied())
            .filter(|a| self.selection.contains(a))
            .or_else(|| self.active());
        self.reveal = self.active();
        self.tree_filter.clone_from(&view.tree_filter);
        self.inspector_filter.clone_from(&view.inspector_filter);
        self.active_tool = view.tool.min(self.tools.len() - 1);
    }

    /// The scene's view as a session keeps it.
    fn view(&self, world: &World) -> View {
        let id = |e: Entity| world.get::<SceneId>(e).copied();
        let mut collapsed: Vec<SceneId> = self.collapsed.iter().filter_map(|&e| id(e)).collect();
        // A set has no order of its own: sorted, the text stays the same while it does.
        collapsed.sort_unstable();
        View {
            camera: Some((
                self.orbit.target.to_array(),
                self.orbit.yaw,
                self.orbit.pitch,
                self.orbit.distance,
            )),
            selection: self.selection.iter().filter_map(|&e| id(e)).collect(),
            anchor: self.anchor.and_then(id),
            collapsed,
            tree_filter: self.tree_filter.clone(),
            inspector_filter: self.inspector_filter.clone(),
            tool: self.active_tool,
        }
    }

    /// Puts the scene of `autosave` in the world in place of the scene's entities. The
    /// world is left as it was when the file cannot be read.
    fn restore(&mut self, world: &mut World, autosave: &Path) -> Result<AssetPaths, String> {
        let text = std::fs::read_to_string(autosave).map_err(|e| e.to_string())?;
        let old = self.roots(world, None);
        let report = engine_scene::load(world, &SceneRegistry::from_program(), &text)
            .map_err(|e| e.to_string())?;
        for (line, warning) in &report.warnings {
            log::warn!(target: "engine_editor", "{}:{line}: {warning}", autosave.display());
        }
        for e in old {
            world.despawn_recursive(e);
        }
        if let Some(id) = report.id {
            self.scene_id = id;
        }
        self.assets.extend(&report.assets);
        Ok(report.assets)
    }

    /// Writes the session file on the writer's thread, unless it would be the same text:
    /// the editor's state, then the scene, what its file was, its autosave while it has
    /// one and its view.
    fn write_session(&mut self, world: &World) {
        if self.session.is_none() {
            return;
        }
        let recent = self.commands.recent();
        let editor = EditorState {
            snapping: Some((
                self.snapping.on,
                [
                    self.snapping.position,
                    self.snapping.scale,
                    self.snapping.angle,
                    self.snapping.number,
                ],
            )),
            recent: recent[recent.len().saturating_sub(session::RECENT)..]
                .iter()
                .map(|&id| self.commands.info(id).name.clone())
                .collect(),
            restart: Some(String::from(self.restart_policy.name())),
            window: self.window,
        };
        let view = self.view(world);
        let Some(s) = &mut self.session else {
            return;
        };
        let file = SessionFile {
            editor,
            documents: vec![Document {
                path: self.scene_path.clone(),
                id: self.scene_id,
                base: s.base,
                autosave: s
                    .autosaved
                    .map(|_| session::autosave_name(self.scene_path.as_deref(), self.scene_id)),
                view,
            }],
        };
        let text = file.to_text();
        if text != s.last_text {
            s.writer
                .write(s.dir.join("session"), text.clone(), 0, Duration::ZERO);
            s.last_text = text;
        }
    }

    /// A gesture runs: a drag, typing, a held button. Autosaves wait for its end.
    fn gesture(&self) -> bool {
        self.drag.is_some()
            || self.typing.is_some()
            || self.renaming.is_some()
            || self.camera_drag
            || self.ui.editing().is_some()
            || [Button::Left, Button::Right, Button::Middle]
                .into_iter()
                .any(|b| self.input.held(b))
    }

    /// The session's part of the frame: the writer's failures told, the lock's state moved
    /// on, the autosave kept in step with the unsaved changes.
    fn tick_session(&mut self, world: &mut World) {
        let Some(s) = &mut self.session else {
            return;
        };
        let mut failed = None;
        for w in s.writer.finished() {
            if let Err(e) = w.result {
                // Written again at its next time.
                if w.path == s.autosave {
                    s.autosaved = None;
                }
                failed = Some(e);
            }
        }
        if s.restoring_until.is_some_and(|t| Instant::now() >= t) {
            s.restoring_until = None;
            if let Some(lock) = &mut s.lock {
                let _ = lock.mark("running");
            }
        }
        if let Some(e) = failed {
            self.say(format!("Not autosaved: {e}"));
        }
        self.keep_autosave(world, false);
        // The view (the camera, the selection, the tree) when it changed, now and then.
        let now = Instant::now();
        let gesture = self.gesture();
        if let Some(s) = &mut self.session
            && session::autosave_now(now, s.view_due, s.last_input, gesture)
        {
            s.view_due = now + session::VIEW_EVERY;
            self.write_session(world);
        }
    }

    /// Keeps the scene's autosave in step with its unsaved changes: written once they have
    /// waited `session::AUTOSAVE_AFTER` and a quiet moment came (at once with `now`), and
    /// deleted with the older ones once the scene is clean again.
    fn keep_autosave(&mut self, world: &mut World, now: bool) {
        let head = self.history.head();
        let gesture = self.gesture();
        let Some(s) = &mut self.session else {
            return;
        };
        if head == self.saved_head {
            s.due = None;
            if s.autosaved.take().is_some() {
                let path = &s.autosave;
                s.writer.remove(vec![
                    session::older(path, 1),
                    session::older(path, 2),
                    path.clone(),
                ]);
                self.write_session(world);
            }
            return;
        }
        if s.autosaved == Some(head) {
            s.due = None;
            return;
        }
        let at = Instant::now();
        let due = *s.due.get_or_insert(at + session::AUTOSAVE_AFTER);
        if now || session::autosave_now(at, due, s.last_input, gesture) {
            self.autosave(world);
        }
    }

    /// Writes the scene's unsaved changes into its autosave (the newest of three) and the
    /// session after it. Only their text is made here; the writer's thread writes them.
    fn autosave(&mut self, world: &mut World) {
        let text = match self.scene_text(world) {
            Ok(text) => text,
            Err(e) => {
                if let Some(s) = &mut self.session {
                    s.due = None;
                }
                self.say(format!("Not autosaved: {e}"));
                return;
            }
        };
        let head = self.history.head();
        let Some(s) = &mut self.session else {
            return;
        };
        s.writer.write(
            s.autosave.clone(),
            text,
            session::OLDER_AUTOSAVES,
            session::AUTOSAVE_AFTER,
        );
        s.autosaved = Some(head);
        s.due = None;
        self.write_session(world);
    }

    /// Autosaves the unsaved changes now and waits until the session is on the disk:
    /// before Play and before a restart.
    fn save_session(&mut self, world: &mut World) {
        self.keep_autosave(world, true);
        if let Some(s) = &self.session {
            s.writer.flush();
        }
    }

    /// Ends the session at a clean exit: the unsaved changes autosaved, every write on the
    /// disk, the lock released.
    fn end_session(&mut self, world: &mut World) {
        if self.session.is_none() {
            return;
        }
        self.keep_autosave(world, true);
        self.write_session(world);
        if let Some(mut s) = self.session.take() {
            s.writer.flush();
            if let Some(lock) = s.lock.take() {
                lock.release();
            }
            self.ended = Some((s.base, s.autosave, s.autosaved, s.conflict));
        }
    }

    /// Goes on with the session ended for a restart whose new build failed at its start:
    /// the lock taken again (the failed one may still be letting go of it), a new writer,
    /// the session written as this editor has it.
    fn resume_session(&mut self, world: &mut World) -> Result<(), Error> {
        let Some((base, autosave, autosaved, conflict)) = self.ended.take() else {
            return Ok(());
        };
        let dir = self.root.join(session::DIR);
        let mut tries = 0;
        let timer = engine_platform::time::HighResTimer::new()?;
        let lock = loop {
            match Lock::take(&dir) {
                Ok((lock, _)) => break lock,
                Err(e) if tries < 100 && e.contains("another editor") => {
                    tries += 1;
                    let _ = timer.sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        };
        let now = Instant::now();
        self.session = Some(Session {
            dir,
            lock: Some(lock),
            writer: Writer::new()?,
            base,
            autosave,
            autosaved,
            due: None,
            last_input: now,
            conflict,
            restoring_until: None,
            last_text: String::new(),
            view_due: now + session::VIEW_EVERY,
        });
        self.write_session(world);
        Ok(())
    }

    /// The scene's file in place of its unsaved changes, after it changed on disk since
    /// they were made: one undo step (Ctrl+Z brings them back), their autosave kept aside
    /// as `….conflict.scene`.
    fn take_disk_version(&mut self, world: &mut World) {
        if !self.session.as_ref().is_some_and(|s| s.conflict) {
            self.say(String::from(
                "Nothing to take: the file on disk is the one your changes started from",
            ));
            return;
        }
        let Some(file) = self.scene_path.as_ref().map(|p| self.root.join(p)) else {
            return;
        };
        if let Err(e) = self.replace_scene(world, &file, "Take Disk Version") {
            self.say(e);
            return;
        }
        self.saved_head = self.history.head();
        if let Some(s) = &mut self.session {
            s.conflict = false;
            s.base = Base::of(&file);
            if s.autosaved.take().is_some() {
                let aside = s.autosave.with_extension("conflict.scene");
                s.writer.rename(s.autosave.clone(), aside);
            }
        }
        self.write_session(world);
        self.say(String::from(
            "Took the version on disk; Ctrl+Z brings your changes back",
        ));
    }

    /// The scene's entities replaced by the scene in `file`, as one undo step named
    /// `step`.
    ///
    /// The scene's entities are recorded and gone first: the history knows entities by
    /// their scene ids, so the file's entities loaded after are the same entities to it
    /// where their ids are the same, and the others are new.
    fn replace_scene(&mut self, world: &mut World, file: &Path, step: &str) -> Result<(), String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let old = self.roots(world, None);
        self.history.begin(step, None);
        for e in old {
            self.history.delete_subtree(world, e);
        }
        let report = match engine_scene::load(world, &SceneRegistry::from_program(), &text) {
            Ok(report) => report,
            Err(e) => {
                self.history.cancel(world);
                return Err(format!("{}: {e}", file.display()));
            }
        };
        for &e in &report.entities {
            self.history.touch_new(world, e);
        }
        self.history.commit(world);
        self.selection.clear();
        self.anchor = None;
        self.assets.extend(&report.assets);
        self.pending_assets.extend(&report.assets);
        Ok(())
    }

    /// Starts building the game's code on changes (E1-5c) when the editor runs a slot's
    /// copy of the game, as `ofeliaengine edit` starts it: `cargo run` runs cargo's own
    /// file, which a build cannot write over while it runs.
    fn begin_builds(&mut self, game: GameCrate) {
        let running = std::env::current_exe().unwrap_or_default();
        if !in_slot(&game, &running) {
            self.say(String::from(NOT_BUILDING));
            self.game_crate = Some(game);
            return;
        }
        let artifact = std::env::var_os(BUILD_VAR).map(PathBuf::from);
        match Builds::start(&game, running, artifact) {
            Ok(builds) => self.builds = Some(builds),
            Err(e) => self.say(format!("Code changes are not built: {e}")),
        }
        self.game_crate = Some(game);
    }

    /// The builds' part of the frame: what the builder did, and a new build taken when
    /// the restart policy says so.
    fn tick_builds(&mut self, world: &mut World) {
        if let Some(at) = self.restarted_at.take() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let took = now.saturating_sub(at) as f64 / 1e9;
            self.say(format!("Restarted with the new build in {took:.2} s"));
        }
        let events: Vec<BuildEvent> = self
            .builds
            .as_ref()
            .map(|b| b.events().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                BuildEvent::Started => self.build_state = BuildState::Building(Instant::now()),
                BuildEvent::Finished(built) => self.finished_build(built),
            }
        }
        if self.ready.is_some()
            && self.restart_with.is_none()
            && self.restarts_by_itself()
            && !self.gesture()
        {
            self.restart(world);
        }
    }

    /// Whether a new build restarts the editor without being asked.
    fn restarts_by_itself(&self) -> bool {
        match self.restart_policy {
            RestartPolicy::Background => !self.focused,
            RestartPolicy::Always => true,
            RestartPolicy::Ask => false,
        }
    }

    fn finished_build(&mut self, built: Built) {
        if built.success {
            self.problems.clear();
            self.build_state = BuildState::Idle;
            let took = built.took.as_secs_f32();
            match built.exe {
                Some(exe) => {
                    self.ready = Some((exe, built.artifact));
                    self.say(format!("Built in {took:.1} s"));
                }
                None => self.say(format!("Built in {took:.1} s: nothing changed")),
            }
            return;
        }
        let game = self
            .game_crate
            .as_ref()
            .map_or_else(|| self.root.clone(), |g| g.manifest_dir.clone());
        let n = built.errors.len();
        let text = match (built.errors.first(), &built.failure) {
            (Some(first), _) => format!(
                "Build failed: {n} error{}, {}: {}",
                if n == 1 { "" } else { "s" },
                first.location(&game),
                first.message
            ),
            (None, Some(failure)) => format!("Build failed: {failure}"),
            (None, None) => String::from("Build failed"),
        };
        log::warn!(target: "engine_editor", "{text}");
        self.problems = built.errors;
        self.build_state = BuildState::Failed(text);
    }

    /// Restarts with the new build: the session saved (the unsaved changes, the view),
    /// then the frame loop ends and the build starts in a new process (`run_game`).
    fn restart(&mut self, world: &mut World) {
        let Some(ready) = self.ready.take() else {
            self.say(String::from(if self.builds.is_some() {
                "No new build to restart with yet"
            } else {
                NOT_BUILDING
            }));
            return;
        };
        self.save_session(world);
        self.restart_with = Some(ready);
    }

    /// "Build: Show Errors…": the last build's errors in the palette.
    fn pick_problem(&mut self, world: &mut World) {
        if self.problems.is_empty() {
            self.say(String::from("The last build had no errors"));
            return;
        }
        let game = self
            .game_crate
            .as_ref()
            .map_or_else(|| self.root.clone(), |g| g.manifest_dir.clone());
        let items = self
            .problems
            .iter()
            .enumerate()
            .map(|(i, p)| Choice {
                label: p.location(&game),
                detail: p.message.clone(),
                pick: Pick::Problem(i),
            })
            .collect();
        self.open_list(world, "Pick an error to open it in VS Code", items);
    }

    /// Opens an error at its line in VS Code (`code -g`); without VS Code, its place goes
    /// to the clipboard.
    fn open_problem(&mut self, index: usize) {
        let Some(problem) = self.problems.get(index) else {
            return;
        };
        let Some((path, line, column)) = &problem.at else {
            let message = problem.message.clone();
            self.say(message);
            return;
        };
        let place = format!("{}:{line}:{column}", path.display());
        let mut command = std::process::Command::new("code.cmd");
        command.args(["-g", &place]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: the batch file's console stays hidden.
            command.creation_flags(0x0800_0000);
        }
        if command.spawn().is_ok() {
            self.say(format!("Opened {place} in VS Code"));
        } else {
            self.input.clipboard.set_text(&place);
            self.say(format!("VS Code (`code`) is not on PATH: copied {place}"));
        }
    }

    /// "Editor: Restart Policy…": what a new build does, in the palette.
    fn pick_policy(&mut self, world: &mut World) {
        let items = RestartPolicy::ALL
            .into_iter()
            .map(|p| Choice {
                label: String::from(p.label()),
                detail: String::from(if p == self.restart_policy { "now" } else { "" }),
                pick: Pick::Policy(p),
            })
            .collect();
        self.open_list(world, "What a new build of the game's code does", items);
    }

    fn set_restart_policy(&mut self, world: &mut World, policy: RestartPolicy) {
        self.restart_policy = policy;
        self.write_session(world);
        self.say(String::from(policy.label()));
    }

    /// Opens the palette as a list to pick from.
    fn open_list(&mut self, world: &mut World, hint: &'static str, items: Vec<Choice>) {
        self.open_palette(world, "");
        if let Some(cp) = &mut self.command_palette {
            cp.choices = Some(Choices {
                hint,
                items,
                shown: Vec::new(),
            });
        }
        self.search_palette(world);
    }

    /// "Restore Unsaved Changes…": the scene's autosaves in the palette, the newest first
    /// (the latest, the two older, one kept after a conflict).
    fn pick_autosave(&mut self, world: &mut World) {
        let name = self.file_name();
        let Some(s) = &self.session else {
            self.say(String::from(
                "No session: the editor keeps no autosaves here",
            ));
            return;
        };
        let latest = &s.autosave;
        let mut found: Vec<(u64, PathBuf, &'static str)> = [
            (latest.clone(), "the latest autosave"),
            (session::older(latest, 1), "an older autosave"),
            (session::older(latest, 2), "an older autosave"),
            (
                latest.with_extension("conflict.scene"),
                "kept when the disk version was taken",
            ),
        ]
        .into_iter()
        .filter_map(|(path, kind)| Some((session::modified(&path)?, path, kind)))
        .collect();
        if found.is_empty() {
            self.say(format!("No autosaves of {name}"));
            return;
        }
        found.sort_unstable_by_key(|&(time, _, _)| core::cmp::Reverse(time));
        let items = found
            .into_iter()
            .map(|(time, path, kind)| Choice {
                label: format!("{name} from {}", ago(time)),
                detail: String::from(kind),
                pick: Pick::Autosave(path),
            })
            .collect();
        self.open_list(world, "Pick the unsaved changes to bring back", items);
    }

    /// The scene of a picked autosave in place of the scene, as one undo step: Ctrl+Z
    /// takes it back.
    fn restore_autosave(&mut self, world: &mut World, autosave: &Path) {
        let when =
            session::modified(autosave).map_or_else(String::new, |t| format!(" from {}", ago(t)));
        match self.replace_scene(world, autosave, "Restore Unsaved Changes") {
            Ok(()) => self.say(format!(
                "Brought back the unsaved changes of {}{when}; Ctrl+Z takes them back",
                self.file_name()
            )),
            Err(e) => self.say(format!("Not restored: {e}")),
        }
    }
}

/// The game the editor wraps: in edit mode nothing of it runs (Play comes with phase E3).
pub struct EditorGame<G> {
    shared: Rc<RefCell<Editor>>,
    /// The game while playing.
    game: Option<G>,
    /// How the game is made when Play starts.
    #[allow(dead_code)]
    start: Start<G>,
}

impl<G> EditorGame<G> {
    /// The viewport panel in pixels, for the player's next frame
    /// (`Player::set_view_rect`).
    pub fn view_rect(&self) -> Option<[u32; 4]> {
        self.shared.borrow().view_rect
    }

    /// Gives the player the asset paths of scenes the editor loaded, and learns the
    /// player's again when they changed (a scene loaded since): the inspector shows
    /// references by path.
    pub fn sync_assets(&mut self, player: &mut Player) {
        let mut shared = self.shared.borrow_mut();
        if !shared.pending_assets.is_empty() {
            player.assets_mut().add_paths(&shared.pending_assets);
            shared.pending_assets = AssetPaths::new();
        }
        let paths = player.assets().paths();
        if shared.assets.len() != paths.len() {
            shared.assets = paths.clone();
        }
    }

    /// Starts the session in the project's `.ofelia/` (ADR 0107 §4): takes the project's
    /// lock, and brings back the scene's unsaved changes from their autosave, as the last
    /// run left them at its exit or its crash, without asking. From then on unsaved changes
    /// are autosaved there after 30 s, at a quiet moment, never over the scene's file.
    /// [`run`] calls it; an editor made with [`editor`] has no session until it does.
    ///
    /// # Errors
    ///
    /// Another editor has the project open, or `.ofelia/` cannot be written.
    pub fn begin_session(&mut self, player: &mut Player) -> Result<(), Error> {
        let learned = self.shared.borrow_mut().begin_session(player.world_mut())?;
        player.assets_mut().add_paths(&learned);
        Ok(())
    }

    /// Autosaves the unsaved changes now and waits until the session is on the disk:
    /// before Play and before a restart.
    pub fn save_session(&mut self, world: &mut World) {
        self.shared.borrow_mut().save_session(world);
    }

    /// Builds the game's code on changes while the editor runs a slot's copy of the game
    /// (`run_game` calls it).
    pub fn begin_builds(&mut self, game: GameCrate) {
        self.shared.borrow_mut().begin_builds(game);
    }

    /// The editor is restarting with a new build: its frame loop ends.
    pub fn quitting(&self) -> bool {
        self.shared.borrow().restart_with.is_some()
    }

    /// The build to restart with, once the frame loop has ended: its slot's executable,
    /// and cargo's path of it.
    pub fn restart_with(&mut self) -> Option<(PathBuf, Option<PathBuf>)> {
        self.shared.borrow_mut().restart_with.take()
    }

    /// Whether the editor's window has the keyboard.
    pub fn focused(&self) -> bool {
        self.shared.borrow().focused
    }

    /// Where the editor's window is (`Player::window_placement`): the session keeps it, so
    /// the next start and a restart's new editor open there.
    pub fn note_window(&mut self, placement: Option<engine_platform::window::WindowPlacement>) {
        if placement.is_some() {
            self.shared.borrow_mut().window = placement;
        }
    }

    /// Goes on with the session after a restart whose new build failed at its start.
    ///
    /// # Errors
    ///
    /// When the project's lock or the writer cannot be had again.
    pub fn resume_session(&mut self, player: &mut Player) -> Result<(), Error> {
        self.shared.borrow_mut().resume_session(player.world_mut())
    }

    /// Tells that a restart failed: the status bar keeps it until the next build.
    pub fn restart_failed(&mut self, why: String) {
        let mut shared = self.shared.borrow_mut();
        let text = format!("Not restarted: {why}; this editor goes on");
        log::warn!(target: "engine_editor", "{text}");
        shared.build_state = BuildState::Failed(text);
    }

    /// Ends the session at a clean exit: the unsaved changes autosaved, every write on the
    /// disk, the project's lock released. Nothing is asked: the next start brings the
    /// changes back. [`run`] calls it when the window closes.
    pub fn end_session(&mut self, world: &mut World) {
        self.shared.borrow_mut().end_session(world);
    }

    /// The active entity of the selection: the last picked, which the inspector shows.
    pub fn selected(&self) -> Option<Entity> {
        self.shared.borrow().active()
    }

    /// The selected entities, in the order they were picked (the active one last).
    pub fn selection(&self) -> Vec<Entity> {
        self.shared.borrow().selection.clone()
    }

    /// Selects an entity alone (or nothing), as a click in the tree does.
    pub fn select(&mut self, entity: Option<Entity>) {
        let mut shared = self.shared.borrow_mut();
        match entity {
            Some(e) => shared.select_only(e),
            None => {
                shared.selection.clear();
                shared.anchor = None;
            }
        }
    }

    /// Runs an editor command by its stable name (`edit.undo`, `edit.redo`, `file.save`,
    /// `scene.delete`, `scene.duplicate`, `scene.add`, `scene.add.cube` and the add menu's
    /// other kinds, `scene.rename`, `scene.find`, `scene.select_next` and the tree's other
    /// moves, `editor.palette`, `edit.snap`), as its shortcut does. False if there is no
    /// such command.
    pub fn run_command(&mut self, world: &mut World, name: &str) -> bool {
        let mut shared = self.shared.borrow_mut();
        let Some(id) = shared.commands.find(name) else {
            return false;
        };
        shared.run(world, id);
        true
    }

    /// Adds an entity of the add menu's `index`-th kind (`Empty`, `Cube`, `Sphere`,
    /// `Plane`, `Cylinder`, `Sun`, `Point Light`, `Spot Light`, `Camera`), as the menu does.
    pub fn add(&mut self, world: &mut World, index: usize) {
        if index < ADD.len() {
            self.shared.borrow_mut().add(world, index);
        }
    }

    /// The scene's file, relative to the project; none while the scene is untitled.
    pub fn scene_path(&self) -> Option<PathBuf> {
        self.shared.borrow().scene_path.clone()
    }

    /// Sets the value at a scene path of `entity` (`Transform.translation`) from scene
    /// text (`(3, 0, 0)`), as one undoable step: what the inspector does by dragging, for
    /// scripts, tests and the command line.
    ///
    /// # Errors
    ///
    /// No such component or field, or text that is not a value of the field's type (the
    /// world is left as it was).
    pub fn set(
        &mut self,
        world: &mut World,
        entity: Entity,
        path: &str,
        value: &str,
    ) -> Result<(), String> {
        self.shared.borrow_mut().set(world, entity, path, value)
    }

    /// Whether the scene has changes not saved yet.
    pub fn unsaved(&self) -> bool {
        let shared = self.shared.borrow();
        shared.history.head() != shared.saved_head
    }
}

impl<G: Game> Game for EditorGame<G> {
    fn event(&mut self, event: &WindowEvent) {
        self.shared.borrow_mut().event(event);
        if let Some(game) = &mut self.game {
            game.event(event);
        }
    }

    fn tick(&mut self, world: &mut World, dt: f32) {
        if let Some(game) = &mut self.game {
            game.tick(world, dt);
        }
    }

    fn present(&mut self, world: &mut World, alpha: f32) {
        if let Some(game) = &mut self.game {
            game.present(world, alpha);
        }
        self.shared.borrow_mut().update(world);
    }
}

/// Paints the editor's shell over the player's frame.
pub struct EditorOverlay {
    shared: Rc<RefCell<Editor>>,
    renderer: Renderer2d,
    glyphs: GlyphTexture,
    canvas: Canvas,
}

impl EditorOverlay {
    /// Frees the GPU objects once the device is idle; call before the player goes.
    ///
    /// # Errors
    ///
    /// When waiting for the device fails.
    pub fn destroy(mut self, player: &mut Player) -> Result<(), Error> {
        player.device().wait_idle()?;
        let setup = player.setup();
        // SAFETY: the device is idle; nothing uses these objects again.
        unsafe {
            self.renderer.destroy(setup.allocator);
            self.glyphs
                .texture_mut()
                .destroy(setup.device, setup.allocator, setup.bindless, 0);
        }
        Ok(())
    }
}

impl Overlay for EditorOverlay {
    fn record<'g>(&'g mut self, pass: &mut OverlayPass<'_, 'g>) -> Result<(), AppError> {
        {
            let mut shared = self.shared.borrow_mut();
            let editor = &mut *shared;
            self.canvas.clear();
            paint(
                &editor.ui.last_frame(),
                &mut editor.text,
                &self.glyphs,
                &mut self.canvas,
                editor.scale.max(0.5),
            );
            self.glyphs
                .sync(pass.fg, pass.ring, &mut editor.text.atlas)
                .map_err(AppError::at("the editor's glyphs"))?;
        }
        self.canvas.prepare();
        self.renderer
            .add_pass(
                pass.fg,
                pass.ring,
                pass.bindless,
                &self.canvas,
                &View2d::pixels(pass.extent.width, pass.extent.height),
                pass.target,
                None,
            )
            .map_err(AppError::at("the editor's panels"))
    }

    fn output_changed(
        &mut self,
        setup: &mut Setup<'_>,
        output: &OverlayOutput,
    ) -> Result<(), AppError> {
        let mut renderer = Renderer2d::new(
            setup.device,
            setup.allocator,
            setup.bindless,
            output.format,
            QuadMode::for_device(setup.device),
        )
        .map_err(AppError::at("the editor's panels"))?;
        renderer.set_white(output.white);
        let mut old = core::mem::replace(&mut self.renderer, renderer);
        // SAFETY: the player calls this with the device idle: nothing uses the old
        // renderer's pipelines and buffers.
        unsafe { old.destroy(setup.allocator) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use crate::input::Clipboard;
    use engine_ecs::reflect_component;
    use engine_platform::window::WindowId;
    use engine_reflect::Reflect;
    use engine_ui::{DrawCmd, Rect};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
    #[repr(u8)]
    enum Gait {
        #[default]
        Walk,
        Run,
        Swim,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
    struct Mover {
        gait: Gait,
        enabled: bool,
        speed: f32,
    }

    reflect_component!(Mover);

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ofelia-editor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn event(ed: &mut Editor, kind: WindowEventKind) {
        ed.event(&WindowEvent {
            window: WindowId(0),
            time: Instant::now(),
            kind,
        });
    }

    fn key(ed: &mut Editor, virtual_key: u16, scancode: u16, pressed: bool) {
        event(
            ed,
            WindowEventKind::Key {
                virtual_key,
                scancode,
                extended: false,
                pressed,
                repeat: false,
            },
        );
    }

    fn mouse(ed: &mut Editor, [x, y]: [f32; 2], pressed: Option<bool>) {
        event(
            ed,
            WindowEventKind::CursorMoved {
                x: x.round() as i32,
                y: y.round() as i32,
            },
        );
        if let Some(pressed) = pressed {
            event(
                ed,
                WindowEventKind::MouseButton {
                    button: MouseButton::Left,
                    pressed,
                },
            );
        }
    }

    /// Where the last frame drew the text `s`, every time.
    fn texts(ed: &Editor, s: &str) -> Vec<Rect> {
        let frame = ed.ui.last_frame();
        frame
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCmd::Text { rect, range, .. } if &frame.text[range.clone()] == s => Some(*rect),
                _ => None,
            })
            .collect()
    }

    /// The middle of the text `s` drawn on the row of the text `row`, right of it.
    fn on_row(ed: &Editor, row: &str, s: &str) -> [f32; 2] {
        let at = texts(ed, row)[0];
        let r = texts(ed, s)
            .into_iter()
            .find(|r| (r.pos[1] - at.pos[1]).abs() < 8.0 && r.pos[0] > at.pos[0])
            .unwrap_or_else(|| panic!("no `{s}` on the row of `{row}`"));
        [r.pos[0] + r.size[0] / 2.0, r.pos[1] + r.size[1] / 2.0]
    }

    /// A click at `at`: hover, press, release, a frame each.
    fn click(ed: &mut Editor, world: &mut World, at: [f32; 2]) {
        mouse(ed, at, None);
        ed.update(world);
        mouse(ed, at, Some(true));
        ed.update(world);
        mouse(ed, at, Some(false));
        ed.update(world);
    }

    #[test]
    fn dragging_a_number_snaps_undoes_and_saves() {
        let dir = folder("drag");
        let mut world = World::new();
        let cube = world.spawn((
            SceneId::new(),
            Name::new("Cube"),
            Transform::from_xyz(0.37, 0.0, 0.0),
            GlobalTransform::default(),
        ));
        let id = AssetId::new();
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            Some(PathBuf::from("main.scene")),
            id,
            AssetPaths::new(),
        )
        .unwrap();
        ed.select_only(cube);
        ed.update(&mut world);
        ed.update(&mut world);
        let x = |world: &World| world.get::<Transform>(cube).unwrap().translation.x;

        // Press on the translation's x, hold Ctrl, move 14 px: 0.51 rounded to the grid.
        let at = on_row(&ed, "translation", "x");
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        assert!(ed.drag.is_some(), "the press starts a drag");
        key(&mut ed, 0x11, 0x1D, true);
        mouse(&mut ed, [at[0] + 14.0, at[1]], None);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.5);
        // Without Ctrl it follows the pointer: 0.37 + 30 px × 0.01.
        key(&mut ed, 0x11, 0x1D, false);
        mouse(&mut ed, [at[0] + 30.0, at[1]], None);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.67);
        // Shift: whole steps from the start (0.37 + 0.3).
        key(&mut ed, 0x10, 0x2A, true);
        mouse(&mut ed, [at[0] + 32.0, at[1]], None);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.67);
        key(&mut ed, 0x10, 0x2A, false);
        mouse(&mut ed, [at[0] + 30.0, at[1]], Some(false));
        ed.update(&mut world);
        assert!(ed.drag.is_none());
        assert_eq!(ed.history.steps().count(), 1, "one step per drag");
        assert!(ed.scene_label.ends_with('*'), "{}", ed.scene_label);

        // Escape takes a drag back: the value as it was, no step, the rest ignored.
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, [at[0] + 50.0, at[1]], None);
        ed.update(&mut world);
        assert_eq!(x(&world), 1.17);
        key(&mut ed, 0x1B, 0x01, true);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.67);
        mouse(&mut ed, [at[0] + 80.0, at[1]], None);
        ed.update(&mut world);
        mouse(&mut ed, [at[0] + 80.0, at[1]], Some(false));
        ed.update(&mut world);
        assert_eq!(
            x(&world),
            0.67,
            "the cancelled gesture changes nothing more"
        );
        assert_eq!(ed.history.steps().count(), 1);

        // Ctrl+Z, Ctrl+Y.
        key(&mut ed, 0x11, 0x1D, true);
        key(&mut ed, 0x5A, 0x2C, true);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.37);
        key(&mut ed, 0x59, 0x15, true);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.67);
        // Ctrl+S: the file, under the scene's id; the tab loses its star.
        key(&mut ed, 0x53, 0x1F, true);
        key(&mut ed, 0x11, 0x1D, false);
        ed.update(&mut world);
        ed.update(&mut world);
        assert!(!ed.scene_label.ends_with('*'), "{}", ed.scene_label);
        let text = std::fs::read_to_string(dir.join("main.scene")).unwrap();
        assert!(
            text.starts_with(&format!(
                "[scene version={} id={id}]",
                engine_scene::VERSION
            )),
            "{text}"
        );
        assert!(text.contains("Name = \"Cube\""), "{text}");
        assert!(
            text.contains("Transform.translation = (0.67, 0, 0)"),
            "{text}"
        );
        assert!(
            !text.contains("Camera"),
            "the editor's camera is not saved: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkboxes_and_enum_lists_edit_with_undo() {
        let mut world = World::new();
        let e = world.spawn((SceneId::new(), Mover::default()));
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.select_only(e);
        ed.update(&mut world);
        ed.update(&mut world);
        // The checkbox: right of the name column, on the name's row.
        let name = texts(&ed, "enabled")[0];
        let check = [name.pos[0] + 104.0, name.pos[1] + name.size[1] / 2.0];
        click(&mut ed, &mut world, check);
        assert!(world.get::<Mover>(e).unwrap().enabled);
        // The enum: open its list, pick `Swim`.
        let walk = on_row(&ed, "gait", "Walk");
        click(&mut ed, &mut world, walk);
        assert!(ed.open_choice.is_some(), "the list opens");
        // It is drawn from the next frame on (the click came after this one was built).
        ed.update(&mut world);
        let swim = texts(&ed, "Swim")[0];
        click(
            &mut ed,
            &mut world,
            [swim.pos[0] + 4.0, swim.pos[1] + swim.size[1] / 2.0],
        );
        assert_eq!(world.get::<Mover>(e).unwrap().gait, Gait::Swim);
        assert!(ed.open_choice.is_none(), "picking closes the list");
        // A click elsewhere closes an open list without picking.
        ed.update(&mut world);
        let current = on_row(&ed, "gait", "Swim");
        click(&mut ed, &mut world, current);
        assert!(ed.open_choice.is_some());
        ed.update(&mut world);
        click(&mut ed, &mut world, [10.0, 700.0]);
        assert!(ed.open_choice.is_none());
        assert_eq!(world.get::<Mover>(e).unwrap().gait, Gait::Swim);

        ed.run(&mut world, ed.ids.undo);
        assert_eq!(world.get::<Mover>(e).unwrap().gait, Gait::Walk);
        ed.run(&mut world, ed.ids.undo);
        assert!(!world.get::<Mover>(e).unwrap().enabled);
    }

    #[test]
    fn values_set_by_path_are_undoable_and_checked() {
        let mut world = World::new();
        let e = world.spawn((SceneId::new(), Transform::IDENTITY, Mover::default()));
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.set(&mut world, e, "Transform.translation", "(3, 0, -1)")
            .unwrap();
        ed.set(&mut world, e, "Mover.gait", "Run").unwrap();
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation,
            Vec3::new(3.0, 0.0, -1.0)
        );
        assert_eq!(world.get::<Mover>(e).unwrap().gait, Gait::Run);
        for (path, value) in [("Mover.gait", "Fly"), ("Mover.wings", "2"), ("Health", "3")] {
            assert!(
                ed.set(&mut world, e, path, value).is_err(),
                "{path} = {value}"
            );
        }
        assert_eq!(ed.history.steps().count(), 2, "refused sets leave no step");
        ed.run(&mut world, ed.ids.undo);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(world.get::<Transform>(e).unwrap().translation, Vec3::ZERO);
        assert_eq!(world.get::<Mover>(e).unwrap().gait, Gait::Walk);
    }

    #[test]
    fn commands_add_duplicate_delete_and_save_untitled() {
        let dir = folder("commands");
        let mut world = World::new();
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        let models = |world: &mut World| {
            let mut q = QueryState::<(Entity, &Model)>::new(world);
            q.iter_readonly(world).count()
        };
        ed.add(&mut world, 1);
        let cube = ed.active().unwrap();
        assert_eq!(world.get::<Name>(cube).map(Name::as_str), Some("Cube"));
        assert_eq!(world.get::<Model>(cube).unwrap().mesh, builtin::CUBE);
        ed.run(&mut world, ed.ids.duplicate);
        let copy = ed.active().unwrap();
        assert_ne!(copy, cube);
        assert_eq!(models(&mut world), 2);
        ed.run(&mut world, ed.ids.delete);
        assert!(ed.selection.is_empty());
        assert_eq!(models(&mut world), 1);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(models(&mut world), 2);
        ed.run(&mut world, ed.ids.undo);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(models(&mut world), 0);
        ed.run(&mut world, ed.ids.redo);
        assert_eq!(models(&mut world), 1);
        // An untitled scene is saved as `untitled.scene` in the project.
        ed.run(&mut world, ed.ids.save);
        assert_eq!(ed.scene_path.as_deref(), Some(Path::new("untitled.scene")));
        let text = std::fs::read_to_string(dir.join("untitled.scene")).unwrap();
        assert!(text.contains("Model.mesh = asset("), "{text}");
        assert_eq!(untitled(&dir), PathBuf::from("untitled-2.scene"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn chars(ed: &mut Editor, text: &str) {
        for c in text.chars() {
            event(ed, WindowEventKind::Char(c));
        }
    }

    /// Two entities, the first selected, after two frames (the layout known).
    fn two_entities() -> (World, Editor, Entity, Entity) {
        let mut world = World::new();
        let cube = world.spawn((
            SceneId::new(),
            Name::new("Cube"),
            Transform::from_xyz(0.25, 0.0, 0.0),
            GlobalTransform::default(),
        ));
        let lamp = world.spawn((
            SceneId::new(),
            Name::new("Lamp"),
            Transform::IDENTITY,
            GlobalTransform::default(),
        ));
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.select_only(cube);
        ed.update(&mut world);
        ed.update(&mut world);
        (world, ed, cube, lamp)
    }

    #[test]
    fn f2_renames_in_place_and_shortcuts_wait_while_typing() {
        let (mut world, mut ed, cube, _) = two_entities();
        key(&mut ed, 0x71, 0x3C, true); // F2
        ed.update(&mut world);
        key(&mut ed, 0x71, 0x3C, false);
        ed.update(&mut world);
        assert!(
            ed.ui.editing().is_some(),
            "the row's name is a field, editing"
        );
        // Everything selected: typing replaces it. Y and Delete are the field's keys now.
        chars(&mut ed, "Gatey");
        key(&mut ed, 0x59, 0x15, true); // Y: not "snap by default"
        ed.update(&mut world);
        key(&mut ed, 0x08, 0x0E, true); // Backspace
        key(&mut ed, 0x2E, 0x53, true); // Delete: not "delete the selection"
        ed.update(&mut world);
        assert_eq!(ed.ui.edited_text(), "Gate");
        key(&mut ed, 0x0D, 0x1C, true); // Enter
        ed.update(&mut world);
        assert_eq!(world.get::<Name>(cube).map(Name::as_str), Some("Gate"));
        assert!(world.contains(cube), "Delete went to the field");
        assert!(!ed.snapping.on, "Y went to the field");
        assert!(ed.renaming.is_none());
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(world.get::<Name>(cube).map(Name::as_str), Some("Cube"));
    }

    #[test]
    fn a_double_click_renames_and_escape_keeps_the_name() {
        let (mut world, mut ed, _, lamp) = two_entities();
        let row = texts(&ed, "Lamp")[0];
        let at = [row.pos[0] + 4.0, row.pos[1] + row.size[1] / 2.0];
        click(&mut ed, &mut world, at);
        assert_eq!(ed.active(), Some(lamp));
        click(&mut ed, &mut world, at);
        assert_eq!(ed.renaming, Some(lamp));
        ed.update(&mut world);
        chars(&mut ed, "Torch");
        ed.update(&mut world);
        key(&mut ed, 0x1B, 0x01, true); // Escape
        ed.update(&mut world);
        assert!(ed.renaming.is_none());
        assert_eq!(world.get::<Name>(lamp).map(Name::as_str), Some("Lamp"));
    }

    #[test]
    fn numbers_take_typed_expressions() {
        let (mut world, mut ed, cube, _) = two_entities();
        let x = |world: &World| world.get::<Transform>(cube).unwrap().translation.x;
        // A click without a drag turns the number into a field, everything selected.
        let at = on_row(&ed, "translation", "x");
        click(&mut ed, &mut world, at);
        assert!(ed.typing.is_some());
        ed.update(&mut world);
        assert!(ed.ui.editing().is_some());
        chars(&mut ed, "2*pi");
        ed.update(&mut world);
        key(&mut ed, 0x0D, 0x1C, true);
        ed.update(&mut world);
        assert!(
            (x(&world) - core::f32::consts::TAU).abs() < 1e-5,
            "{}",
            x(&world)
        );
        assert!(ed.typing.is_none());
        // Relative to the value; a bad expression changes nothing and says why.
        for (text, want) in [
            ("+=1", core::f32::consts::TAU + 1.0),
            ("abc", core::f32::consts::TAU + 1.0),
        ] {
            ed.update(&mut world);
            let at = on_row(&ed, "translation", "x");
            click(&mut ed, &mut world, at);
            ed.update(&mut world);
            chars(&mut ed, text);
            ed.update(&mut world);
            key(&mut ed, 0x0D, 0x1C, true);
            ed.update(&mut world);
            assert!((x(&world) - want).abs() < 1e-4, "{text}: {}", x(&world));
        }
        assert!(
            ed.message
                .as_ref()
                .is_some_and(|(m, _)| m.contains("translation")),
            "the bad expression is reported"
        );
        assert_eq!(ed.history.steps().count(), 2, "one step per typed value");
    }

    #[test]
    fn strings_are_typed_and_filters_narrow_the_panels() {
        let (mut world, mut ed, cube, _) = two_entities();
        // The name's field in the inspector: click, Ctrl+A, type, Enter.
        let at = on_row(&ed, "value", "Cube");
        click(&mut ed, &mut world, at);
        key(&mut ed, 0x11, 0x1D, true);
        key(&mut ed, 0x41, 0x1E, true); // Ctrl+A
        key(&mut ed, 0x11, 0x1D, false);
        ed.update(&mut world);
        chars(&mut ed, "Door \"1\"");
        ed.update(&mut world);
        key(&mut ed, 0x0D, 0x1C, true);
        ed.update(&mut world);
        assert_eq!(
            world.get::<Name>(cube).map(Name::as_str),
            Some("Door \"1\"")
        );

        // The tree's filter keeps the matching rows; Escape clears it.
        ed.update(&mut world);
        assert_eq!(ed.rows.len(), 2);
        let hint = texts(&ed, "Filter nodes")[0];
        click(&mut ed, &mut world, [hint.pos[0] + 4.0, hint.pos[1] + 4.0]);
        chars(&mut ed, "lam");
        ed.update(&mut world);
        ed.update(&mut world);
        assert_eq!(ed.rows.len(), 1, "only the lamp");
        key(&mut ed, 0x1B, 0x01, true);
        ed.update(&mut world);
        ed.update(&mut world);
        assert_eq!(ed.rows.len(), 2);

        // The inspector's filter keeps the matching fields.
        let hint = texts(&ed, "Filter properties")[0];
        click(&mut ed, &mut world, [hint.pos[0] + 4.0, hint.pos[1] + 4.0]);
        chars(&mut ed, "scale");
        ed.update(&mut world);
        ed.update(&mut world);
        assert_eq!(ed.shown.len(), 1, "only Transform.scale");
        assert!(texts(&ed, "translation").is_empty());
        assert!(!texts(&ed, "scale").is_empty());
    }

    /// Presses a chord (modifiers then the key, by their virtual-key codes and scan codes)
    /// for one frame, then releases it.
    fn chord(ed: &mut Editor, world: &mut World, keys: &[(u16, u16)]) {
        for &(vk, sc) in keys {
            key(ed, vk, sc, true);
        }
        ed.update(world);
        for &(vk, sc) in keys.iter().rev() {
            key(ed, vk, sc, false);
        }
    }

    const CTRL: (u16, u16) = (0x11, 0x1D);
    const SHIFT: (u16, u16) = (0x10, 0x2A);
    const P: (u16, u16) = (0x50, 0x19);
    const ENTER: (u16, u16) = (0x0D, 0x1C);
    const DOWN: (u16, u16) = (0x28, 0x50);
    const ESCAPE: (u16, u16) = (0x1B, 0x01);
    const BACKSPACE: (u16, u16) = (0x08, 0x0E);
    const C: (u16, u16) = (0x43, 0x2E);
    const V: (u16, u16) = (0x56, 0x2F);

    /// The names of the scene's entities.
    fn names(world: &mut World) -> Vec<String> {
        let mut q = QueryState::<(Entity, &Name)>::new(world);
        let mut names: Vec<String> = q
            .iter_readonly(world)
            .map(|(_, n)| n.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_palette_runs_commands_and_finds_objects() {
        let (mut world, mut ed, cube, lamp) = two_entities();
        // Ctrl+Shift+P opens it; the box takes the keys from the next frame.
        chord(&mut ed, &mut world, &[CTRL, SHIFT, P]);
        assert!(ed.command_palette.is_some());
        ed.update(&mut world);
        assert!(ed.ui.editing().is_some(), "the query box is being typed in");
        // Every command is listed, its shortcut beside it.
        assert!(!texts(&ed, "Ctrl+Shift+P").is_empty());
        // "add cyl" finds "Scene: Add Cylinder" first; Enter runs it.
        chars(&mut ed, "add cyl");
        ed.update(&mut world);
        let cp = ed.command_palette.as_ref().unwrap();
        assert_eq!(cp.commands[0].id, ed.ids.add_kinds[4]);
        // Drawn from the next frame on: the matched letters apart.
        ed.update(&mut world);
        assert!(!texts(&ed, " Cyl").is_empty(), "{:?}", texts(&ed, "Add"));
        chord(&mut ed, &mut world, &[ENTER]);
        assert!(ed.command_palette.is_none());
        assert_eq!(names(&mut world), ["Cube", "Cylinder", "Lamp"]);
        let cylinder = ed.active().unwrap();
        assert_eq!(ed.history.steps().count(), 1);

        // Ctrl+P: the scene's objects. "c" finds the cube and the cylinder, best first
        // (the shorter name), in tree order among equals; Down, Enter selects the second.
        chord(&mut ed, &mut world, &[CTRL, P]);
        ed.update(&mut world);
        chars(&mut ed, "c");
        ed.update(&mut world);
        let cp = ed.command_palette.as_ref().unwrap();
        assert_eq!(cp.query, "@c", "typing goes after the `@`");
        let found: Vec<Entity> = cp.objects.iter().map(|f| f.entity).collect();
        assert_eq!(found, [cube, cylinder]);
        chord(&mut ed, &mut world, &[DOWN]);
        assert_eq!(ed.command_palette.as_ref().unwrap().selected, 1);
        chord(&mut ed, &mut world, &[ENTER]);
        assert_eq!(ed.active(), Some(cylinder));
        // "@lam" and Enter: the lamp.
        chord(&mut ed, &mut world, &[CTRL, P]);
        ed.update(&mut world);
        chars(&mut ed, "lam");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ENTER]);
        assert_eq!(ed.active(), Some(lamp));

        // Escape closes it, and so does a click elsewhere; nothing runs.
        chord(&mut ed, &mut world, &[CTRL, SHIFT, P]);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert!(ed.command_palette.is_none());
        chord(&mut ed, &mut world, &[CTRL, SHIFT, P]);
        ed.update(&mut world);
        click(&mut ed, &mut world, [10.0, 700.0]);
        assert!(ed.command_palette.is_none());
        assert_eq!(ed.history.steps().count(), 1);

        // A click on a row runs it: "dup" and a click on "Duplicate Selection".
        chord(&mut ed, &mut world, &[CTRL, SHIFT, P]);
        ed.update(&mut world);
        chars(&mut ed, "dup");
        ed.update(&mut world);
        ed.update(&mut world);
        let row = texts(&ed, "licate Selection")[0];
        click(
            &mut ed,
            &mut world,
            [row.pos[0] + 4.0, row.pos[1] + row.size[1] / 2.0],
        );
        assert!(ed.command_palette.is_none());
        assert_eq!(names(&mut world), ["Cube", "Cylinder", "Lamp", "Lamp 2"]);
    }

    #[test]
    fn fields_reset_copy_and_paste() {
        let (mut world, mut ed, cube, lamp) = two_entities();
        ed.input.clipboard = Clipboard::Local(String::new());
        let translation = |world: &World| world.get::<Transform>(cube).unwrap().translation;
        // Only the changed field has a reset button: the cube's translation (the name has
        // none).
        assert_eq!(texts(&ed, icons::RESET).len(), 1);
        let reset = on_row(&ed, "translation", icons::RESET);
        click(&mut ed, &mut world, reset);
        assert_eq!(translation(&world), Vec3::ZERO);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(translation(&world).x, 0.25);

        // Backspace over the field resets it too.
        ed.update(&mut world);
        let name = texts(&ed, "translation")[0];
        mouse(&mut ed, [name.pos[0] + 4.0, name.pos[1] + 4.0], None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[BACKSPACE]);
        assert_eq!(translation(&world), Vec3::ZERO);
        ed.run(&mut world, ed.ids.undo);

        // Ctrl+C over the translation, Ctrl+V over the scale: the value as scene text.
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, C]);
        assert_eq!(ed.input.clipboard.text().as_deref(), Some("(0.25, 0, 0)"));
        let scale = texts(&ed, "scale")[0];
        mouse(&mut ed, [scale.pos[0] + 4.0, scale.pos[1] + 4.0], None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, V]);
        assert_eq!(
            world.get::<Transform>(cube).unwrap().scale,
            Vec3::new(0.25, 0.0, 0.0)
        );
        // Text that is not a vector changes nothing and says why.
        let steps = ed.history.steps().count();
        ed.input.clipboard.set_text("tall");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, V]);
        assert_eq!(ed.history.steps().count(), steps);
        assert!(
            ed.message
                .as_ref()
                .is_some_and(|(m, _)| m.starts_with("Transform.scale")),
            "{:?}",
            ed.message
        );

        // A name is copied as it is, and pasted onto another entity's name.
        ed.update(&mut world);
        let value = on_row(&ed, "value", "Cube");
        mouse(&mut ed, [value[0] - 60.0, value[1]], None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, C]);
        assert_eq!(ed.input.clipboard.text().as_deref(), Some("Cube"));
        ed.select_only(lamp);
        ed.update(&mut world);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, V]);
        assert_eq!(world.get::<Name>(lamp).map(Name::as_str), Some("Cube"));
    }

    /// A key with the extended flag, as Windows sends the arrows.
    fn arrow(ed: &mut Editor, world: &mut World, keys: &[(u16, u16)], vk: u16, sc: u16) {
        for &(k, s) in keys {
            key(ed, k, s, true);
        }
        event(
            ed,
            WindowEventKind::Key {
                virtual_key: vk,
                scancode: sc,
                extended: true,
                pressed: true,
                repeat: false,
            },
        );
        ed.update(world);
        for &(k, s) in keys.iter().rev() {
            key(ed, k, s, false);
        }
    }

    const UP: (u16, u16) = (0x26, 0x48);
    const LEFT: (u16, u16) = (0x25, 0x4B);
    const RIGHT: (u16, u16) = (0x27, 0x4D);
    const DOWN_KEY: (u16, u16) = (0x28, 0x50);

    /// Where the text `s` is drawn in the scene tree's dock (left of the inspector).
    fn in_tree(ed: &Editor, s: &str) -> Vec<Rect> {
        let hint = texts(ed, "Filter nodes")[0];
        let (x0, x1) = (hint.pos[0] - 40.0, hint.pos[0] + 240.0);
        texts(ed, s)
            .into_iter()
            .filter(|r| r.pos[0] > x0 && r.pos[0] < x1 && r.pos[1] > hint.pos[1])
            .collect()
    }

    /// A click on the tree row named `name`, with `mods` held.
    fn click_row(ed: &mut Editor, world: &mut World, name: &str, mods: &[(u16, u16)]) {
        ed.update(world);
        let r = in_tree(ed, name)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no row `{name}`"));
        for &(k, s) in mods {
            key(ed, k, s, true);
        }
        click(ed, world, [r.pos[0] + 4.0, r.pos[1] + r.size[1] / 2.0]);
        for &(k, s) in mods.iter().rev() {
            key(ed, k, s, false);
        }
    }

    /// The names of the selected entities, in their order.
    fn selected_names(ed: &Editor, world: &World) -> Vec<String> {
        ed.selection
            .iter()
            .map(|&e| world.get::<Name>(e).unwrap().as_str().to_owned())
            .collect()
    }

    /// Three entities in the tree: Cube, Lamp, Crate, nothing selected.
    fn three_entities() -> (World, Editor) {
        let (mut world, mut ed, _, _) = two_entities();
        world.spawn((
            SceneId::new(),
            Name::new("Crate"),
            Transform::IDENTITY,
            GlobalTransform::default(),
        ));
        ed.selection.clear();
        ed.update(&mut world);
        ed.update(&mut world);
        (world, ed)
    }

    #[test]
    fn the_tree_selects_with_ctrl_shift_and_the_arrows() {
        let (mut world, mut ed) = three_entities();
        click_row(&mut ed, &mut world, "Cube", &[]);
        assert_eq!(selected_names(&ed, &world), ["Cube"]);
        // Ctrl+click adds, active; again takes out.
        click_row(&mut ed, &mut world, "Crate", &[CTRL]);
        assert_eq!(selected_names(&ed, &world), ["Cube", "Crate"]);
        click_row(&mut ed, &mut world, "Cube", &[CTRL]);
        assert_eq!(selected_names(&ed, &world), ["Crate"]);
        // Shift+click: the rows from the anchor (Cube, the last Ctrl+click) to the click.
        click_row(&mut ed, &mut world, "Lamp", &[]);
        click_row(&mut ed, &mut world, "Crate", &[SHIFT]);
        assert_eq!(selected_names(&ed, &world), ["Lamp", "Crate"]);
        // The inspector's header counts them; the active one is Crate.
        ed.update(&mut world);
        assert!(!texts(&ed, "Crate  ·  2 selected").is_empty());

        // After a click in the tree the arrows move the selection, Shift extends it.
        click_row(&mut ed, &mut world, "Cube", &[]);
        let (d_vk, d_sc) = DOWN_KEY;
        arrow(&mut ed, &mut world, &[], d_vk, d_sc);
        assert_eq!(selected_names(&ed, &world), ["Lamp"]);
        arrow(&mut ed, &mut world, &[SHIFT], d_vk, d_sc);
        assert_eq!(selected_names(&ed, &world), ["Lamp", "Crate"]);
        let (u_vk, u_sc) = UP;
        arrow(&mut ed, &mut world, &[], u_vk, u_sc);
        assert_eq!(selected_names(&ed, &world), ["Lamp"]);
        // A click elsewhere (the viewport) takes the arrows away from the tree.
        click(&mut ed, &mut world, [400.0, 300.0]);
        arrow(&mut ed, &mut world, &[], d_vk, d_sc);
        assert_eq!(selected_names(&ed, &world), ["Lamp"]);
    }

    #[test]
    fn left_and_right_close_and_open_rows() {
        let (mut world, mut ed) = three_entities();
        let names = |world: &World, ed: &Editor| -> Vec<String> {
            ed.rows
                .iter()
                .map(|r| world.get::<Name>(r.entity).unwrap().as_str().to_owned())
                .collect()
        };
        // Crate under Lamp.
        let lamp = ed.rows[1].entity;
        let crate_ = ed.rows[2].entity;
        world.set_parent(crate_, Some(lamp)).unwrap();
        ed.update(&mut world);
        click_row(&mut ed, &mut world, "Lamp", &[]);
        let (l_vk, l_sc) = LEFT;
        let (r_vk, r_sc) = RIGHT;
        arrow(&mut ed, &mut world, &[], l_vk, l_sc);
        ed.update(&mut world);
        assert_eq!(names(&world, &ed), ["Cube", "Lamp"], "Left closes the row");
        arrow(&mut ed, &mut world, &[], r_vk, r_sc);
        ed.update(&mut world);
        assert_eq!(
            names(&world, &ed),
            ["Cube", "Lamp", "Crate"],
            "Right opens it"
        );
        arrow(&mut ed, &mut world, &[], r_vk, r_sc);
        assert_eq!(
            selected_names(&ed, &world),
            ["Crate"],
            "then goes to its child"
        );
        arrow(&mut ed, &mut world, &[], l_vk, l_sc);
        assert_eq!(
            selected_names(&ed, &world),
            ["Lamp"],
            "Left on a leaf: its parent"
        );
    }

    #[test]
    fn the_selection_is_deleted_and_duplicated_whole_with_numbered_names() {
        let (mut world, mut ed) = three_entities();
        click_row(&mut ed, &mut world, "Cube", &[]);
        click_row(&mut ed, &mut world, "Crate", &[CTRL]);
        // Ctrl+D: each copy after its original, numbered; the copies are selected.
        ed.run(&mut world, ed.ids.duplicate);
        assert_eq!(selected_names(&ed, &world), ["Cube 2", "Crate 2"]);
        assert_eq!(
            names(&mut world),
            ["Crate", "Crate 2", "Cube", "Cube 2", "Lamp"]
        );
        assert_eq!(ed.history.steps().count(), 1, "one step");
        // Delete: both copies, one step; undo brings them back selected.
        ed.run(&mut world, ed.ids.delete);
        assert!(ed.selection.is_empty());
        assert_eq!(names(&mut world), ["Crate", "Cube", "Lamp"]);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(
            names(&mut world),
            ["Crate", "Crate 2", "Cube", "Cube 2", "Lamp"]
        );
        // A new entity is numbered among the roots too.
        ed.add(&mut world, 1);
        assert_eq!(selected_names(&ed, &world), ["Cube 3"]);
        // A child goes with its selected parent: deleting both deletes once.
        let lamp = ed
            .rows
            .iter()
            .find(|r| world.get::<Name>(r.entity).unwrap().as_str() == "Lamp")
            .unwrap()
            .entity;
        let cube = ed.active().unwrap();
        world.set_parent(cube, Some(lamp)).unwrap();
        ed.selection = vec![lamp, cube];
        assert_eq!(ed.selection_roots(&world), [lamp]);
    }

    #[test]
    fn a_picked_row_is_scrolled_into_view() {
        let (mut world, mut ed, _, _) = two_entities();
        for i in 0..60 {
            world.spawn((
                SceneId::new(),
                Name::new(format!("Prop {i:02}").as_str()),
                Transform::IDENTITY,
                GlobalTransform::default(),
            ));
        }
        ed.update(&mut world);
        ed.update(&mut world);
        // Below the window at first: not drawn, or drawn below it.
        let y = |ed: &Editor| {
            in_tree(ed, "Prop 59")
                .first()
                .map_or(f32::INFINITY, |r| r.pos[1])
        };
        assert!(y(&ed) > 720.0);
        // Ctrl+P, "@prop 59", Enter: selected, and its row comes into view.
        chord(&mut ed, &mut world, &[CTRL, P]);
        ed.update(&mut world);
        chars(&mut ed, "prop 59");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ENTER]);
        for _ in 0..3 {
            ed.update(&mut world);
        }
        assert_eq!(selected_names(&ed, &world), ["Prop 59"]);
        assert!(y(&ed) < 680.0, "{}", y(&ed));
    }

    #[test]
    fn kinds_have_their_icons_in_the_menu_and_the_tree() {
        let mut world = World::new();
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        // The add menu shows every kind's icon.
        ed.add_open = true;
        ed.update(&mut world);
        for glyph in ADD_ICONS {
            assert!(!texts(&ed, glyph).is_empty(), "{glyph:?} in the menu");
        }
        // A sphere's row has the sphere's icon, a spot light's the spot's.
        ed.add(&mut world, 2);
        ed.add(&mut world, 7);
        ed.update(&mut world);
        ed.update(&mut world);
        let rows: Vec<f32> = in_tree(&ed, "Sphere")
            .iter()
            .chain(in_tree(&ed, "Spot Light").iter())
            .map(|r| r.pos[1])
            .collect();
        let glyph_rows =
            |glyph: &str| -> Vec<f32> { in_tree(&ed, glyph).iter().map(|r| r.pos[1]).collect() };
        assert!(
            glyph_rows(icons::SPHERE)
                .iter()
                .any(|&y| (y - rows[0]).abs() < 6.0)
        );
        assert!(
            glyph_rows(icons::SPOT_LIGHT)
                .iter()
                .any(|&y| (y - rows[1]).abs() < 6.0)
        );
    }

    /// Two entities selected, A then B (B active): A at x 1 with a `Mover` (speed 2, on),
    /// B at x 3 with a `Mover` (speed 5, off) when `both_move`.
    fn two_selected(both_move: bool) -> (World, Editor, Entity, Entity) {
        let mut world = World::new();
        let a = world.spawn((
            SceneId::new(),
            Name::new("A"),
            Transform::from_xyz(1.0, 0.0, 0.0),
            GlobalTransform::default(),
            Mover {
                gait: Gait::Run,
                enabled: true,
                speed: 2.0,
            },
        ));
        let b = world.spawn((
            SceneId::new(),
            Name::new("B"),
            Transform::from_xyz(3.0, 0.0, 0.0),
            GlobalTransform::default(),
        ));
        if both_move {
            world.insert(
                b,
                Mover {
                    gait: Gait::Run,
                    enabled: false,
                    speed: 5.0,
                },
            );
        }
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.selection = vec![a, b];
        ed.update(&mut world);
        ed.update(&mut world);
        (world, ed, a, b)
    }

    #[test]
    fn a_selection_shows_its_shared_components_with_dashes_where_they_differ() {
        let (mut world, ed, _, _) = two_selected(false);
        // B has no `Mover`: only the shared components are shown.
        assert!(texts(&ed, "Mover").is_empty());
        assert!(!texts(&ed, "Transform").is_empty());
        // The x parts differ, y and z do not; the names differ.
        let row = texts(&ed, "translation")[0];
        let on_row = |s: &str| {
            texts(&ed, s)
                .iter()
                .filter(|r| (r.pos[1] - row.pos[1]).abs() < 8.0 && r.pos[0] > row.pos[0])
                .count()
        };
        assert_eq!(on_row(MIXED), 1, "x only");
        assert_eq!(on_row("0"), 2, "y and z");
        let name = texts(&ed, "value")[0];
        assert!(
            texts(&ed, MIXED)
                .iter()
                .any(|r| (r.pos[1] - name.pos[1]).abs() < 8.0),
            "the names differ"
        );
        // Both have the same Mover when added: the section comes back.
        let b = ed.selection[1];
        world.insert(
            b,
            Mover {
                gait: Gait::Run,
                enabled: true,
                speed: 2.0,
            },
        );
        let mut ed = ed;
        ed.update(&mut world);
        ed.update(&mut world);
        assert!(!texts(&ed, "Mover").is_empty());
    }

    #[test]
    fn edits_go_to_every_selected_entity_each_from_its_own_value() {
        let (mut world, mut ed, a, b) = two_selected(true);
        let x = |world: &World| [a, b].map(|e| world.get::<Transform>(e).unwrap().translation.x);
        let type_into = |ed: &mut Editor, world: &mut World, text: &str| {
            ed.update(world);
            let at = on_row(ed, "translation", MIXED);
            click(ed, world, at);
            ed.update(world);
            chars(ed, text);
            ed.update(world);
            key(ed, 0x0D, 0x1C, true);
            ed.update(world);
        };
        // An absolute value goes to both.
        type_into(&mut ed, &mut world, "5");
        assert_eq!(x(&world), [5.0, 5.0]);
        assert_eq!(ed.history.steps().count(), 1, "one step for both");
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(x(&world), [1.0, 3.0]);
        // A relative one moves each from its own.
        type_into(&mut ed, &mut world, "+=1");
        assert_eq!(x(&world), [2.0, 4.0]);
        ed.run(&mut world, ed.ids.undo);

        // A drag of 30 px moves each by 0.3 from its own start.
        ed.update(&mut world);
        let at = on_row(&ed, "translation", MIXED);
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, [at[0] + 30.0, at[1]], None);
        ed.update(&mut world);
        mouse(&mut ed, [at[0] + 30.0, at[1]], Some(false));
        ed.update(&mut world);
        let [xa, xb] = x(&world);
        assert!(
            (xa - 1.3).abs() < 1e-5 && (xb - 3.3).abs() < 1e-5,
            "{xa} {xb}"
        );
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(x(&world), [1.0, 3.0]);

        // A checkbox that differs turns both on, then both off.
        let enabled = |world: &World| [a, b].map(|e| world.get::<Mover>(e).unwrap().enabled);
        ed.update(&mut world);
        let name = texts(&ed, "enabled")[0];
        let check = [name.pos[0] + 104.0, name.pos[1] + name.size[1] / 2.0];
        click(&mut ed, &mut world, check);
        assert_eq!(enabled(&world), [true, true]);
        click(&mut ed, &mut world, check);
        assert_eq!(enabled(&world), [false, false]);

        // The speeds differ from the default (0) in both: ↺ resets both.
        let speed = |world: &World| [a, b].map(|e| world.get::<Mover>(e).unwrap().speed);
        ed.update(&mut world);
        let reset = on_row(&ed, "speed", icons::RESET);
        click(&mut ed, &mut world, reset);
        assert_eq!(speed(&world), [0.0, 0.0]);
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(speed(&world), [2.0, 5.0]);

        // A paste goes to both, as one step.
        ed.input.clipboard = Clipboard::Local(String::from("(0, 2, 0)"));
        let steps = ed.history.steps().count();
        ed.update(&mut world);
        let row = texts(&ed, "translation")[0];
        mouse(&mut ed, [row.pos[0] + 4.0, row.pos[1] + 4.0], None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[CTRL, V]);
        for e in [a, b] {
            assert_eq!(
                world.get::<Transform>(e).unwrap().translation,
                Vec3::new(0.0, 2.0, 0.0)
            );
        }
        assert_eq!(ed.history.steps().count(), steps + 1);
    }

    /// The tree's rows' names, in order.
    fn row_names(ed: &Editor, world: &World) -> Vec<String> {
        ed.rows
            .iter()
            .map(|r| world.get::<Name>(r.entity).unwrap().as_str().to_owned())
            .collect()
    }

    #[test]
    fn roots_keep_their_order_through_add_duplicate_save_and_load() {
        let dir = folder("order");
        let mut world = World::new();
        let a = world.spawn((SceneId::new(), Name::new("A"), Transform::IDENTITY));
        world.spawn((SceneId::new(), Name::new("B"), Transform::IDENTITY));
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            Some(PathBuf::from("main.scene")),
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        // Roots without an order are numbered in the order shown.
        ed.update(&mut world);
        assert_eq!(world.get::<RootOrder>(a), Some(&RootOrder(1024)));
        // A new entity goes last; a copy right after its original.
        ed.add(&mut world, 1);
        ed.select_only(a);
        ed.run(&mut world, ed.ids.duplicate);
        ed.update(&mut world);
        assert_eq!(row_names(&ed, &world), ["A", "A 2", "B", "Cube"]);

        // Saved and loaded: the blocks come in id order, the tree in the same order.
        ed.run(&mut world, ed.ids.save);
        let text = std::fs::read_to_string(dir.join("main.scene")).unwrap();
        assert_eq!(text.matches("\norder = ").count(), 4, "{text}");
        let mut loaded = World::new();
        engine_scene::load(&mut loaded, &SceneRegistry::from_program(), &text).unwrap();
        let mut ed2 = Editor::new(
            [1280, 720],
            dir.clone(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed2.update(&mut loaded);
        assert_eq!(row_names(&ed2, &loaded), ["A", "A 2", "B", "Cube"]);

        // No gap left between A and A 2: the roots are numbered again, in the same step.
        ed.select_only(a);
        for _ in 0..10 {
            ed.run(&mut world, ed.ids.duplicate);
            ed.select_only(a);
        }
        ed.update(&mut world);
        let names = row_names(&ed, &world);
        assert_eq!(names.first().map(String::as_str), Some("A"));
        assert_eq!(names.last().map(String::as_str), Some("Cube"));
        assert_eq!(names.len(), 14);
        // Undo takes the copies back one by one, the order as it was.
        for _ in 0..10 {
            ed.run(&mut world, ed.ids.undo);
        }
        ed.update(&mut world);
        assert_eq!(row_names(&ed, &world), ["A", "A 2", "B", "Cube"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project whose `main.scene` holds a cube at the origin.
    fn project(name: &str) -> PathBuf {
        let dir = folder(name);
        let mut world = World::new();
        world.spawn((
            SceneId::new(),
            Name::new("Cube"),
            Transform::IDENTITY,
            GlobalTransform::default(),
        ));
        let mut text = String::new();
        let options = SaveOptions {
            id: Some(AssetId::new()),
            assets: None,
        };
        engine_scene::save_with(
            &mut world,
            &SceneRegistry::from_program(),
            &options,
            &mut text,
        )
        .unwrap();
        std::fs::write(dir.join("main.scene"), text).unwrap();
        dir
    }

    /// An editor started on the project as the editor's binary starts one: the scene's
    /// file loaded into a new world, then the session begun.
    fn start(dir: &Path) -> (World, Editor) {
        let mut world = World::new();
        let text = std::fs::read_to_string(dir.join("main.scene")).unwrap();
        engine_scene::load(&mut world, &SceneRegistry::from_program(), &text).unwrap();
        let (_, id) = engine_scene::scene_header(&text).unwrap();
        let mut ed = Editor::new(
            [1280, 720],
            dir.to_path_buf(),
            Some(PathBuf::from("main.scene")),
            id.unwrap(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.begin_session(&mut world).unwrap();
        (world, ed)
    }

    fn cube_x(world: &mut World) -> f32 {
        let cube = engine_scene::find_named(world, "Cube").unwrap();
        world.get::<Transform>(cube).unwrap().translation.x
    }

    fn move_cube(ed: &mut Editor, world: &mut World, x: f32) {
        let cube = engine_scene::find_named(world, "Cube").unwrap();
        ed.set(
            world,
            cube,
            "Transform.translation",
            &format!("({x}, 0, 0)"),
        )
        .unwrap();
    }

    fn message(ed: &Editor) -> &str {
        ed.message.as_ref().map_or("", |(m, _)| m.as_str())
    }

    fn flush(ed: &Editor) {
        ed.session.as_ref().unwrap().writer.flush();
    }

    fn autosave_of(ed: &Editor) -> PathBuf {
        ed.session.as_ref().unwrap().autosave.clone()
    }

    #[test]
    fn unsaved_changes_come_back_after_a_crash_and_after_a_clean_exit() {
        let dir = project("session-back");
        let file = std::fs::read_to_string(dir.join("main.scene")).unwrap();
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 3.0);
        ed.update(&mut world);
        let autosave = autosave_of(&ed);
        assert!(!autosave.exists(), "not before its time");
        // A second editor on the same project is refused.
        let mut other = Editor::new(
            [1280, 720],
            dir.clone(),
            Some(PathBuf::from("main.scene")),
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        let refused = other.begin_session(&mut World::new());
        assert!(refused.is_err_and(|e| e.to_string().contains("another editor")));
        // Before Play or a restart: autosaved at once.
        ed.save_session(&mut world);
        let saved = std::fs::read_to_string(&autosave).unwrap();
        assert!(
            saved.contains("Transform.translation = (3, 0, 0)"),
            "{saved}"
        );

        // A crash: the editor is gone without ending its session.
        drop(ed);
        let (mut world, mut ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0, "the unsaved changes are back");
        assert!(
            message(&ed).starts_with("After a crash: restored the unsaved changes of main.scene"),
            "{}",
            message(&ed)
        );
        ed.update(&mut world);
        assert!(ed.scene_label.ends_with('*'), "{}", ed.scene_label);
        assert_eq!(
            std::fs::read_to_string(dir.join("main.scene")).unwrap(),
            file,
            "the user's file is untouched"
        );

        // A clean exit with them asks nothing, and the next start has them again.
        ed.end_session(&mut world);
        assert!(!dir.join(".ofelia/session.lock").exists());
        let (mut world, mut ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0);
        assert!(
            message(&ed).starts_with("Restored the unsaved changes of main.scene"),
            "{}",
            message(&ed)
        );
        // Ctrl+S writes them, and the autosave goes.
        let save = ed.ids.save;
        ed.run(&mut world, save);
        ed.update(&mut world);
        ed.end_session(&mut world);
        let written = std::fs::read_to_string(dir.join("main.scene")).unwrap();
        assert!(written.contains("(3, 0, 0)"), "{written}");
        assert!(!autosave.exists(), "a clean scene has no autosave");
        let (mut world, ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0);
        assert_eq!(message(&ed), "", "nothing to bring back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scene_changed_on_disk_is_never_written_over() {
        let dir = project("session-conflict");
        let file = dir.join("main.scene");
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 3.0);
        ed.save_session(&mut world);
        drop(ed);
        // A teammate's change lands meanwhile.
        let theirs = std::fs::read_to_string(&file).unwrap().replace(
            "Transform.translation = (0, 0, 0)",
            "Transform.translation = (5, 0, 0)",
        );
        assert!(theirs.contains("(5, 0, 0)"), "{theirs}");
        std::fs::write(&file, &theirs).unwrap();

        let (mut world, mut ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0, "the unsaved changes come back");
        assert!(
            message(&ed).ends_with("its file changed on disk since"),
            "{}",
            message(&ed)
        );
        ed.update(&mut world);
        assert!(ed.scene_label.contains(icons::ALERT), "{}", ed.scene_label);
        assert!(
            ed.status.contains("main.scene changed on disk"),
            "{}",
            ed.status
        );
        // Ctrl+S writes nothing and says why.
        let save = ed.ids.save;
        ed.run(&mut world, save);
        assert!(message(&ed).starts_with("Not saved"), "{}", message(&ed));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
        // Their version as one undo step, ours kept aside.
        let take = ed.ids.take_disk;
        ed.run(&mut world, take);
        assert_eq!(cube_x(&mut world), 5.0);
        ed.update(&mut world);
        assert!(
            !ed.scene_label.ends_with('*') && !ed.scene_label.contains(icons::ALERT),
            "{}",
            ed.scene_label
        );
        flush(&ed);
        let aside = autosave_of(&ed).with_extension("conflict.scene");
        assert!(aside.is_file(), "{}", aside.display());
        let undo = ed.ids.undo;
        ed.run(&mut world, undo);
        assert_eq!(cube_x(&mut world), 3.0, "Ctrl+Z brings ours back");

        // Ours kept at a clean exit (a crash this soon after a restore would open the file
        // as saved), theirs changed once more: this time ours goes over it.
        ed.end_session(&mut world);
        std::fs::write(&file, theirs.replace("(5, 0, 0)", "(7, 0, 0)")).unwrap();
        let (mut world, mut ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0);
        let over = ed.ids.save_over_disk;
        ed.run(&mut world, over);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(
            written.contains("Transform.translation = (3, 0, 0)"),
            "{written}"
        );
        ed.update(&mut world);
        assert!(!ed.scene_label.contains(icons::ALERT), "{}", ed.scene_label);
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autosaves_wait_for_their_time_and_go_once_the_scene_is_clean() {
        let dir = project("session-clean");
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 3.0);
        ed.update(&mut world);
        let autosave = autosave_of(&ed);
        let s = ed.session.as_mut().unwrap();
        let due = s.due.expect("the change set its time");
        assert!(due > Instant::now() + Duration::from_secs(25));
        // Its time comes, after a second without input.
        let earlier = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        s.due = Some(earlier);
        s.last_input = earlier;
        ed.update(&mut world);
        flush(&ed);
        assert!(autosave.is_file(), "autosaved at its time");
        // Undone back to the file: the scene is clean, and its autosave goes.
        let undo = ed.ids.undo;
        ed.run(&mut world, undo);
        ed.update(&mut world);
        flush(&ed);
        assert!(!autosave.exists());
        let session = std::fs::read_to_string(dir.join(".ofelia/session")).unwrap();
        assert!(!session.contains("autosave ="), "{session}");
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_start_that_stopped_while_restoring_opens_the_file_as_saved() {
        let dir = project("session-restoring");
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 3.0);
        ed.save_session(&mut world);
        let autosave = autosave_of(&ed);
        drop(ed);
        // This start brings the changes back, then stops before the lock says running.
        let (mut world, ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 3.0);
        drop(ed);
        let (mut world, ed) = start(&dir);
        assert_eq!(cube_x(&mut world), 0.0, "opened as saved");
        assert!(
            message(&ed).starts_with("The last start stopped"),
            "{}",
            message(&ed)
        );
        assert!(autosave.is_file(), "the changes are kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_view_comes_back_with_the_session() {
        let dir = project("session-view");
        let (mut world, mut ed) = start(&dir);
        let cube = engine_scene::find_named(&mut world, "Cube").unwrap();
        ed.orbit.target = Vec3::new(1.5, -2.0, 0.25);
        ed.orbit.yaw = 1.25;
        ed.orbit.pitch = -0.5;
        ed.orbit.distance = 12.0;
        ed.select_only(cube);
        ed.collapsed.insert(cube);
        ed.tree_filter = String::from("Cu");
        ed.inspector_filter = String::from("trans");
        ed.active_tool = 2;
        // Y: snapping by default, a command run (the palette ranks it first).
        let snap = ed.ids.snap;
        ed.run(&mut world, snap);
        assert!(ed.snapping.on);
        ed.end_session(&mut world);

        let (mut world, ed) = start(&dir);
        let cube = engine_scene::find_named(&mut world, "Cube").unwrap();
        assert_eq!(ed.orbit.target, Vec3::new(1.5, -2.0, 0.25));
        assert_eq!(
            (ed.orbit.yaw, ed.orbit.pitch, ed.orbit.distance),
            (1.25, -0.5, 12.0)
        );
        assert_eq!(ed.selection, [cube]);
        assert_eq!(ed.anchor, Some(cube));
        assert!(ed.collapsed.contains(&cube));
        assert_eq!(
            (ed.tree_filter.as_str(), ed.inspector_filter.as_str()),
            ("Cu", "trans")
        );
        assert_eq!(ed.active_tool, 2);
        assert!(ed.snapping.on);
        let recent: Vec<&str> = ed
            .commands
            .recent()
            .into_iter()
            .map(|id| ed.commands.info(id).name.as_str())
            .collect();
        assert_eq!(recent, ["edit.snap"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_autosaves_come_back_from_the_palette() {
        let dir = project("session-older");
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 1.0);
        ed.save_session(&mut world);
        // A minute later the next autosave moves this one down to `.1`.
        let latest = autosave_of(&ed);
        let minute_ago = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&latest)
            .unwrap()
            .set_modified(minute_ago)
            .unwrap();
        move_cube(&mut ed, &mut world, 2.0);
        ed.save_session(&mut world);
        assert!(session::older(&latest, 1).is_file());

        let restore = ed.ids.restore_autosave;
        ed.run(&mut world, restore);
        let cp = ed.command_palette.as_ref().unwrap();
        let choices = cp.choices.as_ref().unwrap();
        let labels: Vec<&str> = choices.items.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["main.scene from just now", "main.scene from 1 min ago"]
        );
        assert_eq!(choices.shown.len(), 2);
        // The older one: x = 1 again, as one step that Ctrl+Z takes back.
        ed.palette_action(&mut world, PaletteAction::Run(1));
        assert_eq!(cube_x(&mut world), 1.0);
        assert!(
            message(&ed).starts_with("Brought back the unsaved changes of main.scene"),
            "{}",
            message(&ed)
        );
        let undo = ed.ids.undo;
        ed.run(&mut world, undo);
        assert_eq!(cube_x(&mut world), 2.0);
        // Opened again as the command palette, it lists commands.
        ed.open_palette(&mut world, "");
        assert!(ed.command_palette.as_ref().unwrap().choices.is_none());
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_build_restarts_by_itself_in_the_background_and_is_offered_in_front() {
        let dir = project("rebuild-policy");
        let (mut world, mut ed) = start(&dir);
        let slot = dir.join(".ofelia/run/b/game.exe");
        let built = || Built {
            success: true,
            took: Duration::from_millis(1480),
            exe: Some(slot.clone()),
            artifact: Some(PathBuf::from("C:\\t\\debug\\game.exe")),
            ..Built::default()
        };
        // In front: offered, and Ctrl+R takes it.
        event(&mut ed, WindowEventKind::Focused(true));
        ed.finished_build(built());
        ed.update(&mut world);
        assert!(ed.restart_with.is_none(), "offered, not taken");
        assert!(ed.status.contains("Ctrl+R restarts"), "{}", ed.status);
        let restart = ed.ids.restart;
        ed.run(&mut world, restart);
        assert_eq!(
            ed.restart_with.as_ref().map(|(exe, _)| exe),
            Some(&slot),
            "Ctrl+R restarts with it"
        );
        // In the background: taken by itself, the session saved first.
        ed.restart_with = None;
        move_cube(&mut ed, &mut world, 3.0);
        event(&mut ed, WindowEventKind::Focused(false));
        ed.finished_build(built());
        ed.update(&mut world);
        assert!(ed.restart_with.is_some(), "taken in the background");
        let autosave = autosave_of(&ed);
        assert!(autosave.is_file(), "the session saved before the restart");
        // Asked always: never by itself.
        ed.restart_with = None;
        ed.restart_policy = RestartPolicy::Ask;
        ed.finished_build(built());
        ed.update(&mut world);
        assert!(ed.restart_with.is_none());
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_build_shows_its_errors_by_line() {
        let dir = project("rebuild-errors");
        let (mut world, mut ed) = start(&dir);
        ed.game_crate = Some(GameCrate::new(dir.clone()));
        ed.finished_build(Built {
            success: false,
            errors: vec![
                Problem {
                    at: Some((dir.join("src").join("main.rs"), 12, 5)),
                    message: String::from("mismatched types"),
                },
                Problem {
                    at: None,
                    message: String::from("linking failed"),
                },
            ],
            ..Built::default()
        });
        ed.update(&mut world);
        assert!(
            ed.status
                .contains("Build failed: 2 errors, src/main.rs:12:5: mismatched types"),
            "{}",
            ed.status
        );
        let errors = ed.ids.build_errors;
        ed.run(&mut world, errors);
        let cp = ed.command_palette.as_ref().unwrap();
        let labels: Vec<&str> = cp
            .choices
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, ["src/main.rs:12:5", "the build"]);
        // The next good build clears them.
        ed.command_palette = None;
        ed.finished_build(Built {
            success: true,
            ..Built::default()
        });
        ed.update(&mut world);
        assert!(!ed.status.contains("Build failed"), "{}", ed.status);
        assert!(ed.problems.is_empty());
        // Without builds (cargo's own file), the commands say how to get them.
        let now = ed.ids.build_now;
        ed.run(&mut world, now);
        assert_eq!(message(&ed), NOT_BUILDING);
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_restart_policy_is_picked_in_the_palette_and_kept() {
        let dir = project("rebuild-kept");
        let (mut world, mut ed) = start(&dir);
        let policy = ed.ids.restart_policy;
        ed.run(&mut world, policy);
        let labels: Vec<String> = ed
            .command_palette
            .as_ref()
            .unwrap()
            .choices
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.label.clone())
            .collect();
        assert_eq!(
            labels,
            [
                "Restart by itself while the editor is in the background",
                "Always restart by itself",
                "Always ask before restarting"
            ]
        );
        ed.palette_action(&mut world, PaletteAction::Run(2));
        assert_eq!(ed.restart_policy, RestartPolicy::Ask);
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert_eq!(ed.restart_policy, RestartPolicy::Ask);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_says_when_from_whom_and_whether_in_front() {
        assert_eq!(
            Restarted::parse("1760990000000000000 4242 1"),
            Some(Restarted {
                at: 1_760_990_000_000_000_000,
                from: Some(4242),
                front: true
            })
        );
        // E1-5c-1's restarts gave the time alone.
        assert_eq!(
            Restarted::parse("17"),
            Some(Restarted {
                at: 17,
                from: None,
                front: false
            })
        );
        assert_eq!(Restarted::parse("soon"), None);
    }

    #[test]
    fn the_last_editor_waits_for_the_new_ones_first_frame_or_its_failure() {
        let dir = folder("handover");
        let cmd = PathBuf::from(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        let args = |words: &[&str]| -> Vec<std::ffi::OsString> {
            words.iter().map(std::ffi::OsString::from).collect()
        };
        // It stops at its start: the exit code says so.
        let failed = hand_over(&cmd, &args(&["/C", "exit", "3"]), None, false, &dir);
        assert!(
            failed
                .as_ref()
                .is_err_and(|e| e.contains("stopped at its start")),
            "{failed:?}"
        );
        // It shows its first frame: it writes `up-<this process>`.
        let up = dir.join(format!("up-{}", std::process::id()));
        let path = up.to_string_lossy().into_owned();
        assert!(
            !path.contains(' '),
            "a path cmd takes without quotes: {path}"
        );
        let took = hand_over(
            &cmd,
            &args(&["/C", "type", "nul", ">", &path]),
            None,
            false,
            &dir,
        );
        assert!(took.is_ok(), "{took:?}");
        assert!(!up.exists(), "the signal is taken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_restart_goes_on_with_the_session() {
        let dir = project("handover-resume");
        let (mut world, mut ed) = start(&dir);
        move_cube(&mut ed, &mut world, 3.0);
        ed.save_session(&mut world);
        let autosave = autosave_of(&ed);
        // The restart ends the session…
        ed.end_session(&mut world);
        assert!(ed.session.is_none());
        assert!(!dir.join(".ofelia/session.lock").exists());
        // …and the new build failed at its start: this editor takes it back as it was.
        ed.resume_session(&mut world).unwrap();
        assert!(dir.join(".ofelia/session.lock").exists());
        assert_eq!(autosave_of(&ed), autosave);
        assert!(ed.session.as_ref().unwrap().autosaved.is_some());
        let mut other = Editor::new(
            [1280, 720],
            dir.clone(),
            Some(PathBuf::from("main.scene")),
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        assert!(
            other.begin_session(&mut World::new()).is_err(),
            "the lock is held again"
        );
        ed.end_session(&mut world);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What an autosave costs the UI thread (the scene's text), a restore (the text loaded)
    /// and the file's base (its hash), at 200 and 10,000 entities with a name, a transform
    /// and a model: `cargo test -p engine_editor --release --lib autosave_cost -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn autosave_cost() {
        let t = Instant::now();
        let registry = SceneRegistry::from_program();
        println!(
            "SceneRegistry::from_program: {:.3} ms",
            t.elapsed().as_secs_f64() * 1e3
        );
        for n in [200usize, 10_000] {
            let dir = folder(&format!("autosave-cost-{n}"));
            let mut world = World::new();
            for i in 0..n {
                world.spawn((
                    SceneId::new(),
                    Name::new(format!("Cube {i}")),
                    Transform::from_xyz(i as f32, 0.0, 0.0),
                    GlobalTransform::default(),
                    Model::new(builtin::CUBE, builtin::MATERIAL),
                ));
            }
            let ed = Editor::new(
                [1280, 720],
                dir.clone(),
                Some(PathBuf::from("main.scene")),
                AssetId::new(),
                AssetPaths::new(),
            )
            .unwrap();
            let best = |f: &mut dyn FnMut()| {
                (0..10)
                    .map(|_| {
                        let t = Instant::now();
                        f();
                        t.elapsed().as_secs_f64() * 1e3
                    })
                    .fold(f64::MAX, f64::min)
            };
            let mut text = String::new();
            let make = best(&mut || text = ed.scene_text(&mut world).unwrap());
            let load = best(&mut || {
                let mut w = World::new();
                engine_scene::load(&mut w, &registry, &text).unwrap();
            });
            let path = dir.join("main.scene");
            std::fs::write(&path, &text).unwrap();
            let base = best(&mut || {
                session::Base::of(&path).unwrap();
            });
            println!(
                "{n} entities, {} KiB: text {make:.2} ms, load {load:.2} ms, base {base:.2} ms",
                text.len() / 1024
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
