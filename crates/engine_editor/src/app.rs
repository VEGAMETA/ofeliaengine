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
//! - **Moving things** in the viewport (ADR 0107 E2): keyboard transforms (Q, E, R; X, Y,
//!   Z for an axis, a typed value), and the selection's gizmo (Shift+Q, E, R pick its
//!   handles). These are our keys, the default; "Editor: Key Map…" takes Blender's or
//!   Godot's.
//! - **Flying:** with the right button held, W, A, S, D fly the camera, Q down, E up,
//!   Shift faster, and the wheel sets the speed (Godot's freelook).
//! - **Picking** (ADR 0109): a click in the viewport selects the object under the pointer
//!   (or the nearest within 3 pixels), Shift or Ctrl adds it or takes it out, a click on
//!   nothing clears the selection. The selection is outlined (its children with it, the
//!   active entity lighter), and so is the object under the pointer.
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
use crate::draw::{self, Draw, Landed, Made, ROOM_WALLS, Stage};
use crate::drop::{ModelPlan, resting_point, snap_along};
use crate::faces::{self, CornerDrag, Face, FaceDrag, PartCorners};
use crate::gizmo::{
    self, COLORS, Change, Gizmo, GizmoDrag, Handle, Mode, Target, ViewProj, hit_plane,
};
use crate::handles::{Handles, Side, SideDrag};
use crate::icons;
use crate::input::{Button, InputCollector};
use crate::inspector::{self, Defaults, Inspector, Kind, Property};
use crate::paint::{UiText, paint};
use crate::session::{self, Base, Document, EditorState, LastRun, Lock, SessionFile, View, Writer};
use crate::shell::{
    Field, FieldAction, FieldEvent, FileItem, FileKind, MenuEntry, MenuItem, Palette,
    PaletteAction, PaletteRow, PaletteView, Section, ShellModel, ShellResponse, TreeItem, Widget,
    build, dismissed,
};
use crate::snap::{Quantity, Snapping};
use crate::thumbnails::{self, Subject, Thumbnails};
use crate::transform::{self, KeyTransform};
use crate::undo::{PlayChange, PlayMark, WorldSnapshot};
use crate::{Chord, CommandId, Commands, Fuzzy, History, KeyContext, Keymap, Match};
use core::any::TypeId;
use engine_app::{
    AppError, AssetMeta, Brush, BrushGroup, BrushOp, BrushShape, Game, Model, Overlay,
    OverlayOutput, OverlayPass, Player, PlayerSettings, Setup, Window, builtin,
};
use engine_core::math::{Affine3A, Quat, Vec2, Vec3, Vec4, look_at, vulkan_orthographic};
use engine_ecs::{ComponentId, Entity, GlobalTransform, QueryState, Transform, World};
use engine_input::Key;
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind};
use engine_reflect::TypeKind;
use engine_render::outline::{OutlineClass, OutlineItem, OutlineStyle, OutlineView, Outliner};
use engine_render::pick::{
    ListRequest, PickPass, PickRequest, PickView, Picker, SurfaceRequest, SurfaceResult,
};
use engine_render2d::{Canvas, ClipRect, GlyphTexture, Order, Quad, QuadMode, Renderer2d, View2d};
use engine_runtime::{Camera, Disabled, PointLight, SpotLight, Sun, look_rotation};
use engine_scene::{
    AssetId, AssetPaths, AssetRef, Name, RootOrder, SaveOptions, SceneId, SceneRegistry,
};
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

/// What the scene dock's add menu offers: each kind's name (its command, "Add …", and a new
/// entity's name), its label in its group, and its icon, the same in the menu, the tree and
/// the palette. The menu groups them ([`ADD_MESHES`], [`ADD_BLOCKOUT`], [`ADD_LIGHTS`]).
const ADD: [(&str, &str, &str); 17] = [
    ("Point", "Point", icons::NODE_3D),
    ("Cube", "Cube", icons::CUBE),
    ("Sphere", "Sphere", icons::SPHERE),
    ("Plane", "Plane", icons::PLANE),
    ("Cylinder", "Cylinder", icons::CYLINDER),
    ("Sun", "Sun", icons::SUN),
    ("Point Light", "Point Light", icons::LIGHT),
    ("Spot Light", "Spot Light", icons::SPOT_LIGHT),
    ("Camera", "Camera", icons::CAMERA),
    ("Box Brush", "Box", icons::CUBE),
    ("Wedge Brush", "Wedge", icons::WEDGE),
    ("Stairs Brush", "Stairs", icons::STAIRS),
    ("Cylinder Brush", "Cylinder", icons::CYLINDER),
    ("Cone Brush", "Cone", icons::CONE),
    ("Truncated Cone Brush", "Truncated Cone", icons::CONE),
    ("Sphere Brush", "Sphere", icons::SPHERE),
    ("Torus Brush", "Torus", icons::TORUS),
];

/// The add menu's groups: its meshes, its blockout brushes, its lights (the owner,
/// 2026-10-21: grouped, brushes too).
const ADD_MESHES: [usize; 4] = [1, 2, 3, 4];
const ADD_BLOCKOUT: [usize; 8] = [9, 10, 11, 12, 13, 14, 15, 16];
const ADD_LIGHTS: [usize; 3] = [5, 6, 7];

/// The first of the add menu's brushes.
const FIRST_BRUSH: usize = 9;

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
            player.lock_cursor(game.cursor_locked());
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
        // Nothing is asked at the exit: unsaved changes stay in the session (the scene's:
        // Play stops first).
        game.stop_playing(&mut player);
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
    let mut editor = Editor::new(
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
    let picker = Picker::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        settings.frames_in_flight,
    )?;
    let outliner = Outliner::new(setup.device, setup.bindless, output.format, output.white)?;
    // The thumbnails' atlas (ADR 0112): its pixels uploaded once the first frame records.
    let atlas = engine_render::texture::GpuTexture::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        engine_rhi::ash::vk::Format::R8G8B8A8_SRGB,
        thumbnails::ATLAS,
        thumbnails::ATLAS,
    )?;
    editor.thumbnails.set_atlas(atlas.sampled_index());
    let shared = Rc::new(RefCell::new(editor));
    // Editing: the world's time stands until Play (ADR 0107 E3), so a scene's tweens,
    // timers and sounds do not change it.
    player.set_simulating(false);
    // The project's assets, their paths to the player (ADR 0108, E4-1).
    {
        let mut editor = shared.borrow_mut();
        let report = editor.open_assets();
        log::info!(
            target: "engine_editor",
            "assets: {} files, {} read, {} .meta written, {} found again, {} gone, {} problems",
            report.files,
            report.read,
            report.metas_written,
            report.found_again,
            report.gone,
            report.problems
        );
        let paths = core::mem::take(&mut editor.pending_assets);
        player.assets_mut().add_paths(&paths);
    }
    note_panics();
    Ok((
        EditorGame {
            shared: Rc::clone(&shared),
            game: None,
            start,
            failed: None,
            stepping: false,
        },
        EditorOverlay {
            shared,
            renderer,
            glyphs,
            canvas: Canvas::new(),
            picker,
            outliner,
            atlas,
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
    disable: CommandId,
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
    /// The tool bar's tools: select (all of the gizmo), move, rotate, scale.
    tools: [CommandId; 4],
    /// The gizmo along the active entity's axes, or the world's.
    local_axes: CommandId,
    /// Keyboard transforms of the selection: move, rotate, scale.
    transforms: [CommandId; 3],
    /// Puts each selected entity on the surface below it.
    drop: CommandId,
    /// B and Shift+B: drawing a brush, a cut (ADR 0110 E2b-3); Ctrl+H rooms.
    draw: CommandId,
    draw_cut: CommandId,
    hollow: CommandId,
    /// Tab: editing the selected brushes' faces (E2b-5).
    edit_faces: CommandId,
    /// A blockout group written as a glTF beside the scene (E2b-6).
    export: CommandId,
    /// Blockout groups baked into glTF files drawn in their place, and back (E2b-6).
    bake: CommandId,
    unbake: CommandId,
    /// Play and Stop (ADR 0107 E3), and the controls while playing.
    play: CommandId,
    stop: CommandId,
    play_from_here: CommandId,
    restart_play: CommandId,
    pause: CommandId,
    step: CommandId,
    eject: CommandId,
    /// Keeping changes made while playing (E3-3).
    keep: CommandId,
    keep_list: CommandId,
    /// Where Play shows the game (E3-4).
    play_where: CommandId,
    /// The snapping steps (position, angle, scale), each a list of values in the palette;
    /// the next pivot.
    steps: [CommandId; 3],
    pivot: CommandId,
    /// The viewport's overlays: icons of lights and cameras, and the selected ones' reach.
    overlays: CommandId,
    /// The blockout's colliders over the viewport (ADR 0110 E2b-4).
    colliders: CommandId,
    /// The navigation mesh baked from the brushes, over the viewport (E2b-4b).
    navigation: CommandId,
    /// Which keys the editor uses: Blender's or Godot's.
    keys: CommandId,
    /// Whether clicks pick transparent surfaces or through them.
    pick_transparent: CommandId,
    /// The camera: frame the selection (or the scene), follow the active entity, and the
    /// bookmarks' nine places, gone to and set.
    frame: CommandId,
    follow: CommandId,
    go_bookmark: [CommandId; 9],
    set_bookmark: [CommandId; 9],
    /// "Add Cube" and the rest: the add menu's kinds, in its order.
    add_kinds: [CommandId; ADD.len()],
}

/// The editor's commands.
fn commands() -> (Commands, CommandIds) {
    let mut c = Commands::new();
    let ids = CommandIds {
        undo: c.register("edit.undo", "Edit", "Undo"),
        redo: c.register("edit.redo", "Edit", "Redo"),
        save: c.register("file.save", "File", "Save Scene"),
        delete: c.register("scene.delete", "Scene", "Delete Selection"),
        disable: c.register("scene.disable", "Scene", "Disable or Enable Selection"),
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
        tools: [
            c.register("tool.select", "Tool", "Select"),
            c.register("tool.move", "Tool", "Move"),
            c.register("tool.rotate", "Tool", "Rotate"),
            c.register("tool.scale", "Tool", "Scale"),
        ],
        local_axes: c.register("tool.local_axes", "Tool", "Local Axes"),
        transforms: [
            c.register("transform.move", "Transform", "Move Selection"),
            c.register("transform.rotate", "Transform", "Rotate Selection"),
            c.register("transform.scale", "Transform", "Scale Selection"),
        ],
        drop: c.register("transform.drop", "Transform", "Drop to the Surface Below"),
        draw: c.register("blockout.draw", "Blockout", "Draw Brush"),
        draw_cut: c.register("blockout.draw_cut", "Blockout", "Draw Cut"),
        hollow: c.register("blockout.hollow", "Blockout", "Room or Solid"),
        edit_faces: c.register("blockout.edit_faces", "Blockout", "Edit Faces"),
        export: c.register("blockout.export", "Blockout", "Export to glTF"),
        bake: c.register("blockout.bake", "Blockout", "Bake to Mesh"),
        unbake: c.register("blockout.unbake", "Blockout", "Unbake"),
        play: c.register("play.play", "Play", "Play Scene"),
        stop: c.register("play.stop", "Play", "Stop"),
        play_from_here: c.register("play.from_here", "Play", "Play from Here"),
        restart_play: c.register("play.restart", "Play", "Restart"),
        pause: c.register("play.pause", "Play", "Pause or Go On"),
        step: c.register("play.step", "Play", "Step One Tick"),
        eject: c.register("play.eject", "Play", "Eject or Return to the Game"),
        keep: c.register("play.keep", "Play", "Keep the Selection's Changes"),
        keep_list: c.register("play.keep_list", "Play", "Keep Changes…"),
        play_where: c.register("play.where", "Play", "In a Window or in the Viewport"),
        steps: [
            c.register("snap.position_step", "Snapping", "Position Step…"),
            c.register("snap.angle_step", "Snapping", "Angle Step…"),
            c.register("snap.scale_step", "Snapping", "Scale Step…"),
        ],
        pivot: c.register("transform.pivot", "Transform", "Next Pivot"),
        overlays: c.register("view.overlays", "View", "Overlays"),
        colliders: c.register("view.colliders", "View", "Colliders"),
        navigation: c.register("view.navigation", "View", "Navigation"),
        keys: c.register("editor.keys", "Editor", "Key Map…"),
        pick_transparent: c.register(
            "viewport.pick_transparent",
            "Viewport",
            "Pick Transparent Surfaces",
        ),
        frame: c.register("view.frame", "View", "Frame Selection"),
        follow: c.register("view.follow", "View", "Follow Selection"),
        go_bookmark: core::array::from_fn(|i| {
            let n = i + 1;
            c.register(
                &format!("view.bookmark.{n}"),
                "View",
                &format!("Go to Bookmark {n}"),
            )
        }),
        set_bookmark: core::array::from_fn(|i| {
            let n = i + 1;
            c.register(
                &format!("view.set_bookmark.{n}"),
                "View",
                &format!("Set Bookmark {n}"),
            )
        }),
        add_kinds: ADD.map(|(kind, _, _)| {
            let name = format!("scene.add.{}", kind.to_lowercase().replace(' ', "_"));
            c.register(&name, "Scene", &format!("Add {kind}"))
        }),
    };
    (c, ids)
}

/// The shortcuts of key map `preset`, and the shortcut the palette shows beside each
/// command (its first).
fn keymap(ids: &CommandIds, preset: KeyPreset) -> (Keymap, HashMap<CommandId, String>) {
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
        ("H", ids.disable),
        ("Ctrl+D", ids.duplicate),
        ("Ctrl+A", ids.add),
        ("F2", ids.rename),
        ("Ctrl+Shift+P", ids.palette),
        ("Ctrl+P", ids.find),
        ("Ctrl+R", ids.restart),
        ("Space", ids.local_axes),
        ("F", ids.frame),
        ("Shift+F", ids.follow),
        ("End", ids.drop),
        ("Period", ids.pivot),
        ("B", ids.draw),
        ("Shift+B", ids.draw_cut),
        ("Ctrl+H", ids.hollow),
        ("Tab", ids.edit_faces),
        // Godot's keys (`docs/research/ux/play-mode.md` §5), Unreal's "Play from Here" on
        // Shift+F5 and its eject beside Stop; F9 to F11 stay free for the script debugger.
        ("F5", ids.play),
        ("F8", ids.stop),
        ("Shift+F5", ids.play_from_here),
        ("Ctrl+F5", ids.restart_play),
        ("F7", ids.pause),
        ("Shift+F7", ids.step),
        ("Shift+F8", ids.eject),
        // Unreal's K, with Ctrl: plain letters are the game's while it plays.
        ("Ctrl+K", ids.keep),
        ("Ctrl+Shift+K", ids.keep_list),
    ];
    // The transforms and the tools (`docs/research/ux/viewport-tools.md` §2):
    // - ours: Q, E, R at once and with Shift the gizmo's handles, the left hand beside
    //   W, A, S, D (the camera's flight);
    // - Blender's: G, R, S, the tools on Q, W, E, T;
    // - Godot's: the tools on Q, W, E, R, G transforming (R and S then switch it,
    //   `Editor::key_transform_frame`).
    let preset_keys: &[(&str, CommandId)] = match preset {
        KeyPreset::Ofelia => &[
            // G for the grid: Y is an axis in a keyboard transform (the owner, 2026-10-21).
            ("G", ids.snap),
            ("Q", ids.transforms[0]),
            ("R", ids.transforms[1]),
            ("E", ids.transforms[2]),
            ("Shift+Q", ids.tools[1]),
            ("Shift+R", ids.tools[2]),
            ("Shift+E", ids.tools[3]),
        ],
        KeyPreset::Blender => &[
            ("Shift+Tab", ids.snap),
            ("G", ids.transforms[0]),
            ("R", ids.transforms[1]),
            ("S", ids.transforms[2]),
            ("Q", ids.tools[0]),
            ("W", ids.tools[1]),
            ("E", ids.tools[2]),
            ("T", ids.tools[3]),
        ],
        KeyPreset::Godot => &[
            ("Y", ids.snap),
            ("G", ids.transforms[0]),
            ("Q", ids.tools[0]),
            ("W", ids.tools[1]),
            ("E", ids.tools[2]),
            ("R", ids.tools[3]),
        ],
    };
    let global = global
        .into_iter()
        .chain(preset_keys.iter().copied())
        .map(|(chord, id)| (chord, id, KeyContext::Global));
    for (chord, id, context) in global.chain(tree) {
        if let Some(chord) = Chord::parse(chord) {
            let _ = keymap.bind(chord, context, id);
            shortcuts.entry(id).or_insert_with(|| chord.to_string());
        }
    }
    // The camera's bookmarks, as Unreal has them: Ctrl and a digit sets one, the digit
    // goes there.
    for n in 1..=9 {
        for (chord, id) in [
            (format!("{n}"), ids.go_bookmark[n - 1]),
            (format!("Ctrl+{n}"), ids.set_bookmark[n - 1]),
        ] {
            if let Some(chord) = Chord::parse(&chord) {
                let _ = keymap.bind(chord, KeyContext::Global, id);
                shortcuts.entry(id).or_insert_with(|| chord.to_string());
            }
        }
    }
    (keymap, shortcuts)
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
    Keys(KeyPreset),
    /// Selects this entity (Alt+click's list of what lies under the pointer).
    Select(Entity),
    /// Sets a snapping step (0 position, 1 angle, 2 scale) to this value.
    Step(usize, f64),
    /// Keeps a change made while playing, or not (E3-3).
    Keep(KeepItem),
}

/// What a line of the "Keep Changes…" list keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepItem {
    /// Every change, or none when every one is kept.
    All,
    /// An entity's changes (or the entity, made while playing).
    Entity(Entity),
    /// A scene entity's going.
    Gone(SceneId),
}

/// What a list of what lies under the pointer is for (ADR 0109).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListUse {
    /// A plain click where the last one was: the next object behind the active one.
    Cycle,
    /// Alt+click: every one of them, in the palette.
    Offer,
}

/// How far from the pointer's pixel a pick still takes an object, in the view's pixels:
/// thin objects stay easy to hit.
const PICK_RADIUS: u32 = 3;

/// A box dragged in the viewport (ADR 0109): from where the left button went down to the
/// pointer, in the window's pixels.
#[derive(Debug, Clone)]
struct BoxDrag {
    from: Vec2,
    to: Vec2,
    /// The last box asked for, and the frame it was asked in while its answer is to come:
    /// one at a time, so a slow GPU draws a box every few frames, not every frame.
    asked: Option<ListRequest>,
    waiting: Option<u64>,
    /// What its newest answer takes, front to back (outlined while dragging).
    taken: Vec<Entity>,
    /// Released: how it changes the selection, once the answer for its last box, drawn
    /// from the frame given on, arrives.
    released: Option<(BoxMode, u64)>,
}

/// What a box does to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxMode {
    /// Alone: it becomes the selection.
    Replace,
    /// With Shift: added.
    Add,
    /// With Ctrl: taken out.
    Remove,
}

/// Frames a box's answer may take before it is asked again (a pick the GPU could not
/// take).
const BOX_PATIENCE: u64 = 8;

/// An entity without geometry the viewport shows as an icon (ADR 0107 E2-7): a light or a
/// camera, at its place on the screen (physical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewIcon {
    entity: Entity,
    glyph: &'static str,
    at: Vec2,
}

/// How far from an icon's middle the pointer takes it, in logical pixels.
const ICON_REACH: f32 = 12.0;
/// How far a camera's pyramid reaches, in meters (its far plane is often a kilometer off).
const CAMERA_REACH: f32 = 2.0;
/// The sun's direction's length, in meters.
const SUN_REACH: f32 = 1.5;

/// The icon of an entity the viewport shows as one: a camera's or a light's; none for the
/// others (they have geometry, or nothing to show).
fn icon_of(world: &World, e: Entity) -> Option<&'static str> {
    let (glyph, kind) = kind_of(world, e);
    matches!(kind, "Camera" | "Sun" | "Light").then_some(glyph)
}

/// A world segment's part in front of the eye, on the screen.
fn segment_on_screen(view: &ViewProj, a: Vec3, b: Vec3) -> Option<(Vec2, Vec2)> {
    let near = 1e-2;
    let (da, db) = (
        (a - view.eye).dot(view.forward),
        (b - view.eye).dot(view.forward),
    );
    if da < near && db < near {
        return None;
    }
    let (mut a, mut b) = (a, b);
    if da < near {
        a += (b - a) * ((near - da) / (db - da));
    }
    if db < near {
        b += (a - b) * ((near - db) / (da - db));
    }
    Some((view.screen(a)?, view.screen(b)?))
}

/// What rotating and scaling turn about (ADR 0107 E2-6; the UX note's pivots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Pivot {
    /// The middle of the selected entities no selected entity carries.
    #[default]
    Median,
    /// Each entity its own origin, the gizmo in their middle.
    Origins,
    /// The active entity's origin.
    Active,
}

impl Pivot {
    /// Its name in the session.
    fn name(self) -> &'static str {
        match self {
            Self::Median => "median",
            Self::Origins => "origins",
            Self::Active => "active",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Self::Median, Self::Origins, Self::Active]
            .into_iter()
            .find(|p| p.name() == name)
    }

    /// What the tool bar's button says.
    fn label(self) -> &'static str {
        match self {
            Self::Median => "Pivot: middle",
            Self::Origins => "Pivot: own origins",
            Self::Active => "Pivot: active",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Median => Self::Origins,
            Self::Origins => Self::Active,
            Self::Active => Self::Median,
        }
    }
}

/// The values each step's menu offers: positions (meters), angles (degrees), scales.
const STEP_VALUES: [&[f64]; 3] = [
    &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0],
    &[1.0, 5.0, 10.0, 15.0, 22.5, 30.0, 45.0, 90.0],
    &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0],
];

/// How a step reads (0 position, 1 angle, 2 scale).
fn step_label(kind: usize, value: f64) -> String {
    match kind {
        0 => format!("{value} m"),
        1 => format!("{value}°"),
        _ => format!("{value}"),
    }
}

/// The steps' menus' entries, as they read.
const STEP_LABELS: [&[&str]; 3] = [
    &[
        "0.01 m", "0.05 m", "0.1 m", "0.25 m", "0.5 m", "1 m", "2 m", "5 m",
    ],
    &["1°", "5°", "10°", "15°", "22.5°", "30°", "45°", "90°"],
    &["0.01", "0.05", "0.1", "0.25", "0.5", "1"],
];

/// End's drops (ADR 0107 E2-5): the entities still to drop, their shared undo step, and
/// the one being dropped.
#[derive(Debug, Clone)]
struct Dropping {
    queue: Vec<Entity>,
    merge: u64,
    current: Option<DropOne>,
}

/// One entity being dropped: its instances' slots with its children's (none: not named
/// yet), its place (the box of an entity without an instance), the frame its surface was
/// asked in with the box's bottom it measured from, and the surface found (none: nothing
/// below).
#[derive(Debug, Clone)]
struct DropOne {
    entity: Entity,
    slots: Option<Vec<u32>>,
    place: Vec3,
    asked: Option<(u64, f32)>,
    found: Option<Option<engine_render::pick::SurfaceHit>>,
}

/// How long the camera takes to fly to a framing or a bookmark, in seconds.
const CAMERA_MOVE: f32 = 0.2;
/// The least radius a framing keeps around what it frames: a light or an empty entity is
/// a point.
const FRAME_RADIUS: f32 = 0.5;

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

/// Which keys the editor uses. Ours by default (team decision, 2026-10-21): Q, E, R move,
/// scale and rotate at once, Shift+Q, E, R pick the gizmo's handles, and W, A, S, D fly the
/// camera while the right button is held. Or Blender's (G, R, S at once; Q, W, E, T the
/// tools), or Godot's (Q, W, E, R the tools; G moves, then R rotates and S scales).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum KeyPreset {
    #[default]
    Ofelia,
    Blender,
    Godot,
}

impl KeyPreset {
    const ALL: [Self; 3] = [Self::Ofelia, Self::Blender, Self::Godot];

    /// Its name in the session file.
    fn name(self) -> &'static str {
        match self {
            Self::Ofelia => "ofelia",
            Self::Blender => "blender",
            Self::Godot => "godot",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ofelia => {
                "ofeliaengine's keys: Q, E, R move, scale, rotate; Shift+Q, E, R the gizmo"
            }
            Self::Blender => "Blender's keys: G, R, S move, rotate, scale; Q, W, E, T the tools",
            Self::Godot => "Godot's keys: Q, W, E, R the tools; G moves (then R rotates, S scales)",
        }
    }

    /// The letters that start a keyboard transform's move, turn and scale, and switch one
    /// to another while it runs (Win32 virtual keys).
    fn transform_letters(self) -> [u16; 3] {
        match self {
            // Q, R, E.
            Self::Ofelia => [0x51, 0x52, 0x45],
            // G, R, S.
            Self::Blender | Self::Godot => [0x47, 0x52, 0x53],
        }
    }
}

/// The camera's flight (the right button held, W, A, S, D): its speed at first in meters
/// per second, the factor a wheel notch changes it by and its bounds, and Shift's factor.
const FLY_SPEED: f32 = 5.0;
const FLY_NOTCH: f32 = 1.2;
const FLY_RANGE: (f32, f32) = (0.05, 1000.0);
const FLY_FAST: f32 = 3.0;

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
    /// After `#`: the assets matching the query, best first.
    assets: Vec<FoundAsset>,
    highlights: Vec<u16>,
    fuzzy: Fuzzy,
    /// The scene's entities in tree order, and a stack for walking it (kept: searching
    /// allocates nothing once they have grown).
    order: Vec<Entity>,
    stack: Vec<Entity>,
    /// A list to pick from instead.
    choices: Option<Choices>,
    /// The inspector field (section, field) an asset picked here goes to, instead of the
    /// FileSystem dock.
    for_field: Option<(usize, usize)>,
}

impl CommandPalette {
    /// Whether the query looks for the scene's objects (`@`) rather than commands.
    fn finds_objects(&self) -> bool {
        self.choices.is_none() && self.query.trim_start().starts_with('@')
    }

    /// Whether the query looks for the project's assets (`#`).
    fn finds_assets(&self) -> bool {
        self.choices.is_none() && self.query.trim_start().starts_with('#')
    }

    /// How many results there are.
    fn len(&self) -> usize {
        if let Some(choices) = &self.choices {
            choices.shown.len()
        } else if self.finds_objects() {
            self.objects.len()
        } else if self.finds_assets() {
            self.assets.len()
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
    /// The project's assets (ADR 0108, E4-1): every asset file with its id, kept live by
    /// watching the folder; a change scanned once the folder has been quiet a moment.
    asset_db: engine_scene::AssetDatabase,
    asset_watch: Option<engine_platform::fs_watch::DirectoryWatcher>,
    assets_changed: Option<Instant>,
    /// The asset browser (the FileSystem dock, E4-2).
    browser: Browser,
    /// A file dragged out of the browser (E4-3).
    asset_drag: Option<AssetDrag>,
    /// The listing's thumbnails (ADR 0112), the side of its tiles' pictures (logical
    /// pixels), and where its scroll area was last frame: rectangle and scroll.
    thumbnails: Thumbnails,
    tile: f32,
    listing: Option<(engine_ui::Rect, f32)>,
    /// Asset files written anew since the player was told (E4-5): made again.
    changed_assets: Vec<AssetId>,
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
    /// The add menu's group opened beside it.
    add_group: Option<usize>,
    /// Drag gestures so far (each its own undo step).
    gestures: u64,
    /// The viewport's transform gizmo (ADR 0107 E2), laid out this frame when shown.
    gizmo: Gizmo,
    gizmo_shown: bool,
    gizmo_drag: Option<GizmoDrag>,
    /// Editing brushes' faces (Tab, ADR 0110 E2b-5).
    face_edit: Option<FaceEdit>,
    /// A blockout group to write as a glTF once the player's frame is at hand (E2b-6).
    export_group: Option<Entity>,
    /// Blockout groups to bake once the player's frame is at hand (E2b-6).
    bake_groups: Vec<Entity>,
    /// Playing (ADR 0107 E3): the scene's world as Play found it, put back at Stop.
    play: Option<Playing>,
    /// Play shows the game in a window of its own (the UX note's default), else in the
    /// viewport ("Play: In a Window or in the Viewport").
    play_in_window: bool,
    /// The game plays in a window of its own, and that window (none offscreen): its events
    /// are told apart by it.
    in_view_window: bool,
    view_window: Option<engine_platform::window::WindowId>,
    /// Play or Stop asked for, done once the player's frame is at hand.
    play_request: Option<PlayRequest>,
    /// The active brush's side handles (ADR 0110 E2b-3c), the one under the pointer, and
    /// a side being dragged.
    handles: Handles,
    hovered_side: Option<Side>,
    side_drag: Option<SideDrag>,
    /// The gizmo's handle under the pointer.
    hovered_handle: Option<Handle>,
    /// The gizmo along the active entity's axes (Space), else the world's.
    local_axes: bool,
    /// Which keys the keymap has.
    key_preset: KeyPreset,
    /// A keyboard transform while it runs (ADR 0107 E2-2).
    key_transform: Option<KeyTransform>,
    /// Its guides this frame (an axis' line, or a plane's two, by color index; 3 the
    /// pivot's line to the pointer) and its hint by the pointer.
    guides: Vec<(Vec2, Vec2, usize)>,
    hint: String,
    /// The blockout's colliders shown over the viewport (View: Colliders), their edges in
    /// the world as the player last evaluated them, and what those were made from (each
    /// group's epoch and place) to make them again only when that changes.
    show_colliders: bool,
    collider_edges: Vec<(Vec3, Vec3)>,
    collider_key: BlockoutKey,
    /// The navigation mesh shown over the viewport (View: Navigation, E2b-4b): its edges as
    /// last baked (their ends, and whether each is a boundary), what that bake was made
    /// from, a bake running in the background, and since when the brushes have been as they
    /// are now (a bake waits a moment after an edit).
    show_nav: bool,
    nav_edges: Vec<(Vec3, Vec3, bool)>,
    nav_key: BlockoutKey,
    nav_job: Option<std::sync::mpsc::Receiver<Result<NavBaked, String>>>,
    nav_changed: Option<(Instant, BlockoutKey)>,
    /// The bakes' own workers, made with the first: the player's pool keeps its frames.
    nav_pool: Option<std::sync::Arc<engine_core::jobs::ThreadPool>>,
    /// A brush being drawn (B, Shift+B), and its brush once it has a size, as last made.
    draw: Option<Draw>,
    drawn: Option<(Entity, Made)>,
    /// What the overlay shows of it this frame: lines (the mark, the box's edges) and
    /// sizes on the screen; the edges and sizes in the world, kept for their storage.
    draw_lines: Vec<(Vec2, Vec2)>,
    draw_labels: Vec<(Vec2, String)>,
    draw_edges: Vec<(Vec3, Vec3)>,
    draw_sizes: Vec<(Vec3, f32)>,
    /// Where the drawing's point landed on another brush this frame (on the screen; a
    /// corner, else an edge), and the corners and edges it can land on.
    draw_landed: Option<(Vec2, bool)>,
    land_corners: Vec<Vec3>,
    land_edges: Vec<(Vec3, Vec3)>,
    brush_boxes: Option<QueryState<(Entity, &'static Brush, &'static GlobalTransform)>>,
    /// The camera's flight speed (the wheel while flying), meters per second.
    fly_speed: f32,
    /// When the last frame was: flying moves by the time since.
    last_frame: Instant,
    /// Picking in the viewport (ADR 0109): the pixel the overlay picks at this frame (at the
    /// view's render size), and whether blended surfaces can be picked.
    pick_request: Option<PickRequest>,
    pick_transparent: bool,
    /// The view's render size over its output size, and the frame's number, as the overlay
    /// last saw them.
    view_scale: [f32; 2],
    frame_seen: u64,
    /// The newest pick the overlay took (its pixel, its frame, the instance there or
    /// nearest), until [`Editor::resolve_pick`] names its entity.
    picked: Option<(PickRequest, u64, Option<u32>)>,
    /// The entity under the pointer as the newest pick saw it (none: nothing), and that
    /// pick's pixel.
    hovered: Option<(PickRequest, Option<Entity>)>,
    /// What the last hover pick saw: the pixel, the camera, the history's head (when none
    /// of them changes, the pick is not made again).
    last_pick: Option<(PickRequest, [f32; 6], u64)>,
    /// Where the left button went down in the viewport, not on a handle: a release near it
    /// is a click.
    press_at: Option<Vec2>,
    /// A click waiting for the pick of its pixel: the pixel, whether it toggles (Shift or
    /// Ctrl), and the first frame whose pick can answer it.
    pending_click: Option<(PickRequest, bool, u64)>,
    /// What lies under a pixel: the list the overlay picks this frame; the newest one it
    /// took (its request, its frame, the slots front to back) until
    /// [`Editor::resolve_list`] names their entities; and a click waiting for one, what
    /// for, and the first frame whose list can answer it.
    list_request: Option<ListRequest>,
    listed: Option<(ListRequest, u64, Vec<u32>)>,
    pending_list: Option<(ListRequest, ListUse, u64)>,
    /// Where the waiting click is, and where the last plain click was with the entity it
    /// left active: a plain click there again cycles behind it.
    click_at: Option<Vec2>,
    last_view_click: Option<(Vec2, Entity)>,
    /// Alt+click's list, nearest first, offered in the next frame (the palette needs the
    /// world).
    under_pointer: Option<Vec<Entity>>,
    /// A box being dragged.
    box_drag: Option<BoxDrag>,
    /// Framing (F): asked for, with the bounds of the selected entities' places (none:
    /// frame the scene); the bounds the overlay found with their world boxes; the camera's
    /// move toward a framing or a bookmark (from, to, since).
    frame_asked: Option<Option<(Vec3, Vec3)>>,
    framed: Option<(Vec3, Vec3)>,
    camera_move: Option<(OrbitCamera, OrbitCamera, Instant)>,
    /// Shift+F: the entity the camera follows and where it last was.
    follow: Option<(Entity, Vec3)>,
    /// The camera's bookmarks, 1 to 9 (the document's; the session keeps them).
    bookmarks: [Option<OrbitCamera>; 9],
    /// Surfaces (ADR 0107 E2-5): the surface under the pointer the overlay picks this
    /// frame (at the render size), the newest one it found, and the selection's world box
    /// as it last found it; End's drops.
    surface_request: Option<SurfaceRequest>,
    surfaced: Option<SurfaceResult>,
    selection_box: Option<(Vec3, Vec3)>,
    dropping: Option<Dropping>,
    /// What rotating and scaling turn about; the snapping steps as the tool bar reads
    /// them (kept: no text made each frame), and the step whose menu is open.
    pivot: Pivot,
    step_labels: [String; 3],
    step_menu: Option<usize>,
    /// The viewport's overlays (E2-7): shown or not (the session keeps it); the icons of
    /// this frame and the one under the pointer; the selected lights' and cameras' reach as
    /// lines on the screen (physical pixels: from, to, and whether a camera's).
    overlays: bool,
    view_icons: Vec<ViewIcon>,
    icon_hover: Option<Entity>,
    reach_lines: Vec<(Vec2, Vec2, bool)>,
    /// What the overlay outlines (ADR 0109): the GPU scene's slots of the selection, its
    /// children and the hovered entity, as [`Editor::outline`] last found them.
    outline_items: Vec<OutlineItem>,
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
    } else if let Some(brush) = world.get::<Brush>(e) {
        if brush.is_room() {
            return (icons::ROOM, "Room");
        }
        (
            brush_icon(brush),
            match brush.op {
                BrushOp::Add => "Brush",
                BrushOp::Cut => "Cut",
                BrushOp::Intersect => "Intersect",
            },
        )
    } else if world.get::<BrushGroup>(e).is_some() {
        (icons::BLOCKOUT, "Blockout")
    } else {
        (icons::NODE_3D, "Entity")
    }
}

/// Whether `e` is off: disabled, or below a disabled entity (the engine's `Inactive`
/// follows a tick later; the tree shows it at once).
fn is_off(world: &World, e: Entity) -> bool {
    let mut at = Some(e);
    while let Some(x) = at {
        if world.get::<Disabled>(x).is_some() {
            return true;
        }
        at = world.parent(x);
    }
    false
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

/// A brush's icon: its shape's.
fn brush_icon(brush: &Brush) -> &'static str {
    match brush.shape {
        BrushShape::Box => icons::CUBE,
        BrushShape::Wedge => icons::WEDGE,
        BrushShape::Stairs => icons::STAIRS,
        BrushShape::Cylinder => icons::CYLINDER,
        BrushShape::Cone => icons::CONE,
        BrushShape::Sphere => icons::SPHERE,
        BrushShape::Torus => icons::TORUS,
        BrushShape::Planes => icons::MODEL,
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
        9.. => {
            let brush = match what {
                9 => Brush::new(BrushShape::Box, Vec3::splat(2.0)),
                10 => Brush::new(BrushShape::Wedge, Vec3::new(2.0, 1.0, 3.0)),
                11 => Brush::new(BrushShape::Stairs, Vec3::new(2.0, 1.6, 3.0)),
                12 => Brush::new(BrushShape::Cylinder, Vec3::splat(2.0)),
                13 => Brush::new(BrushShape::Cone, Vec3::splat(2.0)),
                14 => Brush::new(BrushShape::Cone, Vec3::splat(2.0)).with_top(0.5),
                15 => Brush::new(BrushShape::Sphere, Vec3::splat(2.0)),
                _ => Brush::new(BrushShape::Torus, Vec3::new(3.0, 0.8, 3.0)),
            };
            world.insert(e, brush);
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
        let (commands, ids) = commands();
        let (keymap, shortcuts) = keymap(&ids, KeyPreset::default());
        let thumbnails = Thumbnails::new(&root);
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
            root: root.clone(),
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
            asset_db: engine_scene::AssetDatabase::default(),
            asset_watch: None,
            assets_changed: None,
            browser: Browser::default(),
            asset_drag: None,
            thumbnails,
            tile: DEFAULT_TILE,
            listing: None,
            changed_assets: Vec::new(),
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
            add_group: None,
            gestures: 0,
            gizmo: Gizmo::default(),
            gizmo_shown: false,
            gizmo_drag: None,
            face_edit: None,
            export_group: None,
            bake_groups: Vec::new(),
            play: None,
            play_in_window: true,
            in_view_window: false,
            view_window: None,
            play_request: None,
            handles: Handles::default(),
            hovered_side: None,
            side_drag: None,
            hovered_handle: None,
            local_axes: false,
            key_preset: KeyPreset::default(),
            key_transform: None,
            guides: Vec::new(),
            hint: String::new(),
            show_colliders: false,
            collider_edges: Vec::new(),
            collider_key: Vec::new(),
            show_nav: false,
            nav_edges: Vec::new(),
            nav_key: Vec::new(),
            nav_job: None,
            nav_changed: None,
            nav_pool: None,
            draw: None,
            drawn: None,
            draw_lines: Vec::new(),
            draw_labels: Vec::new(),
            draw_edges: Vec::new(),
            draw_sizes: Vec::new(),
            draw_landed: None,
            land_corners: Vec::new(),
            land_edges: Vec::new(),
            brush_boxes: None,
            fly_speed: FLY_SPEED,
            last_frame: Instant::now(),
            pick_request: None,
            pick_transparent: true,
            view_scale: [1.0, 1.0],
            frame_seen: 0,
            picked: None,
            hovered: None,
            last_pick: None,
            press_at: None,
            pending_click: None,
            list_request: None,
            listed: None,
            pending_list: None,
            click_at: None,
            last_view_click: None,
            under_pointer: None,
            box_drag: None,
            frame_asked: None,
            framed: None,
            camera_move: None,
            follow: None,
            bookmarks: [None; 9],
            surface_request: None,
            surfaced: None,
            selection_box: None,
            dropping: None,
            pivot: Pivot::default(),
            overlays: true,
            view_icons: Vec::new(),
            icon_hover: None,
            reach_lines: Vec::new(),
            step_labels: {
                let d = Snapping::default();
                [
                    step_label(0, d.position),
                    step_label(1, d.angle),
                    step_label(2, d.scale),
                ]
            },
            step_menu: None,
            outline_items: Vec::new(),
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
        // The game's window: its F7 and F8 are Play's; the rest is the game's.
        if self.view_window.is_some() && Some(event.window) == self.view_window {
            if let WindowEventKind::Key {
                virtual_key,
                pressed: true,
                repeat: false,
                ..
            } = event.kind
            {
                match virtual_key {
                    0x76 => self.play_request = Some(PlayRequest::Pause),
                    0x77 => self.play_request = Some(PlayRequest::Stop),
                    _ => {}
                }
            }
            return;
        }
        let input = &mut self.input;
        match event.kind {
            WindowEventKind::Resized { width, height } if width > 0 && height > 0 => {
                self.size = [width, height];
            }
            WindowEventKind::ScaleChanged { dpi } => self.scale = dpi as f32 / 96.0,
            WindowEventKind::Focused(focused) => {
                if !focused {
                    input.focus_lost();
                    if let Some(t) = &mut self.key_transform {
                        t.cancel = true;
                    }
                    if let Some(d) = &mut self.draw {
                        d.cancel = true;
                    }
                }
                self.focused = focused;
            }
            WindowEventKind::CursorMoved { x, y } => input.cursor_moved(x as f32, y as f32),
            WindowEventKind::CursorDelta { dx, dy } => input.cursor_delta(dx as f32, dy as f32),
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
        // The frame's time, for flying: at most a tenth of a second, so a stall does not
        // throw the camera.
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        let playing = self.play.is_some();
        // The session keeps the scene, not the play world: no autosave while playing.
        if !playing {
            self.tick_session(world);
        }
        self.tick_builds(world);
        self.tick_assets();
        self.thumbnails.begin_frame();
        if let Some(under) = self.under_pointer.take() {
            self.offer_under_pointer(world, under);
        }
        // The editor's camera while editing or ejected; the game's cameras in the game's
        // view.
        let game_view = self.game_view();
        if game_view && let Some(camera) = self.camera.take() {
            world.despawn(camera);
        }
        let camera = (!game_view).then(|| self.camera(world));
        self.collect_rows(world);
        self.frames += 1;
        let elapsed = self.since.elapsed().as_secs_f64();
        if elapsed >= 0.5 {
            self.fps = self.frames as f64 / elapsed;
            self.frames = 0;
            self.since = Instant::now();
        }
        // A keyboard transform has the frame's keys and clicks while it runs.
        if self.key_transform.is_some() {
            self.key_transform_frame(world);
        }
        // So does a brush being drawn.
        if self.draw.is_some() {
            self.draw_frame(world);
        }
        // Tab over the viewport edits faces, as in Blender; over the panels, or while a
        // field is typed in, it goes to the next field.
        let over_view = self.view_rect.is_some_and(|[x, y, w, h]| {
            self.input.cursor().is_some_and(|[cx, cy]| {
                cx >= x as f32 && cy >= y as f32 && cx < (x + w) as f32 && cy < (y + h) as f32
            })
        });
        if over_view
            && !game_view
            && self.ui.editing().is_none()
            && self.key_transform.is_none()
            && self
                .input
                .chords()
                .iter()
                .any(|c| c.key == Key::Tab && !c.ctrl && !c.shift && !c.alt)
        {
            self.input.consume_keys();
            let id = self.ids.edit_faces;
            self.run(world, id);
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
                    Kind::Text if engine_scene::text::is_asset_ref(p.ty) => {
                        let entry = p.asset.and_then(|a| self.asset_db.get(a.file));
                        let field = self.inspector.name(p);
                        let preview = p.asset.filter(|a| !a.is_none()).and_then(|a| {
                            let subject = field_subject(a, field, entry.map(|e| e.kind));
                            // A file's as the database saw it; the engine's never change.
                            let (path, kind, key) = match entry {
                                Some(e) => (
                                    e.path.as_str(),
                                    e.kind,
                                    thumbnails::Key {
                                        bytes: e.size,
                                        modified: e.modified,
                                    },
                                ),
                                None if a.file == builtin::CUBE.file => (
                                    "",
                                    engine_scene::AssetKind::Model,
                                    thumbnails::Key {
                                        bytes: 0,
                                        modified: 0,
                                    },
                                ),
                                None => return None,
                            };
                            self.thumbnails.image(subject, path, kind, key)
                        });
                        let icon = match (p.asset, entry) {
                            (Some(a), _) if a.is_none() => "",
                            (_, Some(e)) => file_icon_of(e.kind),
                            _ => icons::MESH,
                        };
                        Widget::Asset {
                            preview,
                            icon,
                            name: first.rsplit('/').next().unwrap_or(first),
                        }
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
                let kind = match &self.play {
                    Some(p) if p.keep.contains(&row.entity) => "Kept",
                    _ => kind,
                };
                TreeItem {
                    depth: row.depth,
                    icon,
                    name: world.get::<Name>(row.entity).map_or(kind, Name::as_str),
                    kind,
                    expanded: row.children.then(|| !self.collapsed.contains(&row.entity)),
                    selected: in_selection(row.entity),
                    active: Some(row.entity) == self.active(),
                    renaming: Some(row.entity) == self.renaming,
                    disabled: world.get::<Disabled>(row.entity).is_some(),
                    off: is_off(world, row.entity),
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
        let leaf = |k: usize| MenuEntry {
            icon: ADD[k].2,
            label: ADD[k].1,
            kind: k,
            children: &[],
        };
        let meshes = ADD_MESHES.map(leaf);
        let blockout = ADD_BLOCKOUT.map(leaf);
        let lights = ADD_LIGHTS.map(leaf);
        let group = |icon, label, children| MenuEntry {
            icon,
            label,
            kind: usize::MAX,
            children,
        };
        let add_items = [
            leaf(0),
            group(icons::MODEL, "Mesh", &meshes[..]),
            group(icons::BLOCKOUT, "Blockout", &blockout[..]),
            group(icons::LIGHT, "Light", &lights[..]),
            leaf(8),
        ];
        // The steps' menus: the current value marked.
        let current = [
            self.snapping.position,
            self.snapping.angle,
            self.snapping.scale,
        ];
        let step_items: [[MenuItem<'_>; 8]; 3] = core::array::from_fn(|k| {
            core::array::from_fn(|i| MenuItem {
                // A blank glyph keeps the check's column, so the values line up.
                icon: if STEP_VALUES[k].get(i) == Some(&current[k]) {
                    icons::CHECK
                } else {
                    " "
                },
                label: STEP_LABELS[k].get(i).copied().unwrap_or(""),
            })
        });
        let step_items: [&[MenuItem<'_>]; 3] =
            core::array::from_fn(|k| &step_items[k][..STEP_VALUES[k].len()]);
        self.status.clear();
        match &self.play {
            Some(p) if p.ejected.is_some() => self.status.push_str(
                "Ejected: the editor's camera on the running game; Shift+F8 returns, F8 stops  ·  ",
            ),
            Some(p) if p.paused => self
                .status
                .push_str("Paused: F7 goes on, Shift+F7 steps a tick, F8 stops  ·  "),
            Some(_) if self.in_view_window => self.status.push_str(
                "Playing in its window: F7 pauses, F8 stops (there too), Shift+F8 ejects here  ·  ",
            ),
            Some(_) => self.status.push_str(
                "Playing: keys without Ctrl go to the game; F7 pauses, Shift+F8 ejects, F8 stops  ·  ",
            ),
            None => {}
        }
        if self.key_transform.is_some() {
            let _ = write!(self.status, "{}  ·  ", transform::HELP);
        }
        if self.draw.is_some() {
            let _ = write!(self.status, "{}  ·  ", draw::HELP);
        }
        if self.face_edit.is_some() {
            let _ = write!(self.status, "{FACES_HELP}  ·  ");
        }
        if self.flying() {
            let _ = write!(
                self.status,
                "Flying at {:.1} m/s: W A S D, Q down, E up, Shift faster, the wheel for \
                 speed  ·  ",
                self.fly_speed
            );
        }
        if let Some(change) = self.gizmo_drag.as_ref().and_then(|d| d.last) {
            let _ = match change {
                Change::Move(d) => write!(self.status, "Move ({}, {}, {})", d.x, d.y, d.z),
                Change::Turn(q) => {
                    write!(
                        self.status,
                        "Rotate {:.1}°",
                        q.to_axis_angle().1.to_degrees()
                    )
                }
                Change::Scale(f) => write!(self.status, "Scale ×({}, {}, {})", f.x, f.y, f.z),
            };
            self.status.push_str("  ·  ");
        }
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
        // The tree's row a dragged file would drop onto.
        let drop_row = match self.asset_drag.as_ref().map(|d| d.over) {
            Some(DropTarget::Row(e)) => self.rows.iter().position(|r| r.entity == e),
            _ => None,
        };
        // The asset browser's rows.
        let folder_items: Vec<TreeItem<'_>> = self
            .browser
            .folders
            .iter()
            .map(|(path, depth)| TreeItem {
                depth: *depth,
                icon: if *path == self.browser.folder {
                    icons::FOLDER_OPEN
                } else {
                    icons::FOLDER
                },
                name: if path.is_empty() {
                    "res://"
                } else {
                    path.rsplit('/').next().unwrap_or(path)
                },
                kind: "",
                expanded: None,
                selected: *path == self.browser.folder,
                active: false,
                renaming: false,
                disabled: false,
                off: false,
            })
            .collect();
        // The tiles, the thumbnails of those on screen (by last frame's scroll area).
        let columns = self
            .listing
            .map_or(4, |(r, _)| crate::shell::tile_columns(r.size[0], self.tile));
        let shown = self.listing.map_or(0..columns * 2, |(r, scroll)| {
            let row = crate::shell::tile_row_height(self.tile);
            let first = (scroll / row).floor().max(0.0) as usize;
            let last = ((scroll + r.size[1]) / row).ceil() as usize;
            first * columns..(last + 1) * columns
        });
        let mut file_items: Vec<FileItem<'_>> = Vec::with_capacity(self.browser.listing.len());
        for (i, l) in self.browser.listing.iter().enumerate() {
            let thumbnail = match l.asset.and_then(|id| self.asset_db.get(id)) {
                Some(e) if shown.contains(&i) => {
                    let key = thumbnails::Key {
                        bytes: e.size,
                        modified: e.modified,
                    };
                    self.thumbnails
                        .image(Subject::File(e.id), &e.path, e.kind, key)
                }
                _ => None,
            };
            file_items.push(FileItem {
                name: &l.name,
                kind: l.kind,
                selected: l.asset.is_some() && l.asset == self.browser.selected,
                thumbnail,
            });
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
                } else if cp.finds_assets() {
                    let found = cp.assets[i];
                    let entry = self.asset_db.get(found.id);
                    PaletteRow {
                        icon: entry.map_or(icons::FILE, |e| file_icon_of(e.kind)),
                        label: entry.map_or("", |e| e.path.as_str()),
                        matched: &cp.highlights[found.matched.0 as usize..found.matched.1 as usize],
                        category: 0,
                        detail: entry.map_or("", |e| e.kind.name()),
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
                            .map_or("", |i| ADD[i].2),
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
            hint: cp.choices.as_ref().map_or(
                "Type a command, @ and an object's name, or # and an asset's (model:, texture:…)",
                |c| c.hint,
            ),
            rows: &palette_rows,
            selected: cp.selected.saturating_sub(cp.first),
        });
        let scenes = [self.scene_label.as_str()];
        let tools = self.tools.each_ref().map(String::as_str);
        // A keyboard transform's hint below and right of the pointer, in the window.
        let hint_at = {
            let s = self.scale.max(0.5);
            let [w, h] = [self.size[0] as f32 / s, self.size[1] as f32 / s];
            let [x, y] = self
                .input
                .cursor()
                .map_or([w * 0.5, h * 0.5], |[x, y]| [x / s, y / s]);
            [
                (x + 18.0).min(w - 300.0).max(4.0),
                (y + 20.0).min(h - 40.0).max(4.0),
            ]
        };
        let model = ShellModel {
            scenes: &scenes,
            active_scene: 0,
            tools: &tools,
            active_tool: self.active_tool,
            tree: &items,
            selection: &self.selection_label,
            inspector: &sections,
            path: &self.browser.label,
            folders: &folder_items,
            files: &file_items,
            tile: self.tile,
            columns,
            bottom_tabs: &[
                "Output",
                "Debugger",
                "Audio",
                "Animation",
                "Shader",
                "Profiler",
            ],
            active_bottom: None,
            playing,
            paused: self.play.as_ref().is_some_and(|p| p.paused),
            status: &self.status,
            snap: self.snapping.on,
            steps: self.step_labels.each_ref().map(String::as_str),
            step_menu: self.step_menu.map(|k| (k, step_items[k])),
            pivot: self.pivot.label(),
            add_menu: self.add_open.then_some((&add_items[..], self.add_group)),
            reveal_row,
            drop_row,
            tree_filter: &self.tree_filter,
            inspector_filter: &self.inspector_filter,
            palette,
            hint: (self.key_transform.is_some()
                || self.draw.is_some()
                || self.asset_drag.is_some())
            .then_some((self.hint.as_str(), hint_at)),
        };
        let scale = self.scale.max(0.5);
        let logical = [self.size[0] as f32 / scale, self.size[1] as f32 / scale];
        let over_listing = self.listing.is_some_and(|(r, _)| {
            self.input
                .cursor()
                .is_some_and(|[x, y]| r.contains([x / scale, y / scale]))
        });
        if over_listing && self.input.ctrl() {
            let notches = self.input.take_wheel();
            if notches != 0.0 {
                let (lo, hi) = TILE_RANGE;
                self.tile = (self.tile * TILE_NOTCH.powf(notches)).clamp(lo, hi).round();
            }
        }
        self.ui.begin_frame(self.input.ui_input(scale), logical);
        let response = build(&mut self.ui, &model, &self.palette);
        self.ui.end_frame(&mut self.text);
        self.listing = self
            .ui
            .rect_of(response.listing)
            .map(|r| (r, self.ui.scroll_offset(response.listing)));

        if let Some(i) = response.tool {
            self.active_tool = i;
        }
        // The browser: a folder opened, a file picked (a folder in the listing opened).
        if let Some(i) = response.folder
            && let Some((path, _)) = self.browser.folders.get(i).cloned()
        {
            self.open_folder(&path);
        }
        if let Some(i) = response.file
            && let Some(l) = self.browser.listing.get(i)
        {
            match l.asset {
                Some(id) => self.reveal_asset(id),
                None => {
                    let path = l.path.clone();
                    self.open_folder(&path);
                }
            }
        }
        if response.play {
            self.run(world, self.ids.play);
        }
        if response.pause {
            self.run(world, self.ids.pause);
        }
        if response.stop {
            self.run(world, self.ids.stop);
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
            self.add_group = None;
            self.open_choice = None;
            self.command_palette = None;
            self.step_menu = None;
        }
        if let Some(k) = response.step {
            self.step_menu = (self.step_menu != Some(k)).then_some(k);
        }
        if let Some(i) = response.step_item
            && let Some(k) = self.step_menu.take()
            && let Some(&value) = STEP_VALUES[k].get(i)
        {
            self.set_step(world, k, value);
        }
        if response.pivot {
            self.run(world, self.ids.pivot);
        }
        if response.add {
            self.add_open = !self.add_open;
            self.add_group = None;
        }
        if response.add_group.is_some() {
            self.add_group = response.add_group;
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
        if let Some(row) = response.tree_eye.and_then(|i| self.rows.get(i).copied()) {
            let on = world.get::<Disabled>(row.entity).is_none();
            self.set_disabled(world, &[row.entity], on);
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
        self.asset_drag_frame(world, &response, game_view);
        // Escape during a drag puts the value back as it was.
        if self.ui.input().keys.contains(&UiKey::Escape)
            && let Some(drag) = self.drag.as_mut()
            && !drag.cancelled
        {
            drag.cancelled = true;
            let merge = drag.merge;
            self.history.revert(world, merge);
        }
        // The shortcuts; while a field is typed in, only saving (the keys are the field's),
        // and none while the right button flies the camera (the letters are the flight's).
        let typing_text = self.ui.editing().is_some();
        let contexts: &[KeyContext] = if self.tree_focus { &[TREE_KEYS] } else { &[] };
        let chords = if self.flying() {
            0
        } else {
            self.input.chords().len()
        };
        for i in 0..chords {
            let chord = self.input.chords()[i];
            // In the game's view, the keys without Ctrl or Alt are the game's, but F1 to
            // F12.
            if game_view && !chord.ctrl && !chord.alt && !is_function_key(chord.key) {
                continue;
            }
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
        // The game's view: the viewport without the editor's tools.
        if game_view {
            if let Some([x, y, w, h]) = rect {
                self.view_rect = Some([
                    (x * scale).round() as u32,
                    (y * scale).round() as u32,
                    ((w * scale).round() as u32).max(1),
                    ((h * scale).round() as u32).max(1),
                ]);
            }
            self.input.next_frame();
            return;
        }
        if let Some([x, y, w, h]) = rect {
            let over = self.input.cursor().is_some_and(|[cx, cy]| {
                let (cx, cy) = (cx / scale, cy / scale);
                cx >= x && cy >= y && cx < x + w && cy < y + h
            });
            if (self.input.pressed(Button::Right) || self.input.pressed(Button::Middle)) && over {
                self.camera_drag = true;
            }
            self.camera_frame(world, w / h.max(1.0));
            if !self.input.held(Button::Right) && !self.input.held(Button::Middle) {
                self.camera_drag = false;
            }
            if self.camera_drag {
                // The hand takes the camera: no move toward a framing; panning, looking
                // around and flying move the target, so they end following too.
                self.camera_move = None;
                let [dx, dy] = self.input.drag();
                if self.input.held(Button::Middle) {
                    if self.input.shift() {
                        self.follow = None;
                        self.orbit.pan(dx, dy, h * scale);
                    } else {
                        self.orbit.orbit(dx, dy);
                    }
                } else {
                    self.follow = None;
                    self.orbit.look(dx, dy);
                    self.fly(dt);
                }
            }
            // The wheel zooms, or sets the speed while flying.
            let notches = self.input.wheel_notches();
            if self.flying() {
                if notches != 0.0 {
                    let (low, high) = FLY_RANGE;
                    self.fly_speed = (self.fly_speed * FLY_NOTCH.powf(notches)).clamp(low, high);
                    self.say(format!("Flight speed {:.1} m/s", self.fly_speed));
                }
            } else if over {
                if notches != 0.0 {
                    self.camera_move = None;
                }
                self.orbit.zoom(notches);
            }
            self.view_rect = Some([
                (x * scale).round() as u32,
                (y * scale).round() as u32,
                ((w * scale).round() as u32).max(1),
                ((h * scale).round() as u32).max(1),
            ]);
            self.pick_frame(world, over, Vec2::new(x * scale, y * scale));
        }
        self.gizmo_frame(world, scale);
        self.overlay_frame(world);
        self.drop_frame(world);
        self.input.next_frame();
        let eye = self.orbit.eye();
        if let Some(camera) = camera
            && let Some(t) = world.get_mut::<Transform>(camera)
        {
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
            FieldAction::Pick => {
                // The palette's assets of the field's kind; Enter gives the field the one
                // picked.
                let query = match self.path_of(&p, component).as_str() {
                    "Model.mesh" => "#model:",
                    "Model.material" => "#material:",
                    _ => "#",
                };
                self.open_palette(world, query);
                if let Some(cp) = self.command_palette.as_mut() {
                    cp.for_field = Some((event.section, event.field));
                }
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
        cp.for_field = None;
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
        cp.assets.clear();
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
        } else if let Some(text) = query.strip_prefix('#') {
            cp.commands.clear();
            // `model:rock`: a kind first narrows it (`mesh:` and `audio:` too).
            let (kind, name) = match text.split_once(':') {
                Some((k, rest)) => match asset_kind_named(k.trim()) {
                    Some(kind) => (Some(kind), rest.trim_start()),
                    None => (None, text),
                },
                None => (None, text),
            };
            cp.fuzzy.set_query(name);
            for e in self.asset_db.entries() {
                if kind.is_some_and(|k| k != e.kind) {
                    continue;
                }
                if let Some(score) = cp.fuzzy.score(&e.path) {
                    let start = cp.highlights.len() as u32;
                    cp.highlights.extend_from_slice(cp.fuzzy.positions());
                    cp.assets.push(FoundAsset {
                        id: e.id,
                        score,
                        matched: (start, cp.highlights.len() as u32),
                    });
                }
            }
            let db = &self.asset_db;
            cp.assets.sort_unstable_by(|a, b| {
                b.score.cmp(&a.score).then_with(|| {
                    let path = |f: &FoundAsset| db.get(f.id).map_or("", |e| e.path.as_str());
                    path(a).cmp(path(b))
                })
            });
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
                            Pick::Keys(p) => self.set_key_preset(world, p),
                            Pick::Select(e) => self.show(world, e),
                            Pick::Step(k, value) => self.set_step(world, k, value),
                            Pick::Keep(item) => self.toggle_keep(world, item),
                        }
                    }
                } else if cp.finds_objects() {
                    if let Some(found) = cp.objects.get(i) {
                        self.show(world, found.entity);
                    }
                } else if cp.finds_assets() {
                    match (cp.assets.get(i), cp.for_field) {
                        (Some(found), Some((section, field))) => {
                            self.give_field_asset(world, section, field, found.id);
                        }
                        (Some(found), None) => self.reveal_asset(found.id),
                        (None, _) => {}
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

    /// Whether an ancestor of `e` is selected too: it moves with that one.
    fn has_selected_ancestor(&self, world: &World, e: Entity) -> bool {
        let mut at = world.parent(e);
        while let Some(p) = at {
            if self.selection.contains(&p) {
                return true;
            }
            at = world.parent(p);
        }
        false
    }

    /// The viewport's picking (ADR 0109): a press there, not on a handle, may become a
    /// click, and a release near it clicks once the pick of its pixel answers; a plain
    /// click where the last one left the active entity cycles to the object behind it, and
    /// Alt+click offers everything under the pointer. The pointer's pixel is picked again
    /// whenever it, the camera or the scene changed. `origin` is the view's top left
    /// corner (physical pixels).
    fn pick_frame(&mut self, world: &World, over: bool, origin: Vec2) {
        let free = !self.camera_drag
            && self.command_palette.is_none()
            && !self.add_open
            && self.open_choice.is_none()
            && self.renaming.is_none()
            && self.gizmo_drag.is_none()
            && self.side_drag.is_none()
            && self.face_edit.as_ref().is_none_or(|f| !f.dragging())
            && self.key_transform.is_none()
            && self.draw.is_none()
            && self.drag.is_none()
            && self
                .asset_drag
                .as_ref()
                .is_none_or(|d| d.kind != engine_scene::AssetKind::Model);
        let cursor = self.input.cursor().map(|[x, y]| Vec2::new(x, y));
        // The pointer's pixel at the view's render size.
        let request = cursor.filter(|_| over && free).map(|at| {
            let local = (at - origin).max(Vec2::ZERO);
            PickRequest {
                x: (local.x * self.view_scale[0]) as u32,
                y: (local.y * self.view_scale[1]) as u32,
                transparent: self.pick_transparent,
            }
        });
        if self.input.pressed(Button::Left) {
            let on_face = self.face_edit.as_ref().is_some_and(|f| f.hovered.is_some());
            self.press_at = cursor.filter(|_| {
                request.is_some()
                    && self.hovered_handle.is_none()
                    && self.hovered_side.is_none()
                    && !on_face
            });
        }
        let near = 4.0 * self.scale.max(0.5);
        // An icon (a light, a camera) is above the scene: it takes the pointer first.
        self.icon_hover = cursor
            .filter(|_| request.is_some())
            .and_then(|at| self.icon_at(at));
        if self.input.released(Button::Left)
            && let Some(e) = self.icon_hover
            && let (Some(from), Some(at)) = (self.press_at, cursor)
            && from.distance(at) <= near
        {
            let toggle = self.input.shift() || self.input.ctrl();
            // A plain click again on the icon it selected goes through to what lies under
            // it (a sun added where a cube is), as cycling.
            let again = self
                .last_view_click
                .is_some_and(|(spot, x)| x == e && spot.distance(at) <= near)
                && self.active() == Some(e);
            if toggle || !again {
                self.press_at = None;
                self.click_viewport(world, Some(e), toggle);
                self.last_view_click = (!toggle).then_some((at, e));
            }
        }
        if self.input.released(Button::Left)
            && let (Some(from), Some(at), Some(request)) = (self.press_at.take(), cursor, request)
            && from.distance(at) <= near
        {
            let toggle = self.input.shift() || self.input.ctrl();
            let list = ListRequest::around(request.x, request.y, PICK_RADIUS, request.transparent);
            let again = self
                .last_view_click
                .is_some_and(|(spot, e)| spot.distance(at) <= near && self.active() == Some(e));
            // Answered by a pick made from the next frame on: the scene as it is now.
            let from = self.frame_seen + 1;
            if self.input.alt() {
                self.pending_list = Some((list, ListUse::Offer, from));
            } else if again && !toggle {
                self.pending_list = Some((list, ListUse::Cycle, from));
            } else {
                self.pending_click = Some((request, toggle, from));
            }
            self.click_at = Some(at);
        }
        self.list_request = self.pending_list.map(|(r, _, _)| r);
        self.box_frame(origin, near, cursor);
        let Some(request) = request else {
            self.hovered = None;
            self.last_pick = None;
            self.pick_request = self.pending_click.map(|(r, _, _)| r);
            return;
        };
        let (eye, target) = (self.orbit.eye(), self.orbit.target);
        let seen = (
            request,
            [eye.x, eye.y, eye.z, target.x, target.y, target.z],
            self.history.head(),
        );
        self.pick_request = if let Some((r, _, _)) = self.pending_click {
            Some(r)
        } else if self.last_pick != Some(seen) {
            self.last_pick = Some(seen);
            Some(request)
        } else {
            None
        };
    }

    /// A box in the viewport (ADR 0109): a press there, not on a handle, dragged past
    /// `near` draws one (Escape takes it back). While it is dragged, what it would take is
    /// asked for whenever it changed, one box at a time; at the release its last box is
    /// asked again and changes the selection once answered: alone it replaces it, with
    /// Shift it adds, with Ctrl it takes out. It takes what shows in it, with Alt hidden
    /// objects too. `origin` is the view's top left corner (physical pixels).
    fn box_frame(&mut self, origin: Vec2, near: f32, cursor: Option<Vec2>) {
        if self.box_drag.is_none()
            && self.input.held(Button::Left)
            && let (Some(from), Some(at)) = (self.press_at, cursor)
            && from.distance(at) > near
        {
            // No click at the release.
            self.press_at = None;
            self.box_drag = Some(BoxDrag {
                from,
                to: at,
                asked: None,
                waiting: None,
                taken: Vec::new(),
                released: None,
            });
        }
        let escape = self.ui.input().keys.contains(&UiKey::Escape);
        let released = self.input.released(Button::Left);
        let mode = if self.input.ctrl() {
            BoxMode::Remove
        } else if self.input.shift() {
            BoxMode::Add
        } else {
            BoxMode::Replace
        };
        let (scale, frame) = (self.view_scale, self.frame_seen);
        let (visible, transparent) = (!self.input.alt(), self.pick_transparent);
        let Some(b) = &mut self.box_drag else {
            return;
        };
        if b.released.is_some() {
            return;
        }
        if escape {
            self.box_drag = None;
            return;
        }
        if let Some(at) = cursor {
            b.to = at;
        }
        // The box at the view's render size, the pointer's pixel in it.
        let lo = (b.from.min(b.to) - origin).max(Vec2::ZERO);
        let hi = (b.from.max(b.to) - origin).max(Vec2::ZERO);
        let request = ListRequest {
            rect: [
                (lo.x * scale[0]) as u32,
                (lo.y * scale[1]) as u32,
                (hi.x * scale[0]) as u32 + 1,
                (hi.y * scale[1]) as u32 + 1,
            ],
            visible,
            transparent,
        };
        if released {
            // Its last box, asked again: answered by a list drawn from the next frame on.
            b.asked = Some(request);
            b.waiting = Some(frame);
            b.released = Some((mode, frame + 1));
            self.list_request = Some(request);
            return;
        }
        let late = b.waiting.is_some_and(|asked| frame > asked + BOX_PATIENCE);
        if (b.waiting.is_none() && b.asked != Some(request)) || late {
            b.asked = Some(request);
            b.waiting = Some(frame);
            self.list_request = Some(request);
        }
    }

    /// The selection after a box: `taken` (front to back) becomes it, is added to it, or is
    /// taken out of it; the nearest one taken ends active, and the tree opens to the active
    /// one.
    fn apply_box(&mut self, world: &World, taken: &[Entity], mode: BoxMode) {
        let before = self.active();
        match mode {
            BoxMode::Replace => self.selection = taken.iter().rev().copied().collect(),
            BoxMode::Add => {
                for &e in taken.iter().rev() {
                    if !self.selection.contains(&e) {
                        self.selection.push(e);
                    }
                }
            }
            BoxMode::Remove => self.selection.retain(|e| !taken.contains(e)),
        }
        self.anchor = self.active();
        if let Some(active) = self.active() {
            let mut at = world.parent(active);
            while let Some(p) = at {
                self.collapsed.remove(&p);
                at = world.parent(p);
            }
        }
        self.selection_changed(before);
    }

    /// The scene's entities whose slots `slots` holds (`slot_of` gives an entity's), in its
    /// order.
    fn entities_in(
        &mut self,
        world: &World,
        slots: &[u32],
        slot_of: impl Fn(Entity) -> Option<u32>,
    ) -> Vec<Entity> {
        let mut order: Vec<(u32, usize)> = slots.iter().enumerate().map(|(k, &s)| (s, k)).collect();
        order.sort_unstable();
        let mut found: Vec<(usize, Entity)> = Vec::new();
        if let Some(q) = self.scene_ids.as_mut() {
            for (e, _) in q.iter_readonly(world) {
                if let Some(slot) = slot_of(e)
                    && let Ok(i) = order.binary_search_by_key(&slot, |&(s, _)| s)
                {
                    found.push((order[i].1, e));
                }
            }
        }
        found.sort_unstable_by_key(|&(k, _)| k);
        found.into_iter().map(|(_, e)| e).collect()
    }

    /// The camera's part of the frame (ADR 0107 E2-4): a framing the overlay answered
    /// starts a move toward it; a move goes on (eased, `CAMERA_MOVE` long); a followed
    /// entity's motion carries the target with it. `aspect` is the viewport's.
    fn camera_frame(&mut self, world: &World, aspect: f32) {
        if let Some((lo, hi)) = self.framed.take() {
            let center = (lo + hi) * 0.5;
            let radius = ((hi - lo).length() * 0.5).max(FRAME_RADIUS);
            let to = self.orbit.framing(center, radius, aspect);
            self.camera_move = Some((self.orbit, to, Instant::now()));
        }
        if let Some((from, to, since)) = self.camera_move {
            let t = (since.elapsed().as_secs_f32() / CAMERA_MOVE).min(1.0);
            // Fast at first, settling at the end.
            self.orbit = from.toward(&to, 1.0 - (1.0 - t).powi(3));
            if t >= 1.0 {
                self.camera_move = None;
            }
        }
        if let Some((e, last)) = self.follow {
            match world.get::<GlobalTransform>(e) {
                Some(g) => {
                    let at = Vec3::from(g.0.translation);
                    self.orbit.target += at - last;
                    if let Some((_, to, _)) = &mut self.camera_move {
                        to.target += at - last;
                    }
                    self.follow = Some((e, at));
                }
                None => self.follow = None,
            }
        }
    }

    /// F: frames the selection with its children (their world boxes, which the overlay
    /// unites, and their places), or the whole scene when nothing is selected.
    fn frame_selection(&mut self, world: &World) {
        let mut points: Option<(Vec3, Vec3)> = None;
        let mut stack: Vec<Entity> = self.selection.clone();
        while let Some(e) = stack.pop() {
            if let Some(g) = world.get::<GlobalTransform>(e) {
                let at = Vec3::from(g.0.translation);
                points = Some(points.map_or((at, at), |(lo, hi)| (lo.min(at), hi.max(at))));
            }
            stack.extend(world.children(e));
        }
        self.frame_asked = Some(points);
        self.say(String::from(if self.selection.is_empty() {
            "Framing the scene"
        } else {
            "Framing the selection"
        }));
    }

    /// Shift+F: the camera follows the active entity as it moves, until the camera is
    /// panned or flown; again, it stops.
    fn toggle_follow(&mut self, world: &World) {
        if self.follow.take().is_some() {
            self.say(String::from("Not following"));
            return;
        }
        let Some(e) = self.active() else {
            self.say(String::from("Nothing selected to follow"));
            return;
        };
        let Some(g) = world.get::<GlobalTransform>(e) else {
            return;
        };
        self.follow = Some((e, Vec3::from(g.0.translation)));
        let name = world.get::<Name>(e).map_or("the selection", Name::as_str);
        self.say(format!("Following {name} (Shift+F again: stop)"));
    }

    /// End: puts each selected entity (those no selected entity carries) on the surface
    /// below it, one after the other, as one undo step: its box's bottom on the first
    /// surface under the bottom's middle, the selection left out.
    fn drop_selection(&mut self, world: &World) {
        let queue = self.selection_roots(world);
        if queue.is_empty() {
            self.say(String::from("Nothing selected to drop"));
            return;
        }
        self.gestures += 1;
        self.dropping = Some(Dropping {
            queue: queue.into_iter().rev().collect(),
            merge: self.gestures,
            current: None,
        });
    }

    /// End's frame: the next entity to drop, and the one whose surface was found moved
    /// onto it.
    fn drop_frame(&mut self, world: &mut World) {
        let Some(mut d) = self.dropping.take() else {
            return;
        };
        if let Some(one) = &d.current
            && let Some(found) = one.found
        {
            let e = one.entity;
            match (found, one.asked) {
                (Some(hit), Some((_, bottom))) => {
                    if let Some(target) = self
                        .transform_targets(world)
                        .into_iter()
                        .find(|t| t.entity == e)
                    {
                        let change = Change::Move(Vec3::Y * (hit.point.y - bottom));
                        self.set_targets(
                            world,
                            "Drop to the Surface Below",
                            d.merge,
                            &[target],
                            change,
                            (Vec3::ZERO, [Vec3::X, Vec3::Y, Vec3::Z]),
                        );
                    }
                }
                _ => {
                    let name = world.get::<Name>(e).map_or("it", Name::as_str);
                    self.say(format!("Nothing below {name}"));
                }
            }
            d.current = None;
        }
        if d.current.is_none() {
            let Some(e) = d.queue.pop() else {
                return;
            };
            let place = world
                .get::<GlobalTransform>(e)
                .map_or(Vec3::ZERO, |g| Vec3::from(g.0.translation));
            d.current = Some(DropOne {
                entity: e,
                slots: None,
                place,
                asked: None,
                found: None,
            });
        }
        self.dropping = Some(d);
    }

    /// The viewport's overlays' part of the frame (ADR 0107 E2-7): the icons of the scene's
    /// lights and cameras on the screen (not the editor's own camera), and the reach of the
    /// selected ones: a point light's sphere as three circles, a spot light's cone, a
    /// camera's pyramid, the sun's direction.
    fn overlay_frame(&mut self, world: &World) {
        self.view_icons.clear();
        self.reach_lines.clear();
        let Some(rect) = self.view_rect.filter(|_| self.overlays) else {
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        if let Some(q) = self.scene_ids.as_mut() {
            for (e, _) in q.iter_readonly(world) {
                if Some(e) == self.camera {
                    continue;
                }
                let Some(glyph) = icon_of(world, e) else {
                    continue;
                };
                let Some(g) = world.get::<GlobalTransform>(e) else {
                    continue;
                };
                if let Some(at) = view.screen(Vec3::from(g.0.translation))
                    && view.contains(at)
                    && (Vec3::from(g.0.translation) - view.eye).dot(view.forward) > 0.0
                {
                    self.view_icons.push(ViewIcon {
                        entity: e,
                        glyph,
                        at,
                    });
                }
            }
        }
        for &e in &self.selection {
            let Some(g) = world.get::<GlobalTransform>(e) else {
                continue;
            };
            let at = Vec3::from(g.0.translation);
            let (right, up, back) = (
                Vec3::from(g.0.matrix3.x_axis).normalize_or_zero(),
                Vec3::from(g.0.matrix3.y_axis).normalize_or_zero(),
                Vec3::from(g.0.matrix3.z_axis).normalize_or_zero(),
            );
            let mut line = |a: Vec3, b: Vec3, camera: bool| {
                if let Some((a, b)) = segment_on_screen(&view, a, b) {
                    self.reach_lines.push((a, b, camera));
                }
            };
            // A circle of `radius` about `center` across `u` and `v`.
            let circle = |center: Vec3,
                          u: Vec3,
                          v: Vec3,
                          radius: f32,
                          line: &mut dyn FnMut(Vec3, Vec3, bool)| {
                const SEGMENTS: usize = 48;
                let point = |k: usize| {
                    let t = k as f32 / SEGMENTS as f32 * core::f32::consts::TAU;
                    center + (u * t.cos() + v * t.sin()) * radius
                };
                for k in 0..SEGMENTS {
                    line(point(k), point(k + 1), false);
                }
            };
            if let Some(light) = world.get::<engine_runtime::PointLight>(e) {
                for (u, v) in [(Vec3::X, Vec3::Y), (Vec3::Y, Vec3::Z), (Vec3::Z, Vec3::X)] {
                    circle(at, u, v, light.range, &mut line);
                }
            } else if let Some(light) = world.get::<engine_runtime::SpotLight>(e) {
                // Down its −Z: the cone's base where the range ends.
                let (sin, cos) = light.outer.sin_cos();
                let base = at - back * (light.range * cos);
                let radius = light.range * sin;
                circle(base, right, up, radius, &mut line);
                for dir in [right, -right, up, -up] {
                    line(at, base + dir * radius, false);
                }
            } else if let Some(camera) = world.get::<engine_runtime::Camera>(e) {
                let aspect = rect[2] as f32 / rect[3].max(1) as f32;
                let corners = |depth: f32| {
                    let h = depth * (camera.fov_y * 0.5).tan();
                    let w = h * aspect;
                    let center = at - back * depth;
                    [
                        center - right * w - up * h,
                        center + right * w - up * h,
                        center + right * w + up * h,
                        center - right * w + up * h,
                    ]
                };
                let (near, far) = (corners(camera.near), corners(CAMERA_REACH.min(camera.far)));
                for k in 0..4 {
                    line(near[k], near[(k + 1) % 4], true);
                    line(far[k], far[(k + 1) % 4], true);
                    line(near[k], far[k], true);
                }
            } else if world.get::<engine_runtime::Sun>(e).is_some() {
                line(at, at - back * SUN_REACH, false);
            }
        }
    }

    /// The icon under `at` (physical pixels), the nearest within reach.
    fn icon_at(&self, at: Vec2) -> Option<Entity> {
        let reach = ICON_REACH * self.scale.max(0.5);
        self.view_icons
            .iter()
            .map(|i| (i.at.distance(at), i.entity))
            .filter(|&(d, _)| d <= reach)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, e)| e)
    }

    /// The tool bar's steps as they read, after one changed.
    fn refresh_steps(&mut self) {
        let values = [
            self.snapping.position,
            self.snapping.angle,
            self.snapping.scale,
        ];
        for (k, value) in values.into_iter().enumerate() {
            self.step_labels[k] = step_label(k, value);
        }
    }

    /// Sets snapping step `kind` (0 position, 1 angle, 2 scale) to `value`; the session
    /// keeps it.
    fn set_step(&mut self, world: &World, kind: usize, value: f64) {
        match kind {
            0 => self.snapping.position = value,
            1 => self.snapping.angle = value,
            _ => self.snapping.scale = value,
        }
        self.refresh_steps();
        self.write_session(world);
        let what = ["Position", "Angle", "Scale"][kind.min(2)];
        self.say(format!("{what} step {}", step_label(kind, value)));
    }

    /// "Snapping: Position Step…" and the others: the step's values in the palette.
    fn pick_step(&mut self, world: &mut World, kind: usize) {
        let current = [
            self.snapping.position,
            self.snapping.angle,
            self.snapping.scale,
        ][kind];
        let items = STEP_VALUES[kind]
            .iter()
            .zip(STEP_LABELS[kind])
            .map(|(&value, &label)| Choice {
                label: String::from(label),
                detail: String::from(if value == current { "now" } else { "" }),
                pick: Pick::Step(kind, value),
            })
            .collect();
        let hint = ["The position step", "The angle step", "The scale step"][kind];
        self.open_list(world, hint, items);
    }

    /// 1 to 9: flies to bookmark `n`; Ctrl with the digit sets it to the camera as it is.
    fn bookmark(&mut self, world: &World, n: usize, set: bool) {
        if set {
            self.bookmarks[n] = Some(self.orbit);
            self.write_session(world);
            self.say(format!("Bookmark {} set (Ctrl+{0}; {0} goes there)", n + 1));
        } else if let Some(to) = self.bookmarks[n] {
            self.follow = None;
            self.camera_move = Some((self.orbit, to, Instant::now()));
        } else {
            self.say(format!("No bookmark {} yet: Ctrl+{0} sets it", n + 1));
        }
    }

    /// Names the entity of the newest pick's instance (`slot_of` gives an entity's), as
    /// the entity under the pointer, and answers a click waiting for that pixel.
    fn resolve_pick(&mut self, world: &World, slot_of: impl Fn(Entity) -> Option<u32>) {
        let Some((request, frame, instance)) = self.picked.take() else {
            return;
        };
        let entity = instance.and_then(|slot| {
            let q = self.scene_ids.as_mut()?;
            q.iter_readonly(world)
                .map(|(e, _)| e)
                .find(|&e| slot_of(e) == Some(slot))
        });
        self.hovered = Some((request, entity));
        if let Some((click, toggle, from)) = self.pending_click
            && click == request
            && frame >= from
        {
            self.pending_click = None;
            self.click_viewport(world, entity, toggle);
            // Only a plain click starts cycling.
            let at = self.click_at.take();
            self.last_view_click = if toggle { None } else { at.zip(self.active()) };
        }
    }

    /// Names the entities of the newest list of what lies under a pixel, front to back
    /// (`slot_of` gives an entity's slot), and answers a click waiting for it: a cycling
    /// click selects the next one behind the active entity (the nearest when the active
    /// one is not there, round to it after the farthest); Alt+click offers them all.
    fn resolve_list(&mut self, world: &World, slot_of: impl Fn(Entity) -> Option<u32>) {
        let Some((request, frame, slots)) = self.listed.take() else {
            return;
        };
        let answers = self
            .pending_list
            .is_some_and(|(wanted, _, from)| wanted == request && frame >= from);
        if self.box_drag.is_none() && !answers {
            return;
        }
        let mut under = self.entities_in(world, &slots, slot_of);
        // A box's: what it takes (the icons in it too), and the selection once its last
        // box is answered.
        if let Some(b) = &mut self.box_drag {
            let (lo, hi) = (b.from.min(b.to), b.from.max(b.to));
            for icon in &self.view_icons {
                if icon.at.cmpge(lo).all()
                    && icon.at.cmple(hi).all()
                    && !under.contains(&icon.entity)
                {
                    under.push(icon.entity);
                }
            }
            if b.asked == Some(request) {
                b.waiting = None;
            }
            b.taken = under;
            if let Some((mode, from)) = b.released
                && b.asked == Some(request)
                && frame >= from
            {
                let taken = core::mem::take(&mut b.taken);
                self.box_drag = None;
                self.apply_box(world, &taken, mode);
            }
            return;
        }
        let Some((_, used, _)) = self.pending_list.take() else {
            return;
        };
        let at = self.click_at.take();
        match used {
            ListUse::Cycle => {
                let next = match self
                    .active()
                    .and_then(|a| under.iter().position(|&e| e == a))
                {
                    Some(i) => under.get((i + 1) % under.len()).copied(),
                    None => under.first().copied(),
                };
                self.click_viewport(world, next, false);
                self.last_view_click = at.zip(self.active());
            }
            ListUse::Offer => {
                self.last_view_click = None;
                if under.is_empty() {
                    self.say(String::from("Nothing under the pointer"));
                } else {
                    self.under_pointer = Some(under);
                }
            }
        }
    }

    /// Alt+click's list: the entities under the pointer, nearest first, in the palette;
    /// picking one selects it.
    fn offer_under_pointer(&mut self, world: &mut World, under: Vec<Entity>) {
        let items = under
            .into_iter()
            .filter(|&e| world.contains(e))
            .map(|e| {
                let (_, kind) = kind_of(world, e);
                Choice {
                    label: String::from(world.get::<Name>(e).map_or(kind, Name::as_str)),
                    detail: String::from(kind),
                    pick: Pick::Select(e),
                }
            })
            .collect();
        self.open_list(world, "What lies under the pointer, nearest first", items);
    }

    /// A click in the viewport on `entity` (none: on nothing; ADR 0109). It selects it
    /// alone; with Shift or Ctrl (`toggle`) it adds it, or takes it out when it is
    /// selected. A click on nothing clears the selection, unless it toggles. The tree opens
    /// to show what was picked.
    fn click_viewport(&mut self, world: &World, entity: Option<Entity>, toggle: bool) {
        match (entity, toggle) {
            (Some(e), false) => self.show(world, e),
            (Some(e), true) => {
                self.toggle(e);
                let mut at = world.parent(e);
                while let Some(p) = at {
                    self.collapsed.remove(&p);
                    at = world.parent(p);
                }
            }
            (None, false) => {
                let before = self.active();
                self.selection.clear();
                self.anchor = None;
                self.selection_changed(before);
            }
            (None, true) => {}
        }
    }

    /// What to outline (ADR 0109; `slot_of` gives an entity's slot): each selected entity
    /// with its children, the active one's as the active class, then the hovered entity
    /// when it is not among them.
    fn outline(&mut self, world: &World, slot_of: impl Fn(Entity) -> Option<u32>) {
        self.outline_items.clear();
        let active = self.active();
        // While a box is dragged, what it takes in the hovered one's line; with Ctrl the
        // selected ones it would take out lose their own.
        let (boxed, removing): (&[Entity], bool) = match &self.box_drag {
            Some(b) => (&b.taken, self.input.ctrl()),
            None => (&[], false),
        };
        let mut stack = Vec::new();
        for &e in &self.selection {
            if removing && boxed.contains(&e) {
                continue;
            }
            let class = if Some(e) == active {
                OutlineClass::Active
            } else {
                OutlineClass::Selected
            };
            stack.push(e);
            while let Some(x) = stack.pop() {
                if let Some(slot) = slot_of(x) {
                    self.outline_items.push(OutlineItem { slot, class });
                }
                stack.extend(world.children(x));
            }
        }
        if self.box_drag.is_some() {
            for &e in boxed {
                if let Some(slot) = slot_of(e)
                    && self.outline_items.iter().all(|i| i.slot != slot)
                {
                    self.outline_items.push(OutlineItem {
                        slot,
                        class: OutlineClass::Hovered,
                    });
                }
            }
        } else if let Some((_, Some(hovered))) = self.hovered
            && let Some(slot) = slot_of(hovered)
            && self.outline_items.iter().all(|i| i.slot != slot)
        {
            self.outline_items.push(OutlineItem {
                slot,
                class: OutlineClass::Hovered,
            });
        }
    }

    /// The gizmo's part of the frame (ADR 0107 E2): a drag in progress goes on, is taken
    /// back by Escape or ends with the button; the gizmo is laid out at the selection's
    /// pivot (the middle of the selected entities that no selected entity carries); a left
    /// press on one of its handles starts a drag.
    fn gizmo_frame(&mut self, world: &mut World, scale: f32) {
        self.gizmo_shown = false;
        let Some(rect) = self.view_rect else {
            self.gizmo_drag = None;
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        let cursor = self.input.cursor().map(|[x, y]| Vec2::new(x, y));
        // A keyboard transform hides the gizmo and shows its guides.
        if let Some(t) = &self.key_transform {
            t.guides(&view, cursor, &mut self.guides);
            self.hovered_handle = None;
            self.handles.clear();
            self.hovered_side = None;
            return;
        }
        if self.draw.is_some() {
            self.hovered_handle = None;
            self.handles.clear();
            self.hovered_side = None;
            return;
        }
        // Editing faces: the faces have the pointer, the gizmo and the handles hide.
        if self.face_edit.is_some() {
            self.faces_frame(world, &view, cursor);
            self.hovered_handle = None;
            self.handles.clear();
            self.hovered_side = None;
            return;
        }
        // The active brush's side handles take the pointer before the gizmo's; the gizmo
        // hides while a side is dragged.
        if self.sides_frame(world, &view, cursor, scale) {
            self.hovered_handle = None;
            return;
        }
        if let Some(mut drag) = self.gizmo_drag.take()
            && self.input.held(Button::Left)
        {
            if self.ui.input().keys.contains(&UiKey::Escape) && !drag.cancelled {
                drag.cancelled = true;
                drag.last = None;
                self.history.revert(world, drag.merge);
            } else if !drag.cancelled
                && let Some(at) = cursor
                && drag.handle.moves()
                && self.input.ctrl()
                && self.input.shift()
            {
                // Ctrl+Shift: on the surface under the pointer, the selection left out; the
                // newest one found places it (a few frames behind the pointer).
                let local = (at - Vec2::new(rect[0] as f32, rect[1] as f32)).max(Vec2::ZERO);
                self.surface_request = Some(SurfaceRequest {
                    x: (local.x * self.view_scale[0]) as u32,
                    y: (local.y * self.view_scale[1]) as u32,
                    transparent: self.pick_transparent,
                });
                if let Some(hit) = self.surfaced.and_then(|r| r.hit) {
                    let change = Change::Move(drag.on_surface(hit.point, hit.normal) - drag.pivot);
                    if drag.last != Some(change) {
                        drag.last = Some(change);
                        self.apply_gizmo(world, &drag, change);
                    }
                }
            } else if !drag.cancelled
                && let Some(at) = cursor
                && let Some(change) = drag.change(
                    &view,
                    at,
                    &self.snapping,
                    self.input.shift(),
                    self.input.ctrl(),
                )
                && drag.last != Some(change)
            {
                drag.last = Some(change);
                self.apply_gizmo(world, &drag, change);
            }
            self.gizmo_drag = Some(drag);
        }
        let mode = Mode::of_tool(self.active_tool);
        let (pivot, axes) = if let Some(drag) = &self.gizmo_drag {
            // Where the drag has carried the selection.
            match drag.last {
                Some(Change::Move(d)) => (drag.pivot + d, drag.axes),
                _ => (drag.pivot, drag.axes),
            }
        } else {
            let Some(pivot) = self.selection_pivot(world) else {
                self.hovered_handle = None;
                return;
            };
            let axes = if self.local_axes {
                self.own_axes(world)
            } else {
                [Vec3::X, Vec3::Y, Vec3::Z]
            };
            (pivot, axes)
        };
        self.gizmo_shown = self.gizmo.layout(&view, mode, pivot, axes, scale);
        if self.gizmo_drag.is_some() {
            return;
        }
        let free = !self.camera_drag
            && self.command_palette.is_none()
            && !self.add_open
            && self.open_choice.is_none()
            && self.renaming.is_none();
        self.hovered_handle = match cursor {
            Some(at)
                if self.gizmo_shown && free && self.hovered_side.is_none() && view.contains(at) =>
            {
                self.gizmo.hit(at, scale)
            }
            _ => None,
        };
        if let (Some(handle), Some(at)) = (self.hovered_handle, cursor)
            && self.input.pressed(Button::Left)
        {
            let targets = self.transform_targets(world);
            self.gestures += 1;
            self.gizmo_drag =
                GizmoDrag::start(&self.gizmo, &view, handle, at, targets, self.gestures);
            self.surfaced = None;
            if let Some(drag) = &mut self.gizmo_drag {
                drag.extent = self
                    .selection_box
                    .map(|(lo, hi)| (lo - drag.pivot, hi - drag.pivot));
            }
        }
    }

    /// The selection's pivot: the middle of the selected entities that no selected entity
    /// carries; none without one.
    fn selection_pivot(&self, world: &World) -> Option<Vec3> {
        if self.pivot == Pivot::Active
            && let Some(g) = self.active().and_then(|e| world.get::<GlobalTransform>(e))
        {
            return Some(Vec3::from(g.0.translation));
        }
        let mut sum = Vec3::ZERO;
        let mut n = 0u32;
        for &e in &self.selection {
            if !self.has_selected_ancestor(world, e)
                && let Some(g) = world.get::<GlobalTransform>(e)
            {
                sum += Vec3::from(g.0.translation);
                n += 1;
            }
        }
        (n > 0).then(|| sum / n as f32)
    }

    /// The active entity's axes in the world, the world's without one: what "local" is to
    /// the gizmo and to keyboard transforms. Float dust is taken off (a quarter turn
    /// leaves 4e-8 across), so moves along them stay exact.
    fn own_axes(&self, world: &World) -> [Vec3; 3] {
        let Some(g) = self.active().and_then(|e| world.get::<GlobalTransform>(e)) else {
            return [Vec3::X, Vec3::Y, Vec3::Z];
        };
        let m = g.0.matrix3;
        [m.x_axis, m.y_axis, m.z_axis].map(|a| {
            let a = Vec3::from(a).normalize_or_zero();
            Vec3::select(a.abs().cmplt(Vec3::splat(1e-6)), Vec3::ZERO, a).normalize_or_zero()
        })
    }

    /// What a transform moves: the selected entities that no selected entity carries,
    /// each with its world transform, its parent's inverse and its own transform now.
    fn transform_targets(&self, world: &World) -> Vec<Target> {
        self.selection
            .iter()
            .copied()
            .filter(|&e| !self.has_selected_ancestor(world, e))
            .filter_map(|e| {
                let local = *world.get::<Transform>(e)?;
                let global = world.get::<GlobalTransform>(e)?.0;
                let parent_inverse = world
                    .parent(e)
                    .and_then(|p| world.get::<GlobalTransform>(p))
                    .map_or(Affine3A::IDENTITY, |g| g.0.inverse());
                Some(Target {
                    entity: e,
                    world: global,
                    parent_inverse,
                    local,
                })
            })
            .collect()
    }

    /// Tab: editing the selected brushes' faces, or the editing ended.
    fn toggle_face_edit(&mut self, world: &World) {
        if self.face_edit.take().is_some() {
            return;
        }
        let brushes: Vec<Entity> = self
            .selection
            .iter()
            .copied()
            .filter(|&e| world.get::<Brush>(e).is_some())
            .collect();
        if let Some(group) = brushes.iter().find_map(|&e| baked_group(world, e)) {
            self.say(baked_message(world, group));
            return;
        }
        if brushes.is_empty() {
            self.say(String::from("Select brushes to edit their faces (Tab)"));
            return;
        }
        self.face_edit = Some(FaceEdit {
            brushes,
            faces: Vec::new(),
            owners: Vec::new(),
            points: Vec::new(),
            point_owners: Vec::new(),
            hovered: None,
            drag: None,
            corner_drag: None,
        });
    }

    /// Editing faces' frame (Tab, ADR 0110 E2b-5): the brushes' faces laid out and the one
    /// under the pointer lit; a press on it starts a drag (with Alt a pull), which moves the
    /// face, or pulls a brush out of it, until the release, as one undo step. Escape takes a
    /// drag back, or ends the editing; a press on no face ends it, the click selecting.
    fn faces_frame(&mut self, world: &mut World, view: &ViewProj, cursor: Option<Vec2>) {
        let Some(mut edit) = self.face_edit.take() else {
            return;
        };
        let escape = self.ui.input().keys.contains(&UiKey::Escape);
        if let Some(mut dragging) = edit.drag.take() {
            if self.input.held(Button::Left) {
                if escape && !dragging.face.cancelled {
                    dragging.face.cancelled = true;
                    self.history.revert(world, dragging.face.merge);
                    dragging.pulled = None;
                } else if !dragging.face.cancelled
                    && let Some(at) = cursor
                    && let Some(d) = dragging.face.travel(
                        view,
                        at,
                        &self.snapping,
                        self.input.shift(),
                        self.input.ctrl(),
                    )
                {
                    if dragging.face.extrude {
                        self.set_pulled(world, &mut dragging, d);
                    } else {
                        self.set_moved(world, &mut dragging, d);
                    }
                }
                edit.drag = Some(dragging);
            } else if let Some(p) = dragging.pulled
                && !edit.brushes.contains(&p)
            {
                // A pulled brush's faces are edited next too.
                edit.brushes.push(p);
            }
        }
        if let Some((e, start, mut drag)) = edit.corner_drag.take()
            && self.input.held(Button::Left)
        {
            if escape && !drag.cancelled {
                drag.cancelled = true;
                self.history.revert(world, drag.merge);
            } else if !drag.cancelled
                && let Some(at) = cursor
                && let Some(planes) = drag.planes(
                    view,
                    at,
                    &self.snapping,
                    self.input.shift(),
                    self.input.ctrl(),
                )
            {
                self.history.begin("Move Corners", Some(drag.merge));
                self.history.touch(world, e);
                world.insert(e, faces::edited(&start, planes));
                self.history.commit(world);
            }
            edit.corner_drag = Some((e, start, drag));
        }
        edit.faces.clear();
        edit.owners.clear();
        edit.points.clear();
        edit.point_owners.clear();
        let mut scratch = Vec::new();
        let mut parts = Vec::new();
        for &e in &edit.brushes {
            if let (Some(b), Some(g)) = (world.get::<Brush>(e), world.get::<GlobalTransform>(e)) {
                faces::faces(b, g.0, &mut scratch);
                edit.owners.extend(scratch.iter().map(|_| e));
                edit.faces.append(&mut scratch);
                faces::corners(b, g.0, &mut parts);
                edit.point_owners.extend(parts.iter().map(|_| e));
                edit.points.append(&mut parts);
            }
        }
        if !edit.dragging() {
            let free = !self.camera_drag && self.command_palette.is_none() && !self.add_open;
            let reach = 8.0 * self.scale.max(0.5);
            edit.hovered = cursor
                .filter(|&at| free && view.contains(at))
                .and_then(|at| hover(&edit, view, at, reach));
            if escape {
                return;
            }
            if self.input.pressed(Button::Left)
                && let Some(Hover::Corner(p, _) | Hover::Edge(p, _)) = edit.hovered
            {
                let e = edit.point_owners[p];
                let pc = &edit.points[p];
                let (moving, grab) = match edit.hovered {
                    Some(Hover::Corner(_, k)) => (vec![k], pc.world[k]),
                    Some(Hover::Edge(_, k)) => {
                        let (a, b) = pc.edges[k];
                        (vec![a, b], (pc.world[a] + pc.world[b]) * 0.5)
                    }
                    _ => (Vec::new(), Vec3::ZERO),
                };
                if let (Some(b), Some(g)) = (world.get::<Brush>(e), world.get::<GlobalTransform>(e))
                {
                    self.gestures += 1;
                    let drag = CornerDrag::start(b, g.0, pc, moving, grab, view.eye, self.gestures);
                    edit.corner_drag = Some((e, b.clone(), drag));
                }
                self.face_edit = Some(edit);
                return;
            }
            if self.input.pressed(Button::Left) {
                let Some(Hover::Face(i)) = edit.hovered else {
                    // A press on nothing of the brushes: the editing ends, the click selects.
                    return;
                };
                let e = edit.owners[i];
                let ray = cursor.map(|at| view.ray(at));
                let grab = ray
                    .and_then(|(o, d)| {
                        faces::pick(&edit.faces[i..=i], (o, d)).map(|(_, t)| o + d * t)
                    })
                    .unwrap_or_else(|| edit.faces[i].middle());
                if let (Some(b), Some(g)) = (world.get::<Brush>(e), world.get::<GlobalTransform>(e))
                {
                    self.gestures += 1;
                    let start = b.clone();
                    if let Some(face) = FaceDrag::start(
                        b,
                        g.0,
                        &edit.faces[i],
                        grab,
                        self.input.alt(),
                        self.gestures,
                    ) {
                        edit.drag = Some(FaceDragging {
                            entity: e,
                            start,
                            face,
                            pulled: None,
                        });
                    }
                }
            }
        }
        self.face_edit = Some(edit);
    }

    /// A face moved `d` metres: its brush's planes as they then are, as one undo step per
    /// drag (a move that would leave a part empty is not taken).
    fn set_moved(&mut self, world: &mut World, dragging: &mut FaceDragging, d: f32) {
        let Some(planes) = dragging.face.moved(d) else {
            return;
        };
        self.history.begin("Move Face", Some(dragging.face.merge));
        self.history.touch(world, dragging.entity);
        world.insert(dragging.entity, faces::edited(&dragging.start, planes));
        self.history.commit(world);
    }

    /// A face pulled `d` metres: the brush it makes (out, an adding brush; in, a cut), made
    /// beside the dragged one the first time, changed after, gone again at no distance;
    /// one undo step per drag.
    fn set_pulled(&mut self, world: &mut World, dragging: &mut FaceDragging, d: f32) {
        let merge = dragging.face.merge;
        let Some((op, planes)) = dragging.face.pulled(d) else {
            if dragging.pulled.take().is_some() {
                self.history.revert(world, merge);
            }
            return;
        };
        let brush = Brush {
            op,
            hollow: 0.0,
            ..faces::edited(&dragging.start, planes)
        };
        let (step, kind) = if op == BrushOp::Cut {
            ("Push Face In", "Cut")
        } else {
            ("Pull Face Out", "Extrusion")
        };
        self.history.begin(step, Some(merge));
        let e = match dragging.pulled {
            Some(e) => {
                self.history.touch(world, e);
                world.insert(e, brush);
                e
            }
            None => {
                let parent = world.parent(dragging.entity);
                let place = world
                    .get::<Transform>(dragging.entity)
                    .copied()
                    .unwrap_or(Transform::IDENTITY);
                let name = self.sibling_name(world, parent, kind, None);
                let e = world.spawn((
                    SceneId::new(),
                    Name::new(&name),
                    place,
                    GlobalTransform::default(),
                    brush,
                ));
                let _ = world.set_parent(e, parent);
                self.history.touch_new(world, e);
                e
            }
        };
        self.history.commit(world);
        dragging.pulled = Some(e);
    }

    /// The active brush's side handles (ADR 0110 E2b-3c): laid out where they face the
    /// camera; a press on one starts a drag of its side, which follows the pointer until the
    /// release (Escape takes it back), one undo step. True while a side is dragged.
    fn sides_frame(
        &mut self,
        world: &mut World,
        view: &ViewProj,
        cursor: Option<Vec2>,
        scale: f32,
    ) -> bool {
        if let Some(mut drag) = self.side_drag.take() {
            if !self.input.held(Button::Left) {
                self.layout_sides(world, view);
                return false;
            }
            if self.ui.input().keys.contains(&UiKey::Escape) && !drag.cancelled {
                drag.cancelled = true;
                self.history.revert(world, drag.merge);
            } else if !drag.cancelled
                && let Some(at) = cursor
                && let Some((size, translation)) = drag.change(
                    view,
                    at,
                    &self.snapping,
                    self.input.shift(),
                    self.input.ctrl(),
                )
            {
                self.history.begin("Resize Brush", Some(drag.merge));
                self.history.touch(world, drag.entity);
                if let Some(b) = world.get_mut::<Brush>(drag.entity) {
                    b.size = size;
                }
                if let Some(t) = world.get_mut::<Transform>(drag.entity) {
                    t.translation = translation;
                }
                self.history.commit(world);
            }
            self.layout_sides(world, view);
            self.hovered_side = Some(drag.side);
            self.side_drag = Some(drag);
            return true;
        }
        self.layout_sides(world, view);
        let free = !self.camera_drag
            && self.command_palette.is_none()
            && !self.add_open
            && self.open_choice.is_none()
            && self.renaming.is_none()
            && self.gizmo_drag.is_none()
            && self.box_drag.is_none();
        self.hovered_side = match cursor {
            Some(at) if free && view.contains(at) => self.handles.hit(at, scale),
            _ => None,
        };
        if let (Some(side), Some(e)) = (self.hovered_side, self.handles.entity)
            && self.input.pressed(Button::Left)
            && let (Some(brush), Some(local), Some(placed)) = (
                world.get::<Brush>(e),
                world.get::<Transform>(e),
                world.get::<GlobalTransform>(e),
            )
        {
            self.gestures += 1;
            self.side_drag = Some(SideDrag::start(
                e,
                side,
                brush.size,
                *local,
                placed.0,
                self.gestures,
            ));
            return true;
        }
        false
    }

    /// The active brush's handles, alone in the selection or not; none for anything else.
    fn layout_sides(&mut self, world: &World, view: &ViewProj) {
        // A baked group's brushes are not drawn: no handles on them.
        let active = self.active().filter(|&e| baked_group(world, e).is_none());
        let active = active.and_then(|e| {
            // An edited brush's size is not its planes': its faces are edited instead (Tab).
            let size = world
                .get::<Brush>(e)
                .filter(|b| b.shape != BrushShape::Planes)?
                .size;
            Some((e, size, world.get::<GlobalTransform>(e)?.0))
        });
        match active {
            Some((e, size, placed)) => self.handles.layout(view, e, size, placed),
            None => self.handles.clear(),
        }
    }

    /// One frame of a gizmo drag ([`Editor::set_targets`]).
    fn apply_gizmo(&mut self, world: &mut World, drag: &GizmoDrag, change: Change) {
        let (step, merge) = (drag.handle.step(), drag.merge);
        self.set_targets(
            world,
            step,
            merge,
            &drag.targets,
            change,
            (drag.pivot, drag.axes),
        );
    }

    /// Sets every target from where it started under `change` about the pivot (a scale's
    /// factors along the axes: `gizmo::apply`), in undo step `name` merged by `merge`: one
    /// step per gesture. Only the parts that change are set, from the target's own
    /// values: a move sets the position, a turn the rotation (the turn seen from the
    /// parent), a scale the scale (its factors along the target's own axes), so a typed
    /// 0.5 or 90° is exact in the file.
    fn set_targets(
        &mut self,
        world: &mut World,
        name: &str,
        merge: u64,
        targets: &[Target],
        change: Change,
        (pivot, axes): (Vec3, [Vec3; 3]),
    ) {
        self.history.begin(name, Some(merge));
        for t in targets {
            self.history.touch(world, t.entity);
            // With own origins, each turns and scales about itself.
            let pivot = match change {
                Change::Turn(_) | Change::Scale(_) if self.pivot == Pivot::Origins => {
                    Vec3::from(t.world.translation)
                }
                _ => pivot,
            };
            let local = t.parent_inverse * gizmo::apply(change, t, pivot, axes);
            let (_, rotation, _) = t.world.to_scale_rotation_translation();
            // The parent's inverse turn: world to the parent's space.
            let (_, inverse, _) = t.parent_inverse.to_scale_rotation_translation();
            if let Some(tr) = world.get_mut::<Transform>(t.entity) {
                *tr = t.local;
                tr.translation = Vec3::from(local.translation);
                match change {
                    Change::Move(_) => {}
                    Change::Turn(q) => {
                        tr.rotation =
                            (inverse * q * inverse.inverse() * t.local.rotation).normalize();
                    }
                    Change::Scale(f) => {
                        tr.scale = t.local.scale * gizmo::own_factors(f, rotation, axes);
                    }
                }
            }
        }
        self.history.commit(world);
    }

    /// Starts a keyboard transform of the selection from the pointer (ADR 0107 E2-2):
    /// nothing while a drag runs.
    fn start_key_transform(&mut self, world: &World, kind: transform::Kind) {
        if self.key_transform.is_some()
            || self.gizmo_drag.is_some()
            || self.drag.is_some()
            || self.camera_drag
        {
            return;
        }
        let Some(pivot) = self.selection_pivot(world) else {
            self.say(String::from("Select something to move, rotate or scale"));
            return;
        };
        let Some(rect) = self.view_rect else {
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        let start = self
            .input
            .cursor()
            .map(|[x, y]| Vec2::new(x, y))
            .or_else(|| view.screen(pivot))
            .unwrap_or(Vec2::ZERO);
        let targets = self.transform_targets(world);
        let mut t = KeyTransform::new(pivot, self.own_axes(world), self.local_axes, targets);
        self.gestures += 1;
        t.begin(kind, &view, start, self.gestures);
        self.add_open = false;
        self.open_choice = None;
        self.key_transform = Some(t);
    }

    /// A keyboard transform's frame. Its keys (X, Y, Z and the transforms' letters by the
    /// letter on the key; Backspace, Enter, Escape), the typed value and the clicks are its
    /// own: the panels, the shortcuts and the camera do not get them. Then what it does with
    /// the pointer, set as one undo step. Enter or a left click keeps it; Escape, a right
    /// click or the window losing the keyboard takes it back; another transform's letter
    /// starts that one over.
    fn key_transform_frame(&mut self, world: &mut World) {
        /// Win32 virtual-key codes.
        const BACK: u16 = 0x08;
        const RETURN: u16 = 0x0D;
        const ESCAPE: u16 = 0x1B;
        let Some(rect) = self.view_rect else {
            return;
        };
        let Some(mut t) = self.key_transform.take() else {
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        let at = self
            .input
            .cursor()
            .map(|[x, y]| Vec2::new(x, y))
            .or_else(|| view.screen(t.pivot))
            .unwrap_or(Vec2::ZERO);
        let mut keep = self.input.pressed(Button::Left);
        let mut cancel = t.cancel || self.input.pressed(Button::Right);
        // Letters with Ctrl or Alt are not the transform's.
        let plain = !self.input.ctrl() && !self.input.alt();
        let plane = self.input.shift();
        let [move_key, turn_key, scale_key] = self.key_preset.transform_letters();
        for i in 0..self.input.letters().len() {
            let kind = match self.input.letters()[i] {
                BACK => {
                    t.backspace();
                    None
                }
                RETURN => {
                    keep = true;
                    None
                }
                ESCAPE => {
                    cancel = true;
                    None
                }
                vk @ 0x58..=0x5A if plain => {
                    t.constrain(usize::from(vk - 0x58), plane);
                    None
                }
                vk if plain && vk == move_key => Some(transform::Kind::Move),
                vk if plain && vk == turn_key => Some(transform::Kind::Turn),
                vk if plain && vk == scale_key => Some(transform::Kind::Scale),
                _ => None,
            };
            if let Some(kind) = kind
                && kind != t.kind
            {
                self.history.revert(world, t.merge);
                self.gestures += 1;
                t.begin(kind, &view, at, self.gestures);
            }
        }
        t.type_text(self.input.text());
        self.input.consume();
        if cancel {
            self.history.revert(world, t.merge);
            return;
        }
        let (invert, round) = (self.input.shift(), self.input.ctrl());
        if let Some(change) = t.change(&view, at, &self.snapping, invert, round)
            && change != t.last
        {
            t.last = change;
            let step = t.kind.step();
            self.set_targets(world, step, t.merge, &t.targets, change, t.frame());
        }
        t.hint(&mut self.hint, self.snapping.on != invert, round);
        if !keep {
            self.key_transform = Some(t);
        }
    }

    /// Whether the right button flies the camera now.
    fn flying(&self) -> bool {
        self.camera_drag && self.input.held(Button::Right) && !self.input.held(Button::Middle)
    }

    /// Flies the camera while the right button is held, as Godot's freelook: W, A, S, D
    /// along the view and across it, Q down and E up, by their place on the keyboard;
    /// Shift three times faster.
    fn fly(&mut self, dt: f32) {
        let held = |plus: Key, minus: Key| {
            f32::from(self.input.key_held(plus)) - f32::from(self.input.key_held(minus))
        };
        let way = Vec3::new(
            held(Key::KeyW, Key::KeyS),
            held(Key::KeyD, Key::KeyA),
            held(Key::KeyE, Key::KeyQ),
        );
        if way == Vec3::ZERO {
            return;
        }
        let fast = if self.input.shift() { FLY_FAST } else { 1.0 };
        let way = way.normalize() * self.fly_speed * fast * dt;
        self.orbit.fly(way.x, way.y, way.z);
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

    /// Disables `entities` (`on`) or enables them, as one undo step: off, they and what is
    /// below them are not drawn, light nothing and leave their blockout groups.
    fn set_disabled(&mut self, world: &mut World, entities: &[Entity], on: bool) {
        self.history
            .begin(if on { "Disable" } else { "Enable" }, None);
        for &e in entities {
            self.history.touch(world, e);
            if on {
                world.insert(e, Disabled {});
            } else {
                world.remove::<Disabled>(e);
            }
        }
        self.history.commit(world);
        let what = match entities {
            [e] => world
                .get::<Name>(*e)
                .map_or_else(|| String::from("the entity"), |n| n.as_str().to_owned()),
            _ => format!("{} entities", entities.len()),
        };
        self.say(if on {
            format!("{what} off: not drawn, with what is below (H turns it back on)")
        } else {
            format!("{what} on")
        });
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
    /// and selects it. A brush goes to the end of the selection's blockout group, or of the
    /// scene's first, or of a new "Blockout" group (ADR 0110).
    fn add(&mut self, world: &mut World, what: usize) {
        self.add_open = false;
        self.add_group = None;
        let step = self.snapping.position as f32;
        let at = (self.orbit.target / step).round() * step;
        let kind = ADD[what].0;
        self.history.begin(&format!("Add {kind}"), None);
        let e = if what >= FIRST_BRUSH {
            let group = self.brush_group_or_new(world);
            // Where the camera looks, in the group's space.
            let inverse = world
                .get::<GlobalTransform>(group)
                .map_or(Affine3A::IDENTITY, |g| g.0.inverse());
            let local = inverse.transform_point3(at);
            let name = self.sibling_name(world, Some(group), kind, None);
            let e = spawn(world, what, &name, local);
            let _ = world.set_parent(e, Some(group));
            self.collapsed.remove(&group);
            e
        } else {
            let name = self.sibling_name(world, None, kind, None);
            let order = self.last_root_order(world);
            let e = spawn(world, what, &name, at);
            world.insert(e, RootOrder(order));
            e
        };
        self.history.touch_new(world, e);
        self.history.commit(world);
        self.select_only(e);
    }

    /// The blockout group a new brush joins, as [`Editor::brush_group`] finds it, or the
    /// scene's first that is not baked, or, when there is none, a new one after the roots:
    /// in the open history step.
    fn brush_group_or_new(&mut self, world: &mut World) -> Entity {
        let live =
            |world: &World, g: Entity| world.get::<BrushGroup>(g).is_some_and(|b| !b.is_baked());
        let found = self.brush_group(world);
        let group = match found {
            Some(g) if live(world, g) => Some(g),
            Some(_) => self
                .roots(world, None)
                .into_iter()
                .find(|&r| live(world, r)),
            None => None,
        };
        if let Some(g) = group {
            self.history.touch(world, g);
            return g;
        }
        if let Some(baked) = found {
            let name = world
                .get::<Name>(baked)
                .map_or_else(|| String::from("The group"), |n| n.as_str().to_owned());
            self.say(format!(
                "{name} is baked: the new brush starts a group of its own"
            ));
        }
        let name = self.sibling_name(world, None, "Blockout", None);
        let order = self.last_root_order(world);
        let g = world.spawn((
            SceneId::new(),
            Name::new(&name),
            Transform::IDENTITY,
            GlobalTransform::default(),
            BrushGroup::default(),
            RootOrder(order),
        ));
        self.history.touch_new(world, g);
        g
    }

    /// Ctrl+H: the selected adding brushes become rooms with walls [`ROOM_WALLS`] thick
    /// or, when they all are, solid again, as one undo step.
    fn toggle_rooms(&mut self, world: &mut World) {
        let brushes: Vec<Entity> = self
            .selection
            .iter()
            .copied()
            .filter(|&e| world.get::<Brush>(e).is_some_and(|b| b.op == BrushOp::Add))
            .collect();
        if let Some(group) = brushes.iter().find_map(|&e| baked_group(world, e)) {
            self.say(baked_message(world, group));
            return;
        }
        if brushes.is_empty() {
            self.say(String::from(
                "Select brushes that add to make them rooms, or solid again (Ctrl+H)",
            ));
            return;
        }
        let rooms = brushes
            .iter()
            .all(|&e| world.get::<Brush>(e).is_some_and(Brush::is_room));
        let (hollow, name) = if rooms {
            (0.0, "Make Solid")
        } else {
            (ROOM_WALLS, "Make Room")
        };
        self.history.begin(name, None);
        for &e in &brushes {
            self.history.touch(world, e);
            if let Some(b) = world.get_mut::<Brush>(e) {
                b.hollow = hollow;
            }
        }
        self.history.commit(world);
    }

    /// B or Shift+B (`op`): a drawing starts with a box, or the one running takes the next
    /// shape or the other operation ([`Draw::again`]).
    fn start_draw(&mut self, op: BrushOp) {
        if let Some(d) = &mut self.draw {
            d.again(op);
            return;
        }
        if self.key_transform.is_some()
            || self.gizmo_drag.is_some()
            || self.drag.is_some()
            || self.camera_drag
        {
            return;
        }
        self.gestures += 1;
        self.draw = Some(Draw::new(op, 0, self.gestures));
        // The surface under the pointer is asked for again, all of the scene this time.
        self.surfaced = None;
        self.add_open = false;
        self.open_choice = None;
    }

    /// A drawing's frame ([`crate::draw`]): its keys (B, R, Tab, Backspace, Enter, Escape)
    /// and typed sizes, unless the right button flies the camera; the pointer's mark, base
    /// or height, and the clicks in the view; the brush as it now is, in its undo step.
    /// The camera keeps the middle button, the wheel and, before the first click, the
    /// right one; clicks outside the view stay the panels' until then.
    fn draw_frame(&mut self, world: &mut World) {
        /// Win32 virtual-key codes.
        const BACK: u16 = 0x08;
        const TAB: u16 = 0x09;
        const RETURN: u16 = 0x0D;
        const ESCAPE: u16 = 0x1B;
        const B: u16 = 0x42;
        const H: u16 = 0x48;
        const R: u16 = 0x52;
        let Some(rect) = self.view_rect else {
            return;
        };
        let Some(mut d) = self.draw.take() else {
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        let aiming = d.stage == Stage::Aim;
        let mut make = false;
        let mut cancel = d.cancel || (!aiming && self.input.pressed(Button::Right));
        if !self.flying() {
            let plain = !self.input.ctrl() && !self.input.alt();
            for i in 0..self.input.letters().len() {
                match self.input.letters()[i] {
                    BACK => d.backspace(),
                    TAB => d.tab(),
                    RETURN => make |= d.enter(),
                    ESCAPE => cancel = true,
                    B if plain => d.again(if self.input.shift() {
                        BrushOp::Cut
                    } else {
                        BrushOp::Add
                    }),
                    R if plain => d.turn(),
                    H if self.input.ctrl() && !self.input.alt() => d.toggle_room(),
                    _ => {}
                }
            }
            d.type_text(self.input.text());
        }
        let cursor = self.input.cursor().map(|[x, y]| Vec2::new(x, y));
        let over = cursor.is_some_and(|at| view.contains(at));
        self.collect_landing(world, self.drawn.map(|(e, _)| e));
        if let Some(at) = cursor
            && !cancel
        {
            let pointer = draw::Pointer {
                view: &view,
                at,
                surface: self
                    .surfaced
                    .and_then(|r| r.hit)
                    .map(|h| (h.point, h.normal)),
                press: self.input.pressed(Button::Left) && (over || !aiming),
                release: self.input.released(Button::Left),
                snap: &self.snapping,
                invert: self.input.shift(),
                round: self.input.ctrl(),
                corners: &self.land_corners,
                edges: &self.land_edges,
                reach: 8.0 * self.scale.max(0.5),
            };
            make |= d.pointer(&pointer);
            // The surface under the pointer for the mark, a few frames on.
            if d.stage == Stage::Aim && over {
                let local = (at - Vec2::new(rect[0] as f32, rect[1] as f32)).max(Vec2::ZERO);
                self.surface_request = Some(SurfaceRequest {
                    x: (local.x * self.view_scale[0]) as u32,
                    y: (local.y * self.view_scale[1]) as u32,
                    transparent: self.pick_transparent,
                });
            }
        }
        // The keys were the drawing's, and so are the clicks in the view, or all of them
        // once it has a corner; the camera keeps the rest.
        self.input.consume_keys();
        if over || !aiming {
            self.input.consume_press(Button::Left);
        }
        if !aiming {
            self.input.consume_press(Button::Right);
        }
        if cancel {
            self.history.revert(world, d.merge);
            self.drawn = None;
            return;
        }
        let made = d.made();
        self.set_drawn(world, d.merge, made);
        d.hint(&mut self.hint);
        self.draw_overlay(&d, &view);
        if make {
            match self.drawn {
                Some((e, _)) => {
                    self.drawn = None;
                    self.select_only(e);
                    return;
                }
                None => self.say(String::from(
                    "A brush needs a size on every side: move the pointer, or type it",
                )),
            }
        }
        self.draw = Some(d);
    }

    /// Blockout group `group` written as a binary glTF beside the scene (ADR 0110 E2b-6),
    /// named after the group (`blockout.glb`), over the file of that name: what to say.
    fn export(&self, blockout: &engine_app::Blockout, world: &World, group: Entity) -> String {
        let mut brushes = Vec::new();
        blockout.baked(group, &mut brushes);
        let name = world
            .get::<Name>(group)
            .map_or_else(|| String::from("Blockout"), |n| n.as_str().to_owned());
        if brushes.is_empty() {
            return format!("{name} shows nothing to export");
        }
        let dir = self
            .scene_path
            .as_ref()
            .and_then(|p| p.parent())
            .map_or_else(|| self.root.clone(), |d| self.root.join(d));
        let path = dir.join(engine_app::bake::file_name(&name));
        let baked = engine_app::bake::glb(&name, &brushes, &engine_app::grid_png());
        let triangles: usize = brushes.iter().map(|b| b.indices.len() / 3).sum();
        match std::fs::write(&path, baked.bytes) {
            Ok(()) => format!(
                "Exported {name} to {} ({triangles} triangles)",
                path.display()
            ),
            Err(e) => format!("{name} not exported: {}: {e}", path.display()),
        }
    }

    /// Play begins (ADR 0107 E3): the tools and gestures end, the editor's camera goes
    /// (the game's cameras show the world), and the world is kept as it is: its scene
    /// entities, its resources, the history's place and the selection.
    fn prepare_play(&mut self, world: &mut World, from_here: bool) -> Playing {
        let started = Instant::now();
        if let Some(mut drag) = self.asset_drag.take() {
            self.take_back_placed(world, &mut drag);
        }
        self.key_transform = None;
        self.draw = None;
        self.face_edit = None;
        self.drag = None;
        self.gizmo_drag = None;
        self.side_drag = None;
        self.box_drag = None;
        self.camera_drag = false;
        self.gizmo_shown = false;
        self.outline_items.clear();
        self.handles.clear();
        if let Some(camera) = self.camera.take() {
            world.despawn(camera);
        }
        let selection = self
            .selection
            .iter()
            .filter_map(|&e| world.get::<SceneId>(e).copied())
            .collect();
        let resources = world.resource_types().collect();
        let mark = self.history.begin_play();
        let snapshot = self.history.snapshot_world(world);
        Playing {
            snapshot,
            resources,
            mark,
            selection,
            started,
            entered: Duration::ZERO,
            from_here,
            paused: false,
            keep: Vec::new(),
            keep_gone: Vec::new(),
            ejected: None,
        }
    }

    /// Ctrl+K while playing (Unreal's K): the selection's changes kept at Stop, or not
    /// when they all are.
    fn keep_selection(&mut self, world: &World) {
        let Some(p) = self.play.as_mut() else {
            self.say(String::from(
                "Changes are kept from Play: F5 plays, then Ctrl+K keeps the selection's",
            ));
            return;
        };
        if self.selection.is_empty() {
            self.say(String::from("Select what to keep, then Ctrl+K"));
            return;
        }
        let all = self.selection.iter().all(|e| p.keep.contains(e));
        for &e in &self.selection {
            let at = p.keep.iter().position(|k| *k == e);
            match (all, at) {
                (true, Some(i)) => {
                    p.keep.swap_remove(i);
                }
                (false, None) => p.keep.push(e),
                _ => {}
            }
        }
        let n = self.selection.len();
        let what = if n == 1 {
            world
                .get::<Name>(self.selection[0])
                .map_or_else(|| String::from("1 entity"), |name| name.as_str().to_owned())
        } else {
            format!("{n} entities")
        };
        self.say(if all {
            format!("{what}: changes not kept")
        } else {
            format!("{what}: changes kept at Stop (Ctrl+K again: not)")
        });
    }

    /// "Keep Changes…": what changed since Play, each line kept at Stop or not.
    fn keep_list(&mut self, world: &mut World) {
        let Some(p) = self.play.as_ref() else {
            self.say(String::from("Nothing is playing (F5 plays the scene)"));
            return;
        };
        let changes = self.history.changes_since(world, &p.snapshot);
        let kept = |item: &PlayChange| match item {
            PlayChange::Changed { entity, .. } | PlayChange::Made { entity } => {
                p.keep.contains(entity)
            }
            PlayChange::Gone { id } => p.keep_gone.contains(id),
        };
        let all = !changes.is_empty() && changes.iter().all(kept);
        let mut items = vec![Choice {
            label: String::from(if all {
                "Keep none of them"
            } else {
                "Keep every change"
            }),
            detail: format!("{} changed since Play", changes.len()),
            pick: Pick::Keep(KeepItem::All),
        }];
        for change in &changes {
            let mark = if kept(change) { "kept" } else { "not kept" };
            let (label, what, item) = match change {
                PlayChange::Changed { entity, components } => (
                    label_of(world, *entity),
                    components.join(", "),
                    KeepItem::Entity(*entity),
                ),
                PlayChange::Made { entity } => (
                    label_of(world, *entity),
                    String::from("made while playing"),
                    KeepItem::Entity(*entity),
                ),
                PlayChange::Gone { id } => (
                    p.snapshot
                        .name_of(*id)
                        .map_or_else(|| format!("an entity ({id})"), str::to_owned),
                    String::from("despawned while playing"),
                    KeepItem::Gone(*id),
                ),
            };
            items.push(Choice {
                label,
                detail: format!("{what} · {mark}"),
                pick: Pick::Keep(item),
            });
        }
        self.open_list(world, "Pick what Stop keeps (Enter toggles)", items);
    }

    /// A line of "Keep Changes…" picked: kept or not, and the list again.
    fn toggle_keep(&mut self, world: &mut World, item: KeepItem) {
        let Some(p) = self.play.as_mut() else {
            return;
        };
        match item {
            KeepItem::Entity(e) => match p.keep.iter().position(|k| *k == e) {
                Some(i) => {
                    p.keep.swap_remove(i);
                }
                None => p.keep.push(e),
            },
            KeepItem::Gone(id) => match p.keep_gone.iter().position(|k| *k == id) {
                Some(i) => {
                    p.keep_gone.swap_remove(i);
                }
                None => p.keep_gone.push(id),
            },
            KeepItem::All => {
                let changes = self.history.changes_since(world, &p.snapshot);
                let kept = |c: &PlayChange| match c {
                    PlayChange::Changed { entity, .. } | PlayChange::Made { entity } => {
                        p.keep.contains(entity)
                    }
                    PlayChange::Gone { id } => p.keep_gone.contains(id),
                };
                let all = !changes.is_empty() && changes.iter().all(kept);
                p.keep.clear();
                p.keep_gone.clear();
                if !all {
                    for c in &changes {
                        match c {
                            PlayChange::Changed { entity, .. } | PlayChange::Made { entity } => {
                                p.keep.push(*entity);
                            }
                            PlayChange::Gone { id } => p.keep_gone.push(*id),
                        }
                    }
                }
            }
        }
        self.keep_list(world);
    }

    /// Whether the viewport is the game's: playing and not ejected.
    fn game_view(&self) -> bool {
        self.play.as_ref().is_some_and(|p| p.ejected.is_none())
    }

    /// "Play from Here": the editor's camera as the game's start (ADR 0107 E3): the
    /// scene's main camera put there, and `PlayFrom` in the world for the game's player
    /// (both put back at Stop, with the rest of the scene).
    fn play_from_here(&mut self, world: &mut World) {
        let eye = self.orbit.eye();
        let rotation = look_rotation(eye, self.orbit.target, Vec3::Y);
        world.insert_resource(engine_app::PlayFrom {
            position: eye,
            rotation,
        });
        let mut cameras = QueryState::<(Entity, &Camera), engine_ecs::With<SceneId>>::new(world);
        let main = cameras
            .iter_readonly(world)
            .filter(|(_, c)| c.active != 0)
            .max_by_key(|(_, c)| c.priority)
            .map(|(e, _)| e);
        let Some(main) = main else {
            return;
        };
        // In its parent's space, so it lands at the editor's camera in the world.
        let parent = world
            .parent(main)
            .and_then(|p| world.get::<GlobalTransform>(p))
            .map_or(Affine3A::IDENTITY, |g| g.0);
        let local = parent.inverse() * Affine3A::from_rotation_translation(rotation, eye);
        let (_, rotation, translation) = local.to_scale_rotation_translation();
        if let Some(t) = world.get_mut::<Transform>(main) {
            t.translation = translation;
            t.rotation = rotation;
        }
    }

    /// Eject, or return to the game (Unreal's F8): the editor's camera and tools on the
    /// running world, the game's keyboard and mouse off meanwhile.
    fn eject(&mut self, world: &mut World) {
        let Some(p) = self.play.as_mut() else {
            self.say(String::from("Nothing is playing (F5 plays the scene)"));
            return;
        };
        let message = match p.ejected.take() {
            None => {
                let input = world.resource_mut::<engine_input::Input>();
                let devices = input.map(|input| {
                    let devices = input.devices();
                    input.set_devices(engine_input::DeviceFilter {
                        keyboard_mouse: false,
                        gamepads: 0,
                    });
                    input.release_all();
                    devices
                });
                p.ejected = Some(devices);
                "Ejected: the editor's camera and tools on the running game (Shift+F8 returns)"
            }
            Some(devices) => {
                if let (Some(devices), Some(input)) =
                    (devices, world.resource_mut::<engine_input::Input>())
                {
                    input.set_devices(devices);
                }
                "Back in the game"
            }
        };
        self.say(String::from(message));
    }

    /// Play ends: the resources the game added go, the world is put back as Play found
    /// it, the steps made while playing are forgotten and the selection comes back. What
    /// to say.
    fn finish_play(&mut self, world: &mut World, playing: Playing, why: Option<String>) -> String {
        let t = Instant::now();
        if let Some(Some(devices)) = playing.ejected
            && let Some(input) = world.resource_mut::<engine_input::Input>()
        {
            input.set_devices(devices);
        }
        let now: Vec<TypeId> = world.resource_types().collect();
        for kind in now {
            if !playing.resources.contains(&kind) {
                world.remove_resource_of(kind);
            }
        }
        // What Stop keeps, taken before the world is put back.
        let kept = if playing.keep.is_empty() && playing.keep_gone.is_empty() {
            None
        } else {
            Some(
                self.history
                    .capture_kept(world, &playing.keep, &playing.keep_gone),
            )
        };
        self.history.restore_world(world, &playing.snapshot);
        self.history.end_play(playing.mark);
        let kept = kept.filter(|k| !k.is_empty()).map_or(0, |kept| {
            self.history.begin("Keep Changes", None);
            self.history.keep(world, &kept);
            self.history.commit(world);
            kept.len()
        });
        let ids = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        let index: HashMap<SceneId, Entity> =
            ids.iter_readonly(world).map(|(e, id)| (*id, e)).collect();
        self.selection = playing
            .selection
            .iter()
            .filter_map(|id| index.get(id).copied())
            .collect();
        self.anchor = None;
        let stopped = t.elapsed().as_secs_f64() * 1e3;
        let played = playing.started.elapsed().as_secs_f64();
        log::info!(
            target: "engine_editor",
            "play: entered in {:.1} ms, played {played:.1} s, the scene put back in {stopped:.1} ms",
            playing.entered.as_secs_f64() * 1e3
        );
        match (why, kept) {
            (Some(why), _) => format!("Play stopped: {why}"),
            (None, 0) => format!("Stopped after {played:.0} s: the scene is as it was"),
            (None, n) => {
                let s = if n == 1 { "y" } else { "ies" };
                format!(
                    "Stopped after {played:.0} s: kept the changes of {n} entit{s} (Ctrl+Z takes them back), the rest as it was"
                )
            }
        }
    }

    /// The blockout groups the selection is in or, when it is in none, every group of the
    /// scene: what Bake to Mesh and Unbake act on.
    fn command_groups(&mut self, world: &mut World) -> Vec<Entity> {
        let mut groups: Vec<Entity> = Vec::new();
        for &e in &self.selection {
            if let Some(g) = group_of(world, e)
                && !groups.contains(&g)
            {
                groups.push(g);
            }
        }
        if groups.is_empty() {
            let mut stack = self.roots(world, None);
            stack.reverse();
            while let Some(e) = stack.pop() {
                if world.get::<BrushGroup>(e).is_some() {
                    groups.push(e);
                }
                let children: Vec<Entity> = world.children(e).collect();
                stack.extend(children.into_iter().rev());
            }
        }
        groups
    }

    /// Blockout: Bake to Mesh (ADR 0110 E2b-6): each of `groups` written as a glTF beside
    /// the scene (named after it, or the file it was baked into before), a model of each
    /// of the file's meshes put in it and its brushes no longer drawn, as one undo step;
    /// the player reads the files again. What to say.
    fn bake(&mut self, player: &mut Player, groups: &[Entity]) -> String {
        let dir = self
            .scene_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let mut brushes = Vec::new();
        let mut made: Vec<Baked> = Vec::new();
        let mut said: Vec<String> = Vec::new();
        for &group in groups {
            let world = &player.runtime().world;
            let name = world
                .get::<Name>(group)
                .map_or_else(|| String::from("Blockout"), |n| n.as_str().to_owned());
            player.blockout().baked(group, &mut brushes);
            if brushes.is_empty() {
                said.push(format!("{name} shows nothing to bake"));
                continue;
            }
            let before = world
                .get::<BrushGroup>(group)
                .map_or(AssetRef::NONE, |b| b.baked);
            // The file it was baked into, else one named after it that no other group's
            // bake holds.
            let taken: Vec<AssetId> = player
                .blockout()
                .groups()
                .filter(|&(g, _)| g != group)
                .filter_map(|(g, _)| world.get::<BrushGroup>(g).map(|b| b.baked.file))
                .chain(made.iter().map(|m| m.file))
                .filter(|f| !f.is_none())
                .collect();
            let file = self
                .assets
                .get(before)
                .filter(|_| !before.is_none())
                .map(str::to_owned)
                .unwrap_or_else(|| self.bake_path(&dir, &name, &taken, &made));
            let full = self.root.join(&file);
            let baked = engine_app::bake::glb(&name, &brushes, &engine_app::grid_png());
            let id = std::fs::write(&full, &baked.bytes)
                .map_err(|e| format!("{}: {e}", full.display()))
                .and_then(|()| AssetMeta::ensure(&full));
            match id {
                Ok(id) => {
                    let triangles = brushes.iter().map(|b| b.indices.len() / 3).sum();
                    made.push(Baked {
                        group,
                        name,
                        before: before.file,
                        file: id,
                        path: file,
                        parts: baked.parts,
                        triangles,
                    });
                }
                Err(e) => said.push(format!("{name} not baked: {e}")),
            }
        }
        if made.is_empty() {
            return said.join("; ");
        }
        // The files' paths known, their parts read anew, then the scene's step.
        let mut paths = AssetPaths::new();
        for m in &made {
            paths.insert(AssetRef::file(m.file), m.path.clone());
            for (mesh, material) in &m.parts {
                paths.insert(AssetRef::part(m.file, mesh), format!("{}#{mesh}", m.path));
                paths.insert(
                    AssetRef::part(m.file, material),
                    format!("{}#{material}", m.path),
                );
            }
        }
        player.assets_mut().add_paths(&paths);
        self.assets.extend(&paths);
        for m in &made {
            player.reload_asset(m.file);
        }
        // The next frame waits for the files: the models show as the brushes go.
        player.wait_for_assets();
        let world = player.world_mut();
        self.history.begin("Bake to Mesh", None);
        for m in &made {
            self.history.touch(world, m.group);
            // Its models from an earlier bake go.
            for c in baked_models(world, m.group, &[m.before, m.file]) {
                self.history.delete_subtree(world, c);
            }
            if let Some(b) = world.get_mut::<BrushGroup>(m.group) {
                b.baked = AssetRef::file(m.file);
            }
            for (mesh, material) in &m.parts {
                let e = world.spawn((
                    SceneId::new(),
                    Name::new(mesh),
                    Transform::IDENTITY,
                    GlobalTransform::default(),
                    Model::new(
                        AssetRef::part(m.file, mesh),
                        AssetRef::part(m.file, material),
                    ),
                ));
                let _ = world.set_parent(e, Some(m.group));
                self.history.touch_new(world, e);
            }
        }
        self.history.commit(world);
        self.selection.retain(|&e| world.contains(e));
        let models: usize = made.iter().map(|m| m.parts.len()).sum();
        let triangles: usize = made.iter().map(|m| m.triangles).sum();
        let (what, its) = match made.as_slice() {
            [m] => (format!("{} into {}", m.name, m.path), "its"),
            _ => (format!("{} groups", made.len()), "their"),
        };
        let s = if models == 1 { "" } else { "s" };
        said.insert(
            0,
            format!(
                "Baked {what}: {models} model{s}, {triangles} triangles; {its} brushes still collide (Blockout: Unbake to edit them)"
            ),
        );
        said.join("; ")
    }

    /// A file for group `name`'s bake in folder `dir` (relative to the game's): named after
    /// it, or with `-2`, `-3`… when another group's bake (`taken`, `made`) holds that name.
    fn bake_path(&self, dir: &str, name: &str, taken: &[AssetId], made: &[Baked]) -> String {
        let file = engine_app::bake::file_name(name);
        let stem = file.trim_end_matches(".glb");
        let join = |f: &str| {
            if dir.is_empty() {
                f.to_owned()
            } else {
                format!("{dir}/{f}")
            }
        };
        let held = |path: &str| {
            if made.iter().any(|m| m.path == path) {
                return true;
            }
            let meta = AssetMeta::path(&self.root.join(path));
            std::fs::read_to_string(meta)
                .ok()
                .and_then(|text| AssetMeta::parse(&text).ok())
                .and_then(|(_, report)| report.id)
                .is_some_and(|id| taken.contains(&id))
        };
        let mut path = join(&file);
        let mut k = 2;
        while held(&path) {
            path = join(&format!("{stem}-{k}.glb"));
            k += 1;
        }
        path
    }

    /// Blockout: Unbake (ADR 0110 E2b-6): the selection's baked groups (or every baked
    /// group) live again, their brushes drawn and their bakes' models gone, as one undo
    /// step. The files stay.
    fn unbake(&mut self, world: &mut World) {
        let groups: Vec<Entity> = self
            .command_groups(world)
            .into_iter()
            .filter(|&g| world.get::<BrushGroup>(g).is_some_and(BrushGroup::is_baked))
            .collect();
        if groups.is_empty() {
            self.say(String::from("No baked blockout groups to unbake"));
            return;
        }
        self.history.begin("Unbake", None);
        for &g in &groups {
            let file = world
                .get::<BrushGroup>(g)
                .map_or(AssetId::NONE, |b| b.baked.file);
            self.history.touch(world, g);
            for c in baked_models(world, g, &[file]) {
                self.history.delete_subtree(world, c);
            }
            if let Some(b) = world.get_mut::<BrushGroup>(g) {
                b.baked = AssetRef::NONE;
            }
        }
        self.history.commit(world);
        self.selection.retain(|&e| world.contains(e));
        let message = match groups.as_slice() {
            [g] => {
                let name = world
                    .get::<Name>(*g)
                    .map_or_else(|| String::from("The group"), |n| n.as_str().to_owned());
                format!("Unbaked {name}: its brushes are drawn again")
            }
            _ => format!(
                "Unbaked {} groups: their brushes are drawn again",
                groups.len()
            ),
        };
        self.say(message);
    }

    /// The navigation's frame (ADR 0110 E2b-4b): a bake's answer taken; a bake started in
    /// the background once the brushes have stayed as they are for [`NAV_WAIT`] and no
    /// gesture runs, from the visible surface of the brushes that collide, for a human-sized
    /// agent (`BakeSettings::default`).
    fn navigation_frame(&mut self, blockout: &engine_app::Blockout, world: &World) {
        let key = blockout_key(blockout, world);
        if key != self.nav_key && self.nav_changed.as_ref().is_none_or(|(_, k)| *k != key) {
            self.nav_changed = Some((Instant::now(), key));
        }
        if let Some(job) = &self.nav_job {
            match job.try_recv() {
                Ok(Ok(baked)) => {
                    self.nav_job = None;
                    let tiles = if baked.tiles == 1 { "tile" } else { "tiles" };
                    self.say(format!(
                        "Navigation: {} {tiles} from {} triangles in {:.0} ms",
                        baked.tiles, baked.triangles, baked.ms
                    ));
                    self.nav_edges = baked.edges;
                }
                Ok(Err(e)) => {
                    self.nav_job = None;
                    self.say(format!("Navigation not baked: {e}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.nav_job = None,
            }
        }
        let due = self
            .nav_changed
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() >= NAV_WAIT);
        if self.nav_job.is_some() || !due || self.gesture() {
            return;
        }
        let Some((_, key)) = self.nav_changed.take() else {
            return;
        };
        self.nav_key = key;
        // Half the cores, at most four: 400 rooms (357 tiles) bake in 0.15 s on four
        // workers, 0.61 s on one (`engine_nav` tests, ADR 0110).
        let pool = match &self.nav_pool {
            Some(pool) => std::sync::Arc::clone(pool),
            None => {
                let workers =
                    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(1, 4));
                match engine_core::jobs::ThreadPool::builder()
                    .workers(workers)
                    .thread_name("ofelia-navigation")
                    .build()
                {
                    Ok(pool) => {
                        let pool = std::sync::Arc::new(pool);
                        self.nav_pool = Some(std::sync::Arc::clone(&pool));
                        pool
                    }
                    Err(e) => {
                        self.say(format!("Navigation not baked: {e}"));
                        return;
                    }
                }
            }
        };
        let (mut vertices, mut triangles) = (Vec::new(), Vec::new());
        blockout.surface_triangles(world, &mut vertices, &mut triangles);
        let (send, answer) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(String::from("ofelia-navigation-bake"))
            .spawn(move || {
                let start = Instant::now();
                let settings = engine_nav::BakeSettings::default();
                let result = pool
                    .install(|| engine_nav::bake(&vertices, &triangles, &settings, Some(&pool)))
                    .and_then(|data| {
                        let mesh = engine_nav::NavMesh::new(&data)?;
                        let mut edges = Vec::new();
                        mesh.edges(&mut edges);
                        Ok(NavBaked {
                            edges,
                            tiles: data.tiles.len(),
                            triangles: triangles.len(),
                            ms: start.elapsed().as_secs_f64() * 1e3,
                        })
                    })
                    .map_err(|e| e.to_string());
                let _ = send.send(result);
            });
        match spawned {
            Ok(_) => self.nav_job = Some(answer),
            Err(e) => self.say(format!("Navigation not baked: {e}")),
        }
    }

    /// The corners and edges a drawing lands on (team decision 2a): each brush's box, a
    /// room's walls' box too, in the world; `except` (the brush being drawn) and brushes
    /// that are off left out.
    fn collect_landing(&mut self, world: &mut World, except: Option<Entity>) {
        self.land_corners.clear();
        self.land_edges.clear();
        let q = self
            .brush_boxes
            .get_or_insert_with(|| QueryState::new(world));
        let world = &*world;
        for (e, brush, placed) in q.iter_readonly(world) {
            if Some(e) == except || is_off(world, e) {
                continue;
            }
            let (lo, hi) = (
                Vec3::new(-brush.size.x * 0.5, 0.0, -brush.size.z * 0.5),
                Vec3::new(brush.size.x * 0.5, brush.size.y, brush.size.z * 0.5),
            );
            let walls = Vec3::splat(brush.hollow);
            let boxes = if brush.is_room() {
                &[(lo, hi), (lo - walls, hi + walls)][..]
            } else {
                &[(lo, hi)][..]
            };
            for &(lo, hi) in boxes {
                let corners: [Vec3; 8] = core::array::from_fn(|i| {
                    let pick = |bit: usize, a: f32, b: f32| if i & bit != 0 { b } else { a };
                    placed.0.transform_point3(Vec3::new(
                        pick(1, lo.x, hi.x),
                        pick(2, lo.y, hi.y),
                        pick(4, lo.z, hi.z),
                    ))
                });
                self.land_corners.extend_from_slice(&corners);
                for (i, &c) in corners.iter().enumerate() {
                    for bit in [1, 2, 4] {
                        if i & bit == 0 {
                            self.land_edges.push((c, corners[i | bit]));
                        }
                    }
                }
            }
        }
    }

    /// The brush being drawn as `made`, in its drawing's undo step `merge`: spawned in its
    /// group (made too when the scene has none) once it has a size, changed as it changes,
    /// and gone again while it has none.
    fn set_drawn(&mut self, world: &mut World, merge: u64, made: Option<Made>) {
        match (made, self.drawn) {
            (Some(m), Some((_, last))) if m == last => {}
            (Some(m), drawn) => {
                // "Box Brush", "Stairs Cut", "Room", "Cylinder Room".
                let kind = match (m.op, m.hollow > 0.0, m.shape.name) {
                    (BrushOp::Cut, _, shape) => format!("{shape} Cut"),
                    (_, true, "Box") => String::from("Room"),
                    (_, true, shape) => format!("{shape} Room"),
                    (_, false, shape) => format!("{shape} Brush"),
                };
                self.history.begin(&format!("Draw {kind}"), Some(merge));
                let e = match drawn {
                    Some((e, _)) => {
                        self.history.touch(world, e);
                        e
                    }
                    None => {
                        let group = self.brush_group_or_new(world);
                        let name = self.sibling_name(world, Some(group), &kind, None);
                        let e = world.spawn((
                            SceneId::new(),
                            Name::new(&name),
                            Transform::IDENTITY,
                            GlobalTransform::default(),
                        ));
                        let _ = world.set_parent(e, Some(group));
                        self.history.touch_new(world, e);
                        self.collapsed.remove(&group);
                        e
                    }
                };
                // Its place in its group's space.
                let inverse = world
                    .parent(e)
                    .and_then(|g| world.get::<GlobalTransform>(g))
                    .map_or(Affine3A::IDENTITY, |g| g.0.inverse());
                let local = inverse * Affine3A::from_rotation_translation(m.rotation, m.at);
                let (scale, rotation, translation) = local.to_scale_rotation_translation();
                let mut brush = Brush::new(m.shape.shape, m.size)
                    .with_op(m.op)
                    .with_top(m.shape.top)
                    .with_hollow(m.hollow);
                if let Some(steps) = m.steps {
                    brush.segments = steps;
                }
                let mut transform = Transform::from_translation(translation);
                transform.rotation = rotation;
                transform.scale = scale;
                world.insert(e, transform);
                world.insert(e, brush);
                self.history.commit(world);
                self.drawn = Some((e, m));
            }
            (None, Some(_)) => {
                self.history.revert(world, merge);
                self.drawn = None;
            }
            (None, None) => {}
        }
    }

    /// What the overlay shows of drawing `d` this frame: before the first click its mark,
    /// a square on the surface's plane around the corner with the plane's two directions
    /// through it; then the box's edges and its sizes.
    fn draw_overlay(&mut self, d: &Draw, view: &ViewProj) {
        self.draw_lines.clear();
        self.draw_landed = d.landed.and_then(|l| {
            let corner = matches!(l, Landed::Corner(_));
            view.screen(l.point()).map(|at| (at, corner))
        });
        let mut labels = core::mem::take(&mut self.draw_labels);
        let mut shown = 0;
        if let (Stage::Aim, Some((corner, normal))) = (d.stage, d.mark) {
            let plane = draw::BasePlane::new(corner, normal);
            let r = view.per_pixel(corner) * 7.0 * self.scale.max(0.5);
            let square = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
                .map(|(a, b): (f32, f32)| corner + (plane.u * a + plane.v * b) * r);
            for i in 0..4 {
                self.draw_edges.push((square[i], square[(i + 1) % 4]));
            }
            self.draw_edges
                .push((corner - plane.u * r * 2.0, corner + plane.u * r * 2.0));
            self.draw_edges
                .push((corner - plane.v * r * 2.0, corner + plane.v * r * 2.0));
        } else {
            d.edges(&mut self.draw_edges);
            d.labels(&mut self.draw_sizes);
            for &(p, size) in &self.draw_sizes {
                if let Some(at) = view.screen(p) {
                    if shown == labels.len() {
                        labels.push((at, String::new()));
                    }
                    let (place, text) = &mut labels[shown];
                    *place = at;
                    text.clear();
                    let _ = write!(text, "{} m", transform::short(size));
                    shown += 1;
                }
            }
        }
        for &(a, b) in &self.draw_edges {
            if let (Some(a), Some(b)) = (view.screen(a), view.screen(b)) {
                self.draw_lines.push((a, b));
            }
        }
        self.draw_edges.clear();
        labels.truncate(shown);
        self.draw_labels = labels;
    }

    /// The blockout group a new brush joins: the active entity's (itself, or the group
    /// above it), else the scene's first.
    fn brush_group(&mut self, world: &mut World) -> Option<Entity> {
        let mut at = self.active();
        while let Some(e) = at {
            if world.get::<BrushGroup>(e).is_some() {
                return Some(e);
            }
            at = world.parent(e);
        }
        self.roots(world, None)
            .into_iter()
            .find(|&r| world.get::<BrushGroup>(r).is_some())
    }

    /// Runs a command.
    fn run(&mut self, world: &mut World, id: CommandId) {
        self.commands.ran(id);
        let ids = self.ids;
        // What writes the scene's files works on the scene, not on the play world.
        if self.play.is_some() && [ids.save, ids.restart, ids.bake].contains(&id) {
            self.say(String::from("The scene is playing: F8 stops it first"));
            return;
        }
        if id == ids.undo || id == ids.redo {
            self.drag = None;
            self.side_drag = None;
            if let Some(f) = &mut self.face_edit {
                f.drag = None;
                f.corner_drag = None;
            }
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
        } else if let Some(tool) = ids.tools.iter().position(|&t| t == id) {
            // In our keys the select tool (all the handles) has no key of its own: the active
            // tool's key again takes it.
            let back = tool == self.active_tool && self.key_preset == KeyPreset::Ofelia;
            self.active_tool = if back { 0 } else { tool };
        } else if let Some(i) = ids.transforms.iter().position(|&t| t == id) {
            let kinds = [
                transform::Kind::Move,
                transform::Kind::Turn,
                transform::Kind::Scale,
            ];
            self.start_key_transform(world, kinds[i]);
        } else if id == ids.keys {
            self.pick_keys(world);
        } else if id == ids.drop {
            self.drop_selection(world);
        } else if id == ids.draw || id == ids.draw_cut {
            let op = if id == ids.draw {
                BrushOp::Add
            } else {
                BrushOp::Cut
            };
            self.start_draw(op);
        } else if id == ids.hollow {
            self.toggle_rooms(world);
        } else if id == ids.edit_faces {
            self.toggle_face_edit(world);
        } else if id == ids.export {
            match self.brush_group(world) {
                Some(group) => self.export_group = Some(group),
                None => self.say(String::from(
                    "Select a blockout group, or a brush in one, to export it",
                )),
            }
        } else if id == ids.bake {
            let groups = self.command_groups(world);
            if groups.is_empty() {
                self.say(String::from("The scene has no blockout groups to bake"));
            } else {
                self.bake_groups = groups;
            }
        } else if id == ids.unbake {
            self.unbake(world);
        } else if id == ids.play || id == ids.play_from_here {
            if self.play.is_none() {
                let from_here = id == ids.play_from_here;
                self.play_request = Some(PlayRequest::Play { from_here });
            }
        } else if id == ids.restart_play {
            self.play_request = Some(match self.play {
                Some(_) => PlayRequest::Restart,
                None => PlayRequest::Play { from_here: false },
            });
        } else if id == ids.pause || id == ids.step {
            if self.play.is_some() {
                self.play_request = Some(if id == ids.pause {
                    PlayRequest::Pause
                } else {
                    PlayRequest::Step
                });
            } else {
                self.say(String::from("Nothing is playing (F5 plays the scene)"));
            }
        } else if id == ids.eject {
            self.eject(world);
        } else if id == ids.play_where {
            self.play_in_window = !self.play_in_window;
            self.say(String::from(if self.play_in_window {
                "Play shows the game in a window of its own"
            } else {
                "Play shows the game in the viewport"
            }));
        } else if id == ids.keep {
            self.keep_selection(world);
        } else if id == ids.keep_list {
            self.keep_list(world);
        } else if id == ids.stop {
            if self.play.is_some() {
                self.play_request = Some(PlayRequest::Stop);
            } else {
                self.say(String::from("Nothing is playing (F5 plays the scene)"));
            }
        } else if let Some(k) = ids.steps.iter().position(|&c| c == id) {
            self.pick_step(world, k);
        } else if id == ids.navigation {
            self.show_nav = !self.show_nav;
            self.nav_key.clear();
            self.say(String::from(if self.show_nav {
                "Navigation shown: where an agent 1.8 m tall walks, baked a moment after each edit"
            } else {
                "Navigation hidden"
            }));
        } else if id == ids.colliders {
            self.show_colliders = !self.show_colliders;
            self.collider_key.clear();
            self.say(String::from(if self.show_colliders {
                "Colliders shown: the brushes' pieces that collide"
            } else {
                "Colliders hidden"
            }));
        } else if id == ids.overlays {
            self.overlays = !self.overlays;
            self.write_session(world);
            self.say(String::from(if self.overlays {
                "Overlays shown: lights' and cameras' icons, the selected ones' reach"
            } else {
                "Overlays hidden"
            }));
        } else if id == ids.pivot {
            self.pivot = self.pivot.next();
            self.write_session(world);
            self.say(String::from(match self.pivot {
                Pivot::Median => "Rotating and scaling about the selection's middle",
                Pivot::Origins => "Rotating and scaling each about its own origin",
                Pivot::Active => "Rotating and scaling about the active entity",
            }));
        } else if id == ids.frame {
            self.frame_selection(world);
        } else if id == ids.follow {
            self.toggle_follow(world);
        } else if let Some(n) = ids.go_bookmark.iter().position(|&b| b == id) {
            self.bookmark(world, n, false);
        } else if let Some(n) = ids.set_bookmark.iter().position(|&b| b == id) {
            self.bookmark(world, n, true);
        } else if id == ids.pick_transparent {
            self.pick_transparent = !self.pick_transparent;
            // The pointer's pixel is picked again the new way.
            self.last_pick = None;
            self.write_session(world);
            self.say(String::from(if self.pick_transparent {
                "Clicks pick transparent surfaces"
            } else {
                "Clicks pick through transparent surfaces"
            }));
        } else if id == ids.local_axes {
            self.local_axes = !self.local_axes;
            self.say(String::from(if self.local_axes {
                "The gizmo along the active entity's axes"
            } else {
                "The gizmo along the world's axes"
            }));
        } else if id == ids.disable {
            let selection: Vec<Entity> = self.selection.clone();
            if !selection.is_empty() {
                // Any of them on: all off; all off: all on.
                let on = selection
                    .iter()
                    .any(|&e| world.get::<Disabled>(e).is_none());
                self.set_disabled(world, &selection, on);
            }
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

    /// The project's asset database: opened from its index, scanned (foreign files met
    /// without a `.meta` given one), the index written, the folder watched. What it found.
    fn open_assets(&mut self) -> engine_scene::ScanReport {
        let mut db = engine_scene::AssetDatabase::open(&self.root);
        let report = db.scan();
        if let Err(e) = db.save() {
            log::warn!(target: "engine_editor", "the asset index: {e}");
        }
        let paths = db.paths();
        self.assets.extend(&paths);
        self.pending_assets.extend(&paths);
        self.asset_db = db;
        self.refresh_browser();
        self.asset_watch = engine_platform::fs_watch::DirectoryWatcher::new(&self.root, true).ok();
        for (path, why) in self.asset_db.problems() {
            log::warn!(target: "engine_editor", "asset {path}: {why}");
        }
        report
    }

    /// The folder's changes: a scan once it has been quiet for [`ASSETS_QUIET`], the new
    /// paths to the player and the inspector.
    fn tick_assets(&mut self) {
        if let Some(watch) = &self.asset_watch {
            let skip = [
                self.root.join(".ofelia"),
                self.root.join("target"),
                self.root.join(".git"),
            ];
            while let Some(event) = watch.try_next() {
                if !skip.iter().any(|s| event.path.starts_with(s)) {
                    self.assets_changed = Some(Instant::now());
                }
            }
        }
        if self
            .assets_changed
            .is_none_or(|at| at.elapsed() < ASSETS_QUIET)
        {
            return;
        }
        self.assets_changed = None;
        let report = self.asset_db.scan();
        if report.read == 0 && report.gone == 0 {
            return;
        }
        // Files written anew: what the player made from them is made again.
        for id in report.changed {
            if !self.changed_assets.contains(&id) {
                self.changed_assets.push(id);
            }
        }
        if let Err(e) = self.asset_db.save() {
            log::warn!(target: "engine_editor", "the asset index: {e}");
        }
        let paths = self.asset_db.paths();
        self.assets.extend(&paths);
        self.pending_assets.extend(&paths);
        for (path, why) in self.asset_db.problems() {
            log::warn!(target: "engine_editor", "asset {path}: {why}");
        }
        self.refresh_browser();
    }

    /// The browser's lists from the database: the project's folders, and the shown
    /// folder's subfolders and files (once a scan or a click changed them, not every frame).
    fn refresh_browser(&mut self) {
        let b = &mut self.browser;
        let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for e in self.asset_db.entries() {
            let mut at = e.path.as_str();
            while let Some((parent, _)) = at.rsplit_once('/') {
                folders.insert(parent.to_owned());
                at = parent;
            }
        }
        if !b.folder.is_empty() && !folders.contains(&b.folder) {
            b.folder.clear();
        }
        b.folders.clear();
        b.folders.push((String::new(), 0));
        b.folders.extend(
            folders
                .iter()
                .map(|f| (f.clone(), f.matches('/').count() as u16 + 1)),
        );
        b.listing.clear();
        let inside = |path: &str| match path.rsplit_once('/') {
            Some((parent, _)) => parent == b.folder,
            None => b.folder.is_empty(),
        };
        for f in &folders {
            if inside(f) {
                let name = f.rsplit('/').next().unwrap_or(f).to_owned();
                b.listing.push(Listed {
                    name,
                    kind: FileKind::Folder,
                    path: f.clone(),
                    asset: None,
                });
            }
        }
        let mut files: Vec<&engine_scene::AssetEntry> = self
            .asset_db
            .entries()
            .iter()
            .filter(|e| inside(&e.path))
            .collect();
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        for e in files {
            b.listing.push(Listed {
                name: e.path.rsplit('/').next().unwrap_or(&e.path).to_owned(),
                kind: file_kind(e.kind),
                path: e.path.clone(),
                asset: Some(e.id),
            });
        }
        b.label.clear();
        b.label.push_str("res://");
        b.label.push_str(&b.folder);
    }

    /// Shows folder `folder` in the browser (`""`: the project's top).
    fn open_folder(&mut self, folder: &str) {
        self.browser.folder.clear();
        self.browser.folder.push_str(folder);
        self.refresh_browser();
    }

    /// Picks asset `id` in the browser, its folder shown: what `#` in the palette does.
    fn reveal_asset(&mut self, id: AssetId) {
        let Some(e) = self.asset_db.get(id) else {
            return;
        };
        let (path, kind) = (e.path.clone(), e.kind);
        let folder = path.rsplit_once('/').map_or("", |(f, _)| f).to_owned();
        self.open_folder(&folder);
        self.browser.selected = Some(id);
        self.say(format!("{path} ({})", kind.name()));
    }

    /// A file dragged out of the browser (ADR 0107 E4-3): started by a file of the listing
    /// held and moved; over the view a model rests on the surface under the pointer, in an
    /// undo step; the release drops it where the pointer is, Escape takes it back. The
    /// keys are the drag's while it lasts.
    fn asset_drag_frame(&mut self, world: &mut World, response: &ShellResponse, game_view: bool) {
        if self.asset_drag.is_none()
            && self.draw.is_none()
            && self.key_transform.is_none()
            && let Some(l) = response.file_drag.and_then(|i| self.browser.listing.get(i))
            && let Some(id) = l.asset
            && let Some(entry) = self.asset_db.get(id)
        {
            self.asset_drag = Some(AssetDrag {
                asset: id,
                kind: entry.kind,
                path: entry.path.clone(),
                name: l.name.clone(),
                plan: None,
                placed: None,
                selection: self.selection.clone(),
                over: DropTarget::Nowhere,
            });
            self.browser.selected = Some(id);
            // The surface under the pointer is asked for anew.
            self.surfaced = None;
        }
        let Some(mut drag) = self.asset_drag.take() else {
            return;
        };
        let escape = self.ui.input().keys.contains(&UiKey::Escape);
        self.input.consume_keys();
        let released = self.input.released(Button::Left);
        let scale = self.scale.max(0.5);
        let cursor = self.input.cursor().map(|[x, y]| Vec2::new(x, y));
        let over_view = !game_view
            && cursor.is_some_and(|at| {
                self.ui.rect_of(response.viewport).is_some_and(|r| {
                    let (x, y) = (at.x / scale, at.y / scale);
                    x >= r.pos[0]
                        && y >= r.pos[1]
                        && x < r.pos[0] + r.size[0]
                        && y < r.pos[1] + r.size[1]
                })
            });
        drag.over = match response.tree_under.and_then(|i| self.rows.get(i)) {
            Some(row) => DropTarget::Row(row.entity),
            None if response.tree_hovered => DropTarget::Tree,
            None => match response.hovered_field {
                Some((section, field)) => DropTarget::Field(section, field),
                None if over_view => DropTarget::View,
                None => DropTarget::Nowhere,
            },
        };
        // The button let go outside the window: no drop.
        if escape || !(released || self.input.held(Button::Left)) {
            self.take_back_placed(world, &mut drag);
            return;
        }
        match (drag.kind, drag.over, cursor) {
            (engine_scene::AssetKind::Model, DropTarget::View, Some(at)) => {
                self.place_dragged(world, &mut drag, at);
            }
            _ => self.take_back_placed(world, &mut drag),
        }
        if released {
            self.drop_asset(world, drag);
            return;
        }
        self.hint = self.drag_hint(world, &drag);
        self.asset_drag = Some(drag);
    }

    /// The dragged model where the pointer is in the view: placed the first time, then
    /// moved, resting on the surface under the pointer (asked for every frame, found a few
    /// frames on), or where nothing is, on the ground's plane or ten meters ahead. Its
    /// place along the surface snaps to the grid with snapping (Shift inverts it).
    fn place_dragged(&mut self, world: &mut World, drag: &mut AssetDrag, at: Vec2) {
        let Some(rect) = self.view_rect else {
            return;
        };
        let Ok(plan) = self.model_plan(drag) else {
            return;
        };
        let view = ViewProj::new(&self.orbit, rect);
        let local = (at - Vec2::new(rect[0] as f32, rect[1] as f32)).max(Vec2::ZERO);
        self.surface_request = Some(SurfaceRequest {
            x: (local.x * self.view_scale[0]) as u32,
            y: (local.y * self.view_scale[1]) as u32,
            transparent: self.pick_transparent,
        });
        let (point, normal) = match self.surfaced.and_then(|r| r.hit) {
            Some(hit) => (hit.point, hit.normal),
            None => {
                let ray = view.ray(at);
                let ground = hit_plane(ray, Vec3::ZERO, Vec3::Y)
                    .filter(|p| p.distance(ray.0) < 200.0)
                    .unwrap_or(ray.0 + ray.1 * 10.0);
                (ground, Vec3::Y)
            }
        };
        let point = if self.snapping.on == self.input.shift() {
            point
        } else {
            snap_along(point, normal, self.snapping.position as f32)
        };
        let to = resting_point(point, normal, plan.bounds);
        match drag.placed.filter(|p| world.contains(p.root)) {
            Some(p) => {
                if world
                    .get::<Transform>(p.root)
                    .is_some_and(|t| t.translation != to)
                {
                    self.history
                        .begin(&format!("Place {}", drag.name), Some(p.merge));
                    self.history.touch(world, p.root);
                    if let Some(t) = world.get_mut::<Transform>(p.root) {
                        t.translation = to;
                    }
                    self.history.commit(world);
                }
            }
            None => {
                self.gestures += 1;
                let merge = self.gestures;
                self.history
                    .begin(&format!("Place {}", drag.name), Some(merge));
                let root = self.spawn_plan(world, &plan, None, to);
                self.history.commit(world);
                // Selected: surface picks leave it out, so it never lands on itself.
                self.select_only(root);
                drag.placed = Some(Placed { merge, root });
            }
        }
    }

    /// The dragged model placed in the view taken back, the selection as it was.
    fn take_back_placed(&mut self, world: &mut World, drag: &mut AssetDrag) {
        let Some(placed) = drag.placed.take() else {
            return;
        };
        self.history.revert(world, placed.merge);
        let before = self.active();
        self.selection = drag
            .selection
            .iter()
            .copied()
            .filter(|&e| world.contains(e))
            .collect();
        self.anchor = self.selection.last().copied();
        self.selection_changed(before);
    }

    /// What a preview of `subject` shows (ADR 0112), none for other kinds or a model that
    /// cannot be read:
    /// - a model's meshes with their materials, where its file places them;
    /// - one mesh with its own material (the default one for a built-in mesh);
    /// - a material on the built-in sphere.
    fn preview_parts(&self, subject: Subject) -> Option<Vec<(AssetRef, AssetRef, Affine3A)>> {
        use engine_scene::AssetKind as K;
        let one = |mesh, material| Some(vec![(mesh, material, Affine3A::IDENTITY)]);
        let id = match subject {
            Subject::Material(material) => return one(builtin::SPHERE, material),
            Subject::Mesh(mesh) if mesh.file == builtin::CUBE.file => {
                return one(mesh, builtin::MATERIAL);
            }
            Subject::Mesh(mesh) => {
                // The mesh's own material, from where its file uses it.
                let entry = self.asset_db.get(mesh.file)?;
                let outline =
                    engine_assets::GltfOutline::read(&self.root.join(&entry.path)).ok()?;
                let plan = ModelPlan::new(mesh.file, &entry.path, "preview", &outline).ok()?;
                let material = plan
                    .nodes
                    .iter()
                    .filter_map(|n| n.model)
                    .find(|m| m.mesh == mesh)
                    .map_or(builtin::MATERIAL, |m| m.material);
                return one(mesh, material);
            }
            Subject::File(id) => id,
        };
        let entry = self.asset_db.get(id)?;
        match entry.kind {
            K::Material => one(builtin::SPHERE, AssetRef::file(id)),
            K::Model => {
                let outline =
                    engine_assets::GltfOutline::read(&self.root.join(&entry.path)).ok()?;
                let plan = ModelPlan::new(id, &entry.path, "preview", &outline).ok()?;
                // Each node where the file places it under the root.
                let mut placed: Vec<Affine3A> = Vec::with_capacity(plan.nodes.len());
                let mut parts = Vec::new();
                for node in &plan.nodes {
                    let t = node.transform;
                    let local = Affine3A::from_scale_rotation_translation(
                        t.scale,
                        t.rotation,
                        t.translation,
                    );
                    let at = match node.parent {
                        Some(p) => placed[p] * local,
                        None => Affine3A::from_scale_rotation_translation(
                            t.scale,
                            t.rotation,
                            Vec3::ZERO,
                        ),
                    };
                    placed.push(at);
                    if let Some(model) = node.model {
                        parts.push((model.mesh, model.material, at));
                    }
                }
                // The player finds the parts by the file's path, which it has.
                (!parts.is_empty()).then_some(parts)
            }
            _ => None,
        }
    }

    /// The release: the file dropped where the pointer is.
    fn drop_asset(&mut self, world: &mut World, mut drag: AssetDrag) {
        use engine_scene::AssetKind as K;
        let name = drag.name.clone();
        match (drag.over, drag.kind) {
            (DropTarget::View, K::Model) => match (drag.placed, self.model_plan(&mut drag)) {
                (Some(placed), Ok(plan)) => {
                    self.show(world, placed.root);
                    self.say(placed_message(&name, &plan));
                }
                (_, Err(why)) => self.say(why),
                (None, Ok(_)) => {}
            },
            (DropTarget::View, K::Material) => match self.hovered.and_then(|(_, e)| e) {
                Some(e) => self.give_material(world, e, &drag),
                None => self.say(format!("Drop {name} onto an object")),
            },
            (DropTarget::Row(e), K::Model) => self.add_model(world, &mut drag, Some(e)),
            (DropTarget::Tree, K::Model) => self.add_model(world, &mut drag, None),
            (DropTarget::Row(e), K::Material) => self.give_material(world, e, &drag),
            (DropTarget::Field(section, field), _) => {
                self.drop_on_field(world, section, field, &mut drag);
            }
            (DropTarget::View | DropTarget::Row(_) | DropTarget::Tree, _) => self.say(format!(
                "{name}: models and materials go into the scene; other assets onto fields"
            )),
            (DropTarget::Nowhere, _) => {}
        }
    }

    /// A dragged model's entities, read from its file the first time.
    fn model_plan(&self, drag: &mut AssetDrag) -> Result<Rc<ModelPlan>, String> {
        if drag.plan.is_none() {
            let stem = Path::new(&drag.name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&drag.name);
            let plan = engine_assets::GltfOutline::read(&self.root.join(&drag.path))
                .map_err(|e| format!("{}: {e}", drag.name))
                .and_then(|outline| ModelPlan::new(drag.asset, &drag.path, stem, &outline))
                .map(Rc::new);
            drag.plan = Some(plan);
        }
        drag.plan
            .clone()
            .unwrap_or_else(|| Err(String::from("no plan")))
    }

    /// The model `plan` makes, in the open history step: its root under `parent` (a new
    /// root after the others for none) at `at` in its parent's space. The parts' paths go
    /// to the player and the inspector. Returns the root.
    fn spawn_plan(
        &mut self,
        world: &mut World,
        plan: &ModelPlan,
        parent: Option<Entity>,
        at: Vec3,
    ) -> Entity {
        let mut made: Vec<Entity> = Vec::with_capacity(plan.nodes.len());
        for node in &plan.nodes {
            let mut transform = node.transform;
            let (name, above) = match node.parent {
                Some(i) => (node.name.clone(), Some(made[i])),
                None => {
                    transform.translation = at;
                    (self.sibling_name(world, parent, &node.name, None), parent)
                }
            };
            let order = above.is_none().then(|| self.last_root_order(world));
            let e = world.spawn((
                SceneId::new(),
                Name::new(&name),
                transform,
                GlobalTransform::default(),
            ));
            if let Some(model) = node.model {
                world.insert(e, model);
            }
            match (above, order) {
                (Some(p), _) => {
                    let _ = world.set_parent(e, Some(p));
                }
                (None, Some(order)) => {
                    world.insert(e, RootOrder(order));
                }
                (None, None) => {}
            }
            self.history.touch_new(world, e);
            made.push(e);
        }
        if let Some(p) = parent {
            self.collapsed.remove(&p);
        }
        let mut paths = AssetPaths::new();
        for (asset, path) in &plan.paths {
            paths.insert(*asset, path.clone());
        }
        self.assets.extend(&paths);
        self.pending_assets.extend(&paths);
        made[0]
    }

    /// A dragged model dropped onto the tree: under entity `parent` at its origin, or as a
    /// root where the camera looks (on the grid), as one undo step, selected.
    fn add_model(&mut self, world: &mut World, drag: &mut AssetDrag, parent: Option<Entity>) {
        let plan = match self.model_plan(drag) {
            Ok(plan) => plan,
            Err(why) => {
                self.say(why);
                return;
            }
        };
        let at = match parent {
            Some(_) => Vec3::ZERO,
            None => {
                let step = self.snapping.position as f32;
                (self.orbit.target / step).round() * step
            }
        };
        self.history.begin(&format!("Add {}", drag.name), None);
        if let Some(p) = parent {
            self.history.touch(world, p);
        }
        let root = self.spawn_plan(world, &plan, parent, at);
        self.history.commit(world);
        self.show(world, root);
        self.say(placed_message(&drag.name, &plan));
    }

    /// A dragged material file given to entity `e`'s model, as one undo step.
    fn give_material(&mut self, world: &mut World, e: Entity, drag: &AssetDrag) {
        let target = world
            .get::<Name>(e)
            .map_or_else(|| String::from("The entity"), |n| n.as_str().to_owned());
        if world.get::<Model>(e).is_none() {
            self.say(format!("{target} has no model to take {}", drag.name));
            return;
        }
        self.history
            .begin(&format!("Give {target} {}", drag.name), None);
        self.history.touch(world, e);
        if let Some(model) = world.get_mut::<Model>(e) {
            model.material = AssetRef::file(drag.asset);
        }
        self.history.commit(world);
        self.say(format!("{} → {target}", drag.name));
    }

    /// A dragged file dropped onto inspector field `field` of section `section`: an asset
    /// field takes it, on every selected entity with the field's component, as one undo
    /// step. A model's mesh field takes a model file of one mesh; its material field a
    /// material file, or a model file of one material.
    fn drop_on_field(
        &mut self,
        world: &mut World,
        section: usize,
        field: usize,
        drag: &mut AssetDrag,
    ) {
        use engine_scene::AssetKind as K;
        let Some((p, component)) = self.shown_property(section, field) else {
            return;
        };
        let path = self.path_of(&p, component);
        if !engine_scene::text::is_asset_ref(p.ty) {
            self.say(format!("{path} takes no asset"));
            return;
        }
        let file = drag.name.clone();
        let one = |parts: &[String], what: &str| -> Result<String, String> {
            match parts {
                [one] => Ok(one.clone()),
                _ => Err(format!(
                    "{file} has {} {what}s: drop it into the view or onto the tree",
                    parts.len()
                )),
            }
        };
        let asset = match (path.as_str(), drag.kind) {
            ("Model.mesh", K::Model) | ("Model.material", K::Model) => {
                let mesh = path == "Model.mesh";
                let part = self.model_plan(drag).and_then(|plan| {
                    if mesh {
                        one(&plan.meshes, "mesh")
                    } else {
                        one(&plan.materials, "material")
                    }
                });
                match part {
                    Ok(part) => {
                        let asset = AssetRef::part(drag.asset, &part);
                        let mut paths = AssetPaths::new();
                        paths.insert(asset, format!("{}#{part}", drag.path));
                        self.assets.extend(&paths);
                        self.pending_assets.extend(&paths);
                        asset
                    }
                    Err(why) => {
                        self.say(why);
                        return;
                    }
                }
            }
            ("Model.mesh", _) => {
                self.say(format!("{path} takes a model's mesh, not {}", drag.name));
                return;
            }
            ("Model.material", K::Material) => AssetRef::file(drag.asset),
            ("Model.material", _) => {
                self.say(format!("{path} takes a material, not {}", drag.name));
                return;
            }
            _ => AssetRef::file(drag.asset),
        };
        let mut value = String::new();
        asset.write(Some(&self.assets), &mut value);
        let targets = self.targets(world, p.component);
        match self.set_on(world, &targets, &path, &value) {
            Ok(()) => self.say(format!("{} → {path}", drag.name)),
            Err(reason) => self.say(format!("{path}: {reason}")),
        }
    }

    /// Inspector field `field` of section `section` given asset `id` (picked in the
    /// palette), as a file dropped on it would be.
    fn give_field_asset(&mut self, world: &mut World, section: usize, field: usize, id: AssetId) {
        let Some(entry) = self.asset_db.get(id) else {
            return;
        };
        let mut drag = AssetDrag {
            asset: id,
            kind: entry.kind,
            path: entry.path.clone(),
            name: entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_owned(),
            plan: None,
            placed: None,
            selection: Vec::new(),
            over: DropTarget::Field(section, field),
        };
        self.drop_on_field(world, section, field, &mut drag);
    }

    /// What a release would do now, by the pointer.
    fn drag_hint(&self, world: &World, drag: &AssetDrag) -> String {
        use engine_scene::AssetKind as K;
        let name = &drag.name;
        let named = |e: Entity| world.get::<Name>(e).map_or("the entity", Name::as_str);
        match (drag.over, drag.kind) {
            (DropTarget::View, K::Model) => match &drag.plan {
                Some(Err(why)) => why.clone(),
                _ => format!("{name}: let go to place it  ·  Esc takes it back"),
            },
            (DropTarget::View, K::Material) => match self.hovered.and_then(|(_, e)| e) {
                Some(e) if world.get::<Model>(e).is_some() => format!("{name} → {}", named(e)),
                Some(e) => format!("{} has no model to take {name}", named(e)),
                None => format!("{name}: onto an object"),
            },
            (DropTarget::Row(e), K::Model) => {
                format!("{name}: let go to add it under {}", named(e))
            }
            (DropTarget::Row(e), K::Material) => format!("{name} → {}", named(e)),
            (DropTarget::Tree, K::Model) => format!("{name}: let go to add it to the scene"),
            (DropTarget::Field(section, field), _) => match self.shown_property(section, field) {
                Some((p, component)) => format!("{name} → {}", self.path_of(&p, component)),
                None => name.clone(),
            },
            _ => name.clone(),
        }
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
        if let Some(preset) = state.keys.as_deref().and_then(KeyPreset::from_name) {
            self.use_keys(preset);
        }
        if let Some(fly) = state.fly {
            let (low, high) = FLY_RANGE;
            self.fly_speed = (fly as f32).clamp(low, high);
        }
        if let Some(on) = state.pick_transparent {
            self.pick_transparent = on;
        }
        if let Some(pivot) = state.pivot.as_deref().and_then(Pivot::from_name) {
            self.pivot = pivot;
        }
        if let Some(on) = state.overlays {
            self.overlays = on;
        }
        self.refresh_steps();
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
        self.bookmarks = [None; 9];
        for &(n, [x, y, z], yaw, pitch, distance) in &view.bookmarks {
            if let Some(b) = self.bookmarks.get_mut(usize::from(n).wrapping_sub(1)) {
                *b = Some(OrbitCamera {
                    target: Vec3::new(x, y, z),
                    yaw,
                    pitch,
                    distance,
                    ..self.orbit
                });
            }
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
            bookmarks: self
                .bookmarks
                .iter()
                .enumerate()
                .filter_map(|(n, b)| {
                    b.map(|c| (n as u8 + 1, c.target.to_array(), c.yaw, c.pitch, c.distance))
                })
                .collect(),
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
            keys: Some(String::from(self.key_preset.name())),
            fly: Some(f64::from(self.fly_speed)),
            pick_transparent: Some(self.pick_transparent),
            pivot: Some(String::from(self.pivot.name())),
            overlays: Some(self.overlays),
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
            || self.gizmo_drag.is_some()
            || self.side_drag.is_some()
            || self.face_edit.as_ref().is_some_and(FaceEdit::dragging)
            || self.key_transform.is_some()
            || self.draw.is_some()
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
            && self.play.is_none()
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

    /// "Editor: Key Map…": Blender's keys or Godot's, in the palette.
    fn pick_keys(&mut self, world: &mut World) {
        let items = KeyPreset::ALL
            .into_iter()
            .map(|p| Choice {
                label: String::from(p.label()),
                detail: String::from(if p == self.key_preset { "now" } else { "" }),
                pick: Pick::Keys(p),
            })
            .collect();
        self.open_list(world, "Which keys the editor uses", items);
    }

    fn set_key_preset(&mut self, world: &mut World, preset: KeyPreset) {
        self.use_keys(preset);
        self.write_session(world);
        self.say(String::from(preset.label()));
    }

    /// Takes the shortcuts of key map `preset`.
    fn use_keys(&mut self, preset: KeyPreset) {
        self.key_preset = preset;
        (self.keymap, self.shortcuts) = keymap(&self.ids, preset);
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
    start: Start<G>,
    /// Why the game stopped by itself (a hook panicked): Play ends at the next frame.
    failed: Option<String>,
    /// A step runs while paused: the game's hooks are called for it.
    stepping: bool,
}

impl<G> EditorGame<G> {
    /// The viewport panel in pixels, for the player's next frame
    /// (`Player::set_view_rect`).
    pub fn view_rect(&self) -> Option<[u32; 4]> {
        self.shared.borrow().view_rect
    }

    /// Play's transitions once the frame is drawn (ADR 0107 E3): Play (the world kept,
    /// the game made by its `start` on it, the world's time running) or Stop (the game
    /// dropped, what it left in the player ended, the world put back).
    fn play_frame(&mut self, player: &mut Player) {
        // The game's window: closed by its button, Stop; ejected or not, the view there
        // or in the viewport.
        if player.view_window_close_requested() {
            self.shared.borrow_mut().play_request = Some(PlayRequest::Stop);
        }
        if player.has_view_window() {
            let in_window = self.shared.borrow().game_view();
            player.show_view_in_window(in_window);
        }
        let request = match self.failed.take() {
            Some(why) => Some((PlayRequest::Stop, Some(why))),
            None => self
                .shared
                .borrow_mut()
                .play_request
                .take()
                .map(|r| (r, None)),
        };
        // A step's tick has run: paused again.
        if self.stepping && player.steps_pending() == 0 {
            self.stepping = false;
        }
        match request {
            Some((PlayRequest::Play { from_here }, _)) => self.begin_play(player, from_here),
            Some((PlayRequest::Stop, why)) => self.end_play(player, why),
            Some((PlayRequest::Restart, _)) => {
                let from_here = self
                    .shared
                    .borrow()
                    .play
                    .as_ref()
                    .is_some_and(|p| p.from_here);
                self.end_play(player, None);
                self.begin_play(player, from_here);
            }
            Some((PlayRequest::Pause, _)) => {
                let mut shared = self.shared.borrow_mut();
                if let Some(p) = shared.play.as_mut() {
                    p.paused = !p.paused;
                    player.set_simulating(!p.paused);
                    self.stepping = false;
                    let message = if p.paused { "Paused" } else { "Going on" };
                    shared.say(String::from(message));
                }
            }
            Some((PlayRequest::Step, _)) => {
                let mut shared = self.shared.borrow_mut();
                if let Some(p) = shared.play.as_mut() {
                    p.paused = true;
                    player.set_simulating(false);
                    player.step_tick();
                    self.stepping = true;
                }
            }
            None => {}
        }
    }

    /// Whether the game's hooks are called: playing and not paused, or a step running.
    fn running(&self) -> bool {
        self.stepping
            || self
                .shared
                .borrow()
                .play
                .as_ref()
                .is_some_and(|p| !p.paused)
    }

    fn begin_play(&mut self, player: &mut Player, from_here: bool) {
        if self.game.is_some() || self.shared.borrow().play.is_some() {
            return;
        }
        let asked = Instant::now();
        {
            let mut shared = self.shared.borrow_mut();
            // The scene as it is goes to the session first, as before a restart.
            shared.save_session(player.world_mut());
            let playing = shared.prepare_play(player.world_mut(), from_here);
            shared.play = Some(playing);
            if from_here {
                shared.play_from_here(player.world_mut());
            }
        }
        // The game's own window (the UX note's default), as large as the viewport.
        {
            let mut shared = self.shared.borrow_mut();
            if shared.play_in_window {
                let scale = shared.scale.max(0.5);
                let [w, h] = shared.view_rect.map_or([1280.0, 720.0], |[_, _, w, h]| {
                    [w as f32 / scale, h as f32 / scale]
                });
                let title = format!("{} (playing)", shared.file_name());
                match player.open_view_window(&title, (w as u32).max(640), (h as u32).max(360)) {
                    Ok(()) => {
                        shared.in_view_window = true;
                        shared.view_window = player.view_window_id();
                    }
                    Err(e) => shared.say(format!("Playing in the viewport: {e}")),
                }
            }
        }
        player.set_simulating(true);
        let start = self.start;
        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(player)));
        let failed = match made {
            Ok(Ok(game)) => {
                self.game = Some(game);
                None
            }
            Ok(Err(e)) => Some(format!("the game's start failed: {e}")),
            Err(payload) => Some(format!(
                "the game's start panicked: {}",
                panic_note(&*payload)
            )),
        };
        if let Some(why) = failed {
            self.end_play(player, Some(why));
            return;
        }
        let mut shared = self.shared.borrow_mut();
        if let Some(p) = shared.play.as_mut() {
            p.entered = asked.elapsed();
            let ms = p.entered.as_secs_f64() * 1e3;
            shared.say(format!("Play started in {ms:.1} ms"));
        }
    }

    fn end_play(&mut self, player: &mut Player, why: Option<String>) {
        self.game = None;
        self.failed = None;
        self.stepping = false;
        player.close_view_window();
        {
            let mut shared = self.shared.borrow_mut();
            shared.in_view_window = false;
            shared.view_window = None;
        }
        player.end_run();
        player.set_simulating(false);
        let mut shared = self.shared.borrow_mut();
        if let Some(playing) = shared.play.take() {
            let message = shared.finish_play(player.world_mut(), playing, why);
            shared.say(message);
        }
    }

    /// Ends Play, if the scene is playing: before the editor exits or restarts, so the
    /// session keeps the scene, not the play world.
    pub fn stop_playing(&mut self, player: &mut Player) {
        if self.shared.borrow().play.is_some() {
            self.end_play(player, None);
        }
    }

    /// Whether the scene is playing (ADR 0107 E3).
    pub fn playing(&self) -> bool {
        self.shared.borrow().play.is_some()
    }

    /// The project's asset file at `path` (under its folder, with `/`), as the asset database
    /// knows it (ADR 0108, E4-1).
    pub fn asset(&self, path: &str) -> Option<engine_scene::AssetEntry> {
        self.shared.borrow().asset_db.find(path).cloned()
    }

    /// What the status bar last said (a command's answer), for tests and tools.
    pub fn message(&self) -> Option<String> {
        self.shared
            .borrow()
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
    }

    /// The game's hook `hook`, run on the game while playing, a panic in it caught: Play
    /// then ends at the next frame and says where it happened.
    fn guarded<R>(&mut self, hook: &str, f: impl FnOnce(&mut G) -> R) -> Option<R> {
        if self.failed.is_some() {
            return None;
        }
        let game = self.game.as_mut()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(game))) {
            Ok(r) => Some(r),
            Err(payload) => {
                self.failed = Some(format!(
                    "the game panicked in `{hook}`: {}",
                    panic_note(&*payload)
                ));
                None
            }
        }
    }

    /// Whether the cursor is locked for the player's next frame (`Player::lock_cursor`):
    /// while the right or middle button drags the camera, as in Godot and Unreal, the
    /// cursor is hidden and stays where the drag began.
    pub fn cursor_locked(&self) -> bool {
        self.shared.borrow().camera_drag
    }

    /// Gives the player the asset paths of scenes the editor loaded, and learns the
    /// player's again when they changed (a scene loaded since): the inspector shows
    /// references by path.
    pub fn sync_assets(&mut self, player: &mut Player) {
        self.play_frame(player);
        let mut shared = self.shared.borrow_mut();
        if !shared.pending_assets.is_empty() {
            player.assets_mut().add_paths(&shared.pending_assets);
            shared.pending_assets = AssetPaths::new();
        }
        let paths = player.assets().paths();
        if shared.assets.len() != paths.len() {
            shared.assets = paths.clone();
        }
        // Thumbnails of models and materials (ADR 0112): drawn by the player, taken back.
        for (subject, _) in shared.thumbnails.take_to_draw() {
            let parts = shared.preview_parts(subject);
            let key = thumbnails::preview_key(subject);
            match parts {
                Some(parts) => player.request_preview(engine_app::PreviewRequest { key, parts }),
                None => shared.thumbnails.drawn(key, Vec::new()),
            }
        }
        let mut previews = Vec::new();
        player.take_previews(&mut previews);
        for preview in previews {
            shared.thumbnails.drawn(preview.key, preview.pixels);
        }
        // Files written anew (E4-5): made again, the old versions drawn until then.
        for id in core::mem::take(&mut shared.changed_assets) {
            if let Some(entry) = shared.asset_db.get(id) {
                let path = entry.path.clone();
                player.asset_changed(id, &path);
            }
        }
        // Groups baked (E2b-6): their files written and read, their models in place.
        if !shared.bake_groups.is_empty() {
            let groups = core::mem::take(&mut shared.bake_groups);
            let message = shared.bake(player, &groups);
            shared.say(message);
        }
        // The newest pick's entity, and what to outline (ADR 0109), through the
        // extraction's slots.
        let runtime = player.runtime();
        // A group exported (E2b-6): its surface as the player draws it, beside the scene.
        if let Some(group) = shared.export_group.take() {
            let message = shared.export(player.blockout(), &runtime.world, group);
            shared.say(message);
        }
        // The navigation's overlay (E2b-4b): baked in the background a moment after the
        // brushes last changed.
        if shared.show_nav {
            shared.navigation_frame(player.blockout(), &runtime.world);
        } else {
            shared.nav_edges.clear();
            shared.nav_key.clear();
        }
        // The colliders' overlay (ADR 0110 E2b-4): the edges again when a group changed.
        if shared.show_colliders {
            let world = &runtime.world;
            let key = blockout_key(player.blockout(), world);
            if key != shared.collider_key {
                let mut edges = core::mem::take(&mut shared.collider_edges);
                player.blockout().collider_edges(world, &mut edges);
                shared.collider_edges = edges;
                shared.collider_key = key;
            }
        } else {
            shared.collider_edges.clear();
        }
        let extract = runtime.extract();
        let slot_of = |e: Entity| extract.instance(e).map(|i| i.slot());
        shared.resolve_pick(&runtime.world, slot_of);
        shared.resolve_list(&runtime.world, slot_of);
        shared.outline(&runtime.world, slot_of);
        // End: the instances of the entity being dropped, with its children's.
        if let Some(one) = shared.dropping.as_mut().and_then(|d| d.current.as_mut())
            && one.slots.is_none()
        {
            let world = &runtime.world;
            let mut slots = Vec::new();
            let mut stack = vec![one.entity];
            while let Some(e) = stack.pop() {
                slots.extend(slot_of(e));
                stack.extend(world.children(e));
            }
            one.slots = Some(slots);
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

    /// The viewport's icons this frame (ADR 0107 E2-7): each light's and camera's entity and
    /// its place on the screen (physical pixels).
    pub fn view_icons(&self) -> Vec<(Entity, [f32; 2])> {
        let shared = self.shared.borrow();
        shared
            .view_icons
            .iter()
            .map(|i| (i.entity, i.at.to_array()))
            .collect()
    }

    /// Where world point `p` shows in the viewport through the editor's camera (physical
    /// pixels); none behind it or before the viewport is laid out. For tests and tools
    /// that point at the scene.
    pub fn on_screen(&self, p: Vec3) -> Option<[f32; 2]> {
        let shared = self.shared.borrow();
        let view = ViewProj::new(&shared.orbit, shared.view_rect?);
        view.screen(p).map(|p| p.to_array())
    }

    /// The middle of where the last frame drew the text `s` (physical pixels), the first
    /// time it drew it: for tests and tools that point at the editor's panels.
    pub fn text_at(&self, s: &str) -> Option<[f32; 2]> {
        let shared = self.shared.borrow();
        let scale = shared.scale.max(0.5);
        let frame = shared.ui.last_frame();
        frame.commands.iter().find_map(|c| match c {
            engine_ui::DrawCmd::Text { rect, range, .. } if frame.text[range.clone()] == *s => {
                Some([
                    (rect.pos[0] + rect.size[0] / 2.0) * scale,
                    (rect.pos[1] + rect.size[1] / 2.0) * scale,
                ])
            }
            _ => None,
        })
    }

    /// Whether the asset at `path` (under the project) has its thumbnail made (ADR 0112):
    /// for tests.
    pub fn thumbnail_ready(&self, path: &str) -> bool {
        let shared = self.shared.borrow();
        shared
            .asset_db
            .find(path)
            .is_some_and(|e| shared.thumbnails.made(Subject::File(e.id)))
    }

    /// The navigation mesh's edges the overlay shows now (View: Navigation): for tests.
    pub fn navigation_edges(&self) -> Vec<(Vec3, Vec3, bool)> {
        self.shared.borrow().nav_edges.clone()
    }

    /// The entity under the pointer in the viewport, as the newest pick saw it (ADR 0109).
    pub fn hovered(&self) -> Option<Entity> {
        self.shared.borrow().hovered.and_then(|(_, e)| e)
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

    /// Adds an entity of the add menu's `index`-th kind (`Point`, `Cube`, `Sphere`, `Plane`,
    /// `Cylinder`, `Sun`, `Point Light`, `Spot Light`, `Camera`, then the brushes: box,
    /// wedge, stairs, cylinder, cone, truncated cone, sphere, torus), as the menu does.
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
        // The game sees the events of the window it plays in.
        let theirs = {
            let shared = self.shared.borrow();
            match shared.view_window {
                Some(view) => event.window == view,
                None => true,
            }
        };
        if theirs && self.running() {
            self.guarded("event", |game| game.event(event));
        }
    }

    fn tick(&mut self, world: &mut World, dt: f32) {
        self.guarded("tick", |game| game.tick(world, dt));
    }

    fn before_save(&mut self, world: &World, image: &mut engine_save::SaveImage) {
        self.guarded("before_save", |game| game.before_save(world, image));
    }

    fn before_load(&mut self, world: &mut World, save: &engine_save::LoadedSave) {
        self.guarded("before_load", |game| game.before_load(world, save));
    }

    fn after_load(
        &mut self,
        world: &mut World,
        save: &engine_save::LoadedSave,
        report: &engine_save::LoadReport,
    ) {
        self.guarded("after_load", |game| game.after_load(world, save, report));
    }

    fn can_autosave(&self, world: &World) -> bool {
        // Only while playing: a game's autosave is its own (never the scene's).
        self.game.as_ref().is_some_and(|game| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| game.can_autosave(world)))
                .unwrap_or(false)
        })
    }

    fn net_input(&mut self, world: &mut World) {
        if self.running() {
            self.guarded("net_input", |game| game.net_input(world));
        }
    }

    fn acoustics(&self) -> Option<&dyn engine_physics::PhysicsBackend> {
        self.game.as_ref().and_then(Game::acoustics)
    }

    fn present(&mut self, world: &mut World, alpha: f32) {
        if self.running() {
            self.guarded("present", |game| game.present(world, alpha));
        }
        self.shared.borrow_mut().update(world);
    }

    fn animate(&mut self, parts: &mut engine_app::FrameParts<'_>, dt: f32) {
        if self.running() {
            self.guarded("animate", |game| game.animate(parts, dt));
        }
    }

    fn ui(&self, draw: &mut dyn FnMut(&[engine_frame::UiCanvas<'_>])) {
        if let Some(game) = &self.game {
            game.ui(draw);
        }
    }
}

/// The viewport's rectangle as a clip.
fn clip_of(rect: [u32; 4]) -> ClipRect {
    ClipRect {
        x: rect[0] as i32,
        y: rect[1] as i32,
        width: rect[2],
        height: rect[3],
    }
}

/// Draws the gizmo below the UI (layer −1), clipped to the viewport: the handle `lit`
/// (under the pointer, or dragged) in its own color.
fn draw_gizmo(gizmo: &Gizmo, lit: Option<Handle>, rect: [u32; 4], scale: f32, canvas: &mut Canvas) {
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    let color = |h: Handle| {
        if Some(h) == lit {
            COLORS[4]
        } else {
            h.axis().map_or(COLORS[3], |i| COLORS[i])
        }
    };
    for &(h, a, b) in &gizmo.lines {
        canvas.draw(&Quad::line(a, b, 2.0 * scale).color(color(h)).order(order));
    }
    for &(h, tip, direction) in &gizmo.tips {
        let base = tip - direction * 14.0 * scale;
        canvas.draw(
            &Quad::line(base, tip, 7.0 * scale)
                .color(color(h))
                .order(order),
        );
    }
    for &(h, corners) in &gizmo.quads {
        let mut c = color(h);
        // A square lit is filled; the others only outlined, faint.
        if Some(h) != lit {
            c.w = 0.75;
        }
        for i in 0..4 {
            canvas.draw(
                &Quad::line(corners[i], corners[(i + 1) % 4], 1.5 * scale)
                    .color(c)
                    .order(order),
            );
        }
    }
    for &(h, center, half) in &gizmo.boxes {
        canvas.draw(
            &Quad::solid(center, Vec2::splat(half * 2.0))
                .color(color(h))
                .order(order),
        );
    }
    canvas.set_clip(None);
}

/// Draws a keyboard transform's guides below the UI, clipped to the viewport: an axis'
/// line in its color, the pivot's line to the pointer dashed.
/// The viewport's overlays (ADR 0107 E2-7), under the panels: the selected lights' and
/// cameras' reach as thin lines, then the icons on dark disks, the selected ones' orange
/// (the active one lighter), the one under the pointer white.
fn draw_overlays(editor: &mut Editor, rect: [u32; 4], glyphs: &GlyphTexture, canvas: &mut Canvas) {
    let scale = editor.scale.max(0.5);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    for &(a, b, camera) in &editor.reach_lines {
        let color = if camera {
            Vec4::new(0.55, 0.75, 1.0, 0.8)
        } else {
            Vec4::new(1.0, 0.85, 0.45, 0.8)
        };
        canvas.draw(&Quad::line(a, b, scale).color(color).order(order));
    }
    let active = editor.active();
    for icon in &editor.view_icons {
        let color = if Some(icon.entity) == editor.icon_hover {
            Vec4::ONE
        } else if Some(icon.entity) == active {
            Vec4::new(1.0, 0.62, 0.18, 1.0)
        } else if editor.selection.contains(&icon.entity) {
            Vec4::new(1.0, 0.32, 0.02, 1.0)
        } else {
            Vec4::new(0.85, 0.85, 0.85, 1.0)
        };
        canvas.draw(
            &Quad::circle(icon.at, 11.0 * scale)
                .color(Vec4::new(0.08, 0.08, 0.1, 0.75))
                .order(order),
        );
        editor.text.draw_centered(
            canvas,
            glyphs,
            icon.glyph,
            15.0 * scale,
            icon.at,
            color,
            order,
        );
    }
    canvas.set_clip(None);
}

/// The window's color everywhere but the viewport `view` (`[x, y, width, height]` in the
/// output's pixels), under everything else: what shows in the gaps between docks.
fn draw_window(
    view: Option<[u32; 4]>,
    extent: engine_rhi::ash::vk::Extent2D,
    color: [f32; 4],
    canvas: &mut Canvas,
) {
    let (w, h) = (extent.width as f32, extent.height as f32);
    let around = match view {
        Some([x, y, vw, vh]) => {
            let [x, y] = [x as f32, y as f32];
            let [right, bottom] = [x + vw as f32, y + vh as f32];
            [
                [0.0, 0.0, w, y],
                [0.0, bottom, w, h],
                [0.0, y, x, bottom],
                [right, y, w, bottom],
            ]
        }
        None => [[0.0, 0.0, w, h], [0.0; 4], [0.0; 4], [0.0; 4]],
    };
    for [x0, y0, x1, y1] in around {
        if x1 > x0 && y1 > y0 {
            let (min, max) = (Vec2::new(x0, y0), Vec2::new(x1, y1));
            canvas.draw(
                &Quad::solid((min + max) * 0.5, max - min)
                    .color(Vec4::from_array(color))
                    .order(Order::layer(i16::MIN)),
            );
        }
    }
}

/// The box being dragged in the viewport (ADR 0109): a light frame over a faint fill,
/// dashed while it takes hidden objects too (Alt).
fn draw_box(
    (from, to): (Vec2, Vec2),
    hidden: bool,
    rect: [u32; 4],
    scale: f32,
    canvas: &mut Canvas,
) {
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    let (lo, hi) = (from.min(to), from.max(to));
    let (center, size) = ((lo + hi) * 0.5, hi - lo);
    let light = |alpha| Vec4::new(0.85, 0.85, 0.85, alpha);
    canvas.draw(&Quad::rect(center, size).color(light(0.08)).order(order));
    if hidden {
        let corners = [lo, Vec2::new(hi.x, lo.y), hi, Vec2::new(lo.x, hi.y)];
        let (dash, gap) = (6.0 * scale, 4.0 * scale);
        for k in 0..4 {
            let (a, b) = (corners[k], corners[(k + 1) % 4]);
            let length = a.distance(b);
            let along = (b - a) / length.max(1e-3);
            let mut s = 0.0;
            while s < length {
                let e = (s + dash).min(length);
                canvas.draw(
                    &Quad::line(a + along * s, a + along * e, scale)
                        .color(light(0.8))
                        .order(order),
                );
                s += dash + gap;
            }
        }
    } else {
        canvas.draw(
            &Quad::rect(center, size)
                .outline(scale)
                .color(light(0.8))
                .order(order),
        );
    }
    canvas.set_clip(None);
}

fn draw_guides(guides: &[(Vec2, Vec2, usize)], rect: [u32; 4], scale: f32, canvas: &mut Canvas) {
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    for &(a, b, i) in guides {
        let mut color = COLORS[i.min(3)];
        if i < 3 {
            canvas.draw(&Quad::line(a, b, 1.5 * scale).color(color).order(order));
            continue;
        }
        color.w = 0.7;
        dashed_line(canvas, (a, b), scale, color, order);
    }
    canvas.set_clip(None);
}

/// A dashed line `scale` pixels wide.
fn dashed_line(canvas: &mut Canvas, (a, b): (Vec2, Vec2), scale: f32, color: Vec4, order: Order) {
    let length = a.distance(b);
    let along = (b - a) / length.max(1e-3);
    let (dash, gap) = (6.0 * scale, 5.0 * scale);
    let mut s = 0.0;
    while s < length {
        let e = (s + dash).min(length);
        canvas.draw(
            &Quad::line(a + along * s, a + along * e, scale)
                .color(color)
                .order(order),
        );
        s += dash + gap;
    }
}

/// The brush being drawn (ADR 0110 E2b-3), under the panels: its mark or its box's edges,
/// a brush's light and solid, a cut's orange and dashed (as cuts show in the UX note),
/// and its sizes on dark labels.
fn draw_drawing(editor: &mut Editor, rect: [u32; 4], glyphs: &GlyphTexture, canvas: &mut Canvas) {
    let scale = editor.scale.max(0.5);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    let cut = editor.draw.as_ref().is_some_and(|d| d.op == BrushOp::Cut);
    for &(a, b) in &editor.draw_lines {
        if cut {
            dashed_line(
                canvas,
                (a, b),
                1.5 * scale,
                Vec4::new(1.0, 0.55, 0.15, 0.95),
                order,
            );
        } else {
            let color = Vec4::new(0.85, 0.95, 1.0, 0.95);
            canvas.draw(&Quad::line(a, b, 1.5 * scale).color(color).order(order));
        }
    }
    for (at, text) in &editor.draw_labels {
        label(&mut editor.text, glyphs, canvas, *at, text, scale);
    }
    // Landed on another brush: a corner as a green dot, an edge as a red ring (SketchUp's
    // colors; the shapes tell them apart without color).
    if let Some((at, corner)) = editor.draw_landed {
        let dark = Vec4::new(0.08, 0.08, 0.1, 0.9);
        canvas.draw(&Quad::circle(at, 7.0 * scale).color(dark).order(order));
        if corner {
            let green = Vec4::new(0.3, 0.95, 0.4, 1.0);
            canvas.draw(&Quad::circle(at, 5.0 * scale).color(green).order(order));
        } else {
            let red = Vec4::new(1.0, 0.35, 0.3, 1.0);
            canvas.draw(&Quad::circle(at, 5.0 * scale).color(red).order(order));
            canvas.draw(&Quad::circle(at, 2.5 * scale).color(dark).order(order));
        }
    }
    canvas.set_clip(None);
}

/// The keys while editing faces, for the status bar.
const FACES_HELP: &str = "drag a face, an edge or a corner: move it  ·  Alt+drag a face: pull a \
                          brush out of it (pushed in: a cut)  ·  Tab or Esc: done";

/// Editing brushes' faces, edges and corners (Tab, ADR 0110 E2b-5): the brushes; their faces
/// and their parts' corners this frame, each with its brush; what is under the pointer; a
/// face or corners being dragged.
#[derive(Debug)]
struct FaceEdit {
    brushes: Vec<Entity>,
    faces: Vec<Face>,
    owners: Vec<Entity>,
    points: Vec<PartCorners>,
    point_owners: Vec<Entity>,
    hovered: Option<Hover>,
    drag: Option<FaceDragging>,
    corner_drag: Option<(Entity, Brush, CornerDrag)>,
}

impl FaceEdit {
    /// Whether a face or corners are being dragged.
    fn dragging(&self) -> bool {
        self.drag.is_some() || self.corner_drag.is_some()
    }
}

/// What the pointer is over while editing faces: a face (its place in the faces), a
/// corner or an edge (its part's place in the corners, and its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hover {
    Face(usize),
    Corner(usize, usize),
    Edge(usize, usize),
}

/// A face being dragged: its brush as it was, the drag, and the brush a pull made.
#[derive(Debug)]
struct FaceDragging {
    entity: Entity,
    start: Brush,
    face: FaceDrag,
    pulled: Option<Entity>,
}

/// What is under the pointer at `at` while editing: a corner within `reach` (physical
/// pixels), else an edge within it, else the face the pointer's ray meets first.
fn hover(edit: &FaceEdit, view: &ViewProj, at: Vec2, reach: f32) -> Option<Hover> {
    let mut best: Option<(f32, Hover)> = None;
    for (p, pc) in edit.points.iter().enumerate() {
        for (k, &w) in pc.world.iter().enumerate() {
            if let Some(d) = view.screen(w).map(|s| s.distance(at))
                && d <= reach
                && best.is_none_or(|(b, _)| d < b)
            {
                best = Some((d, Hover::Corner(p, k)));
            }
        }
    }
    if best.is_none() {
        for (p, pc) in edit.points.iter().enumerate() {
            for (k, &(a, b)) in pc.edges.iter().enumerate() {
                let (Some(a), Some(b)) = (view.screen(pc.world[a]), view.screen(pc.world[b]))
                else {
                    continue;
                };
                let ab = b - a;
                let t = ((at - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
                let d = (a + ab * t).distance(at);
                if d <= reach && best.is_none_or(|(b, _)| d < b) {
                    best = Some((d, Hover::Edge(p, k)));
                }
            }
        }
    }
    best.map(|(_, h)| h)
        .or_else(|| faces::pick(&edit.faces, view.ray(at)).map(|(i, _)| Hover::Face(i)))
}

/// The faces being edited (ADR 0110 E2b-5), under the panels: their edges thin and light,
/// the parts' corners as small squares, the face, edge or corner under the pointer or
/// dragged light blue (the selection's outline is orange); while a face is dragged, how
/// far on a label.
fn draw_faces(editor: &mut Editor, rect: [u32; 4], glyphs: &GlyphTexture, canvas: &mut Canvas) {
    let Some(edit) = &editor.face_edit else {
        return;
    };
    let scale = editor.scale.max(0.5);
    let view = ViewProj::new(&editor.orbit, rect);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    for (i, f) in edit.faces.iter().enumerate() {
        if edit.hovered == Some(Hover::Face(i)) {
            continue;
        }
        for k in 0..f.corners.len() {
            let (a, b) = (f.corners[k], f.corners[(k + 1) % f.corners.len()]);
            if let (Some(a), Some(b)) = (view.screen(a), view.screen(b)) {
                let color = Vec4::new(0.9, 0.95, 1.0, 0.45);
                canvas.draw(&Quad::line(a, b, scale).color(color).order(order));
            }
        }
    }
    let blue = Vec4::new(0.45, 0.85, 1.0, 1.0);
    for (p, pc) in edit.points.iter().enumerate() {
        for (k, &w) in pc.world.iter().enumerate() {
            let Some(at) = view.screen(w) else {
                continue;
            };
            let lit = edit.hovered == Some(Hover::Corner(p, k));
            let (size, color) = if lit {
                (9.0, blue)
            } else {
                (6.0, Vec4::new(0.95, 0.97, 1.0, 1.0))
            };
            canvas.draw(
                &Quad::solid(at, Vec2::splat((size + 3.0) * scale))
                    .color(Vec4::new(0.08, 0.08, 0.1, 0.85))
                    .order(order),
            );
            canvas.draw(
                &Quad::solid(at, Vec2::splat(size * scale))
                    .color(color)
                    .order(order),
            );
        }
        for (k, &(a, b)) in pc.edges.iter().enumerate() {
            if edit.hovered == Some(Hover::Edge(p, k))
                && let (Some(a), Some(b)) = (view.screen(pc.world[a]), view.screen(pc.world[b]))
            {
                canvas.draw(&Quad::line(a, b, 3.0 * scale).color(blue).order(order));
            }
        }
    }
    let lit = match edit.hovered {
        Some(Hover::Face(i)) => edit.faces.get(i),
        _ => None,
    };
    if let Some(f) = lit {
        for k in 0..f.corners.len() {
            let (a, b) = (f.corners[k], f.corners[(k + 1) % f.corners.len()]);
            if let (Some(a), Some(b)) = (view.screen(a), view.screen(b)) {
                // Not the selection's orange: light blue, thicker.
                let color = Vec4::new(0.45, 0.85, 1.0, 1.0);
                canvas.draw(&Quad::line(a, b, 2.5 * scale).color(color).order(order));
            }
        }
    }
    let label_at = lit.and_then(|f| view.screen(f.middle()));
    let distance = edit.drag.as_ref().map(|d| d.face.distance);
    if let (Some(at), Some(d)) = (label_at, distance) {
        let mut text = if d > 0.0 {
            String::from("+")
        } else {
            String::new()
        };
        text.push_str(&transform::short(d));
        text.push_str(" m");
        label(&mut editor.text, glyphs, canvas, at, &text, scale);
    }
    canvas.set_clip(None);
}

/// What the blockout's groups are: each group, its epoch and its place, by group.
type BlockoutKey = Vec<(Entity, u64, [f32; 12])>;

/// What the blockout's groups are now. Colliders and the navigation are made again when it
/// changes.
fn blockout_key(blockout: &engine_app::Blockout, world: &World) -> BlockoutKey {
    let mut key: BlockoutKey = blockout
        .groups()
        .map(|(g, epoch)| {
            let placed = world
                .get::<GlobalTransform>(g)
                .map_or(Affine3A::IDENTITY, |t| t.0);
            (g, epoch, placed.to_cols_array())
        })
        .collect();
    key.sort_unstable_by_key(|k| k.0.to_bits());
    key
}

/// How long the project's folder stays quiet before a change is scanned (a save writes a
/// file in several steps; a copy of many files comes in a burst).
const ASSETS_QUIET: Duration = Duration::from_millis(150);

/// The listing's tiles: a picture's side by default and its bounds (logical pixels), and
/// how much a wheel's notch with Ctrl changes it.
const DEFAULT_TILE: f32 = 64.0;
const TILE_RANGE: (f32, f32) = (32.0, 128.0);
const TILE_NOTCH: f32 = 1.15;

/// The asset browser's state (E4-2).
#[derive(Debug, Default)]
struct Browser {
    /// The folder shown, under the project's (`""`: its top), and how the dock titles it.
    folder: String,
    label: String,
    /// The asset picked.
    selected: Option<AssetId>,
    /// The project's folders with assets in them, in tree order: each one's path and
    /// depth (the top first).
    folders: Vec<(String, u16)>,
    /// The shown folder's subfolders, then its files.
    listing: Vec<Listed>,
}

/// A line of the browser's listing: a subfolder or a file.
#[derive(Debug, Clone)]
struct Listed {
    name: String,
    kind: FileKind,
    /// Its path under the project's folder.
    path: String,
    /// The file's asset (none for a folder).
    asset: Option<AssetId>,
}

/// A file dragged out of the browser (ADR 0107 E4-3): what it is, where a release would
/// drop it, and a model placed in the view while the pointer is there.
#[derive(Debug)]
struct AssetDrag {
    asset: AssetId,
    kind: engine_scene::AssetKind,
    /// Its path under the project's folder, and its file's name.
    path: String,
    name: String,
    /// A model file's entities, read the first time they are needed (or why not).
    plan: Option<Result<Rc<ModelPlan>, String>>,
    /// The model where the pointer is in the view, in its undo step.
    placed: Option<Placed>,
    /// The selection before the model was placed: back when it is taken back.
    selection: Vec<Entity>,
    over: DropTarget,
}

/// A dragged model placed in the view: its undo step and its root.
#[derive(Debug, Clone, Copy)]
struct Placed {
    merge: u64,
    root: Entity,
}

/// Where a dragged file would drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropTarget {
    Nowhere,
    /// The viewport: a model on the surface under the pointer, a material onto the object.
    View,
    /// A row of the scene tree: a model under its entity, a material onto it.
    Row(Entity),
    /// The tree below its rows: a model as a root.
    Tree,
    /// An inspector field (section, field): the asset assigned.
    Field(usize, usize),
}

/// An asset `#` in the palette found.
#[derive(Debug, Clone, Copy)]
struct FoundAsset {
    id: AssetId,
    score: i32,
    /// Its matched characters: a range of the palette's `highlights`.
    matched: (u32, u32),
}

/// What placing model file `name` says: its meshes of several materials, which draw with
/// their first.
fn placed_message(name: &str, plan: &ModelPlan) -> String {
    if plan.mixed.is_empty() {
        format!("Placed {name}")
    } else {
        format!("Placed {name}: {}", plan.mixed.join("; "))
    }
}

/// What an inspector's asset field `field` holding `asset` shows a preview of: a model's
/// field (`mesh`, `material`) its part, or the engine's; another field its file.
fn field_subject(asset: AssetRef, field: &str, kind: Option<engine_scene::AssetKind>) -> Subject {
    use engine_scene::AssetKind as K;
    let part = asset.part != 0 || kind.is_none();
    match field {
        "mesh" if part => Subject::Mesh(asset),
        "material" if part || kind == Some(K::Model) => Subject::Material(asset),
        _ => Subject::File(asset.file),
    }
}

/// The browser's kind of an asset's kind.
fn file_kind(kind: engine_scene::AssetKind) -> FileKind {
    use engine_scene::AssetKind as K;
    match kind {
        K::Scene => FileKind::Scene,
        K::Model => FileKind::Mesh,
        K::Texture => FileKind::Texture,
        K::Sound => FileKind::Audio,
        K::Script => FileKind::Script,
        K::Material | K::Video | K::Font => FileKind::Other,
    }
}

/// The palette's icon of an asset's kind.
fn file_icon_of(kind: engine_scene::AssetKind) -> &'static str {
    match file_kind(kind) {
        FileKind::Folder => icons::FOLDER,
        FileKind::Scene => icons::SCENE,
        FileKind::Script => icons::SCRIPT,
        FileKind::Texture => icons::TEXTURE,
        FileKind::Mesh => icons::MESH,
        FileKind::Audio => icons::AUDIO,
        FileKind::Other => icons::FILE,
    }
}

/// The asset kind a `#` filter names: the kinds' own names, and `mesh` and `audio`.
fn asset_kind_named(name: &str) -> Option<engine_scene::AssetKind> {
    match name {
        "mesh" => Some(engine_scene::AssetKind::Model),
        "audio" => Some(engine_scene::AssetKind::Sound),
        other => engine_scene::AssetKind::named(other),
    }
}

/// What the Play controls asked for (ADR 0107 E3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayRequest {
    /// Play, from the editor's camera with `from_here`.
    Play {
        from_here: bool,
    },
    Stop,
    /// Stop and play again, as the last Play did.
    Restart,
    /// Pause, or go on.
    Pause,
    /// One tick while paused (pausing first).
    Step,
}

/// Playing (ADR 0107 E3): the scene's world as Play found it, to put back at Stop.
struct Playing {
    snapshot: WorldSnapshot,
    /// The world's resources then: those the game adds go at Stop.
    resources: Vec<TypeId>,
    /// The history then.
    mark: PlayMark,
    /// The selection then, by scene id.
    selection: Vec<SceneId>,
    started: Instant,
    /// How long entering took (the session saved, the world kept, the game made).
    entered: Duration,
    /// Played from the editor's camera ("Play from Here").
    from_here: bool,
    /// The world's time stands; the game's hooks are not called.
    paused: bool,
    /// The entities whose changes Stop keeps (made while playing: kept as new ones), and
    /// the scene entities whose going it keeps.
    keep: Vec<Entity>,
    keep_gone: Vec<SceneId>,
    /// The editor's camera and tools on the running world (Unreal's eject), with the
    /// devices the game's input listened to before, given back on returning.
    ejected: Option<Option<engine_input::DeviceFilter>>,
}

/// Whether `key` is one of F1 to F12 (the editor's keys while playing).
fn is_function_key(key: Key) -> bool {
    matches!(
        key,
        Key::F1
            | Key::F2
            | Key::F3
            | Key::F4
            | Key::F5
            | Key::F6
            | Key::F7
            | Key::F8
            | Key::F9
            | Key::F10
            | Key::F11
            | Key::F12
    )
}

thread_local! {
    /// The last panic on this thread, with where it happened (`file:line`).
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Keeps every panic's place for [`panic_note`], before the panic hook in place (the
/// crash reporter's, or the default) runs: a panic in the game's hooks during Play is
/// said at its line.
fn note_panics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = panic_text(info.payload());
            let note = match info.location() {
                Some(at) => format!("{message} ({}:{})", at.file(), at.line()),
                None => message,
            };
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(note));
            previous(info);
        }));
    });
}

/// A panic's message.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("a panic"))
}

/// What to say about the panic just caught: its message with its place.
fn panic_note(payload: &(dyn std::any::Any + Send)) -> String {
    LAST_PANIC
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| panic_text(payload))
}

/// A group baked into a file (Blockout: Bake to Mesh): its name, the file it was baked
/// into before (or none), the file's id and path, its meshes with their materials, and
/// how many triangles they hold.
struct Baked {
    group: Entity,
    name: String,
    before: AssetId,
    file: AssetId,
    path: String,
    parts: Vec<(String, String)>,
    triangles: usize,
}

/// What a list calls entity `e`: its name, or its kind.
fn label_of(world: &World, e: Entity) -> String {
    world
        .get::<Name>(e)
        .map_or_else(|| kind_of(world, e).1.to_owned(), |n| n.as_str().to_owned())
}

/// The blockout group `e` is in: itself, or its nearest ancestor with a [`BrushGroup`].
fn group_of(world: &World, e: Entity) -> Option<Entity> {
    let mut at = Some(e);
    while let Some(x) = at {
        if world.get::<BrushGroup>(x).is_some() {
            return Some(x);
        }
        at = world.parent(x);
    }
    None
}

/// The baked group `e` is in, if it is in one: its brushes are not drawn, and the tools
/// leave them alone until it is unbaked.
fn baked_group(world: &World, e: Entity) -> Option<Entity> {
    group_of(world, e).filter(|&g| world.get::<BrushGroup>(g).is_some_and(BrushGroup::is_baked))
}

/// What to say when a tool meets the brushes of baked group `group`.
fn baked_message(world: &World, group: Entity) -> String {
    let name = world
        .get::<Name>(group)
        .map_or_else(|| String::from("The group"), |n| n.as_str().to_owned());
    format!("{name} is baked: Blockout: Unbake to edit its brushes")
}

/// Group `group`'s children that are models of files `files` (a bake's), not brushes.
fn baked_models(world: &World, group: Entity, files: &[AssetId]) -> Vec<Entity> {
    world
        .children(group)
        .filter(|&c| {
            world.get::<Brush>(c).is_none()
                && world
                    .get::<Model>(c)
                    .is_some_and(|m| !m.mesh.file.is_none() && files.contains(&m.mesh.file))
        })
        .collect()
}

/// A navigation mesh baked in the background: its edges (ends, a boundary or not), how
/// many tiles, from how many triangles, in how long.
#[derive(Debug)]
struct NavBaked {
    edges: Vec<(Vec3, Vec3, bool)>,
    tiles: usize,
    triangles: usize,
    ms: f64,
}

/// How long the brushes stay as they are before their navigation is baked again.
const NAV_WAIT: Duration = Duration::from_millis(300);

/// The most navigation edges drawn.
const NAV_EDGES: usize = 60_000;

/// The navigation mesh over the viewport (ADR 0110 E2b-4b), under the panels: its
/// boundaries (walls, drops) bright green, the edges between its polygons faint, a
/// couple of centimetres above the surface.
fn draw_navigation(editor: &Editor, rect: [u32; 4], canvas: &mut Canvas) {
    if !editor.show_nav || editor.nav_edges.is_empty() {
        return;
    }
    let scale = editor.scale.max(0.5);
    let view = ViewProj::new(&editor.orbit, rect);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    let lift = Vec3::Y * 0.02;
    for &(a, b, boundary) in editor.nav_edges.iter().take(NAV_EDGES) {
        if let (Some(a), Some(b)) = (view.screen(a + lift), view.screen(b + lift)) {
            let (width, color) = if boundary {
                (1.5 * scale, Vec4::new(0.35, 1.0, 0.45, 0.9))
            } else {
                (scale, Vec4::new(0.35, 1.0, 0.45, 0.3))
            };
            canvas.draw(&Quad::line(a, b, width).color(color).order(order));
        }
    }
    canvas.set_clip(None);
}

/// The most collider edges drawn: a level's worth; beyond, the overlay would cost frames.
const COLLIDER_EDGES: usize = 60_000;

/// The blockout's colliders over the viewport (ADR 0110 E2b-4), under the panels: the
/// edges of the brushes' pieces that collide, thin and cyan, as the physics gets them.
fn draw_colliders(editor: &Editor, rect: [u32; 4], canvas: &mut Canvas) {
    if !editor.show_colliders || editor.collider_edges.is_empty() {
        return;
    }
    let scale = editor.scale.max(0.5);
    let view = ViewProj::new(&editor.orbit, rect);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    let color = Vec4::new(0.35, 0.95, 1.0, 0.7);
    for &(a, b) in editor.collider_edges.iter().take(COLLIDER_EDGES) {
        if let (Some(a), Some(b)) = (view.screen(a), view.screen(b)) {
            canvas.draw(&Quad::line(a, b, scale).color(color).order(order));
        }
    }
    canvas.set_clip(None);
}

/// A size over the viewport: `text` on a dark label centered on `at`.
fn label(
    text_cache: &mut UiText,
    glyphs: &GlyphTexture,
    canvas: &mut Canvas,
    at: Vec2,
    text: &str,
    scale: f32,
) {
    let order = Order::layer(-1);
    let size = 13.0 * scale;
    let width = text.chars().count() as f32 * size * 0.55 + 10.0 * scale;
    canvas.draw(
        &Quad::rounded_rect(at, Vec2::new(width, size + 6.0 * scale), 4.0 * scale)
            .color(Vec4::new(0.08, 0.08, 0.1, 0.8))
            .order(order),
    );
    text_cache.draw_centered(canvas, glyphs, text, size, at, Vec4::ONE, order);
}

/// The viewport while the game plays in a window of its own: the dock's color and a word
/// on where the game is.
fn draw_playing_elsewhere(
    editor: &mut Editor,
    rect: [u32; 4],
    glyphs: &GlyphTexture,
    canvas: &mut Canvas,
) {
    let [x, y, w, h] = rect.map(|v| v as f32);
    let (min, max) = (Vec2::new(x, y), Vec2::new(x + w, y + h));
    canvas.draw(
        &Quad::solid((min + max) * 0.5, max - min)
            .color(Vec4::from_array(editor.palette.dock))
            .order(Order::layer(-1)),
    );
    let scale = editor.scale.max(0.5);
    label(
        &mut editor.text,
        glyphs,
        canvas,
        (min + max) * 0.5,
        "The game plays in its window: Shift+F8 brings it here, F8 stops",
        scale,
    );
}

/// The active brush's side handles (ADR 0110 E2b-3c), under the panels: light squares on
/// dark rims, the one under the pointer or dragged orange; while a side is dragged, its
/// size on a label above it.
fn draw_sides(editor: &mut Editor, rect: [u32; 4], glyphs: &GlyphTexture, canvas: &mut Canvas) {
    if editor.handles.at.is_empty() {
        return;
    }
    let scale = editor.scale.max(0.5);
    canvas.set_clip(Some(clip_of(rect)));
    let order = Order::layer(-1);
    for &(side, at) in &editor.handles.at {
        let lit = editor.hovered_side == Some(side);
        let color = if lit {
            Vec4::new(1.0, 0.62, 0.18, 1.0)
        } else {
            Vec4::new(0.9, 0.95, 1.0, 1.0)
        };
        canvas.draw(
            &Quad::solid(at, Vec2::splat(11.0 * scale))
                .color(Vec4::new(0.08, 0.08, 0.1, 0.85))
                .order(order),
        );
        canvas.draw(
            &Quad::solid(at, Vec2::splat(7.0 * scale))
                .color(color)
                .order(order),
        );
    }
    if let Some(drag) = &editor.side_drag
        && let Some(&(_, at)) = editor.handles.at.iter().find(|(s, _)| *s == drag.side)
    {
        let mut text = transform::short(drag.length);
        text.push_str(" m");
        label(
            &mut editor.text,
            glyphs,
            canvas,
            at - Vec2::new(0.0, 20.0 * scale),
            &text,
            scale,
        );
    }
    canvas.set_clip(None);
}

/// Paints the editor's shell over the player's frame.
pub struct EditorOverlay {
    shared: Rc<RefCell<Editor>>,
    renderer: Renderer2d,
    glyphs: GlyphTexture,
    canvas: Canvas,
    /// Picks in the scene's view (ADR 0109).
    picker: Picker,
    /// Outlines the selection in it.
    outliner: Outliner,
    /// The listing's thumbnails, in cells (ADR 0112).
    atlas: engine_render::texture::GpuTexture,
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
            self.atlas
                .destroy(setup.device, setup.allocator, setup.bindless, 0);
            self.picker.destroy(setup.allocator);
        }
        Ok(())
    }
}

impl Overlay for EditorOverlay {
    fn record<'g>(&'g mut self, pass: &mut OverlayPass<'_, 'g>) -> Result<(), AppError> {
        {
            let mut shared = self.shared.borrow_mut();
            let editor = &mut *shared;
            // Picking (ADR 0109): the newest pick's result, then this frame's pick.
            if let Some(view) = pass.view {
                let (render, output) = (view.frame.render, view.frame.output);
                editor.view_scale = [
                    render.0 as f32 / output.0.max(1) as f32,
                    render.1 as f32 / output.1.max(1) as f32,
                ];
                editor.frame_seen = view.frame_number;
                if let Some(result) = self.picker.poll(view.frame_number) {
                    editor.picked =
                        Some((result.request, result.frame, result.nearest(PICK_RADIUS)));
                }
                if let Some(result) = self.picker.poll_list(view.frame_number) {
                    editor.listed = Some((
                        result.request,
                        result.frame,
                        result.hits.iter().map(|&(slot, _)| slot).collect(),
                    ));
                }
                let rect = view.rect.unwrap_or(engine_rhi::ash::vk::Rect2D {
                    offset: engine_rhi::ash::vk::Offset2D::default(),
                    extent: pass.extent,
                });
                let aspect = rect.extent.width as f32 / rect.extent.height.max(1) as f32;
                // The scene's picks draw its candidates through the camera without jitter,
                // at the render size.
                let pick_view = PickView {
                    clip_from_world: view.camera.clip_from_world(aspect),
                    size: render,
                };
                if let Some(request) = editor.list_request.take() {
                    let declared = match view.scene {
                        Some(scene) => self.picker.add_list_pass(
                            pass.fg,
                            pass.ring,
                            pass.bindless,
                            view.geometry,
                            view.materials,
                            scene,
                            &pick_view,
                            request,
                            view.frame_number,
                        ),
                        None => PickPass::Empty,
                    };
                    // Nothing under the pixel: answered at once.
                    if declared == PickPass::Empty {
                        editor.listed = Some((request, view.frame_number, Vec::new()));
                    }
                }
                // The selection's instances (its outline's items but the hovered one): its
                // world box, and what surface picks leave out.
                let selected: Vec<u32> = editor
                    .outline_items
                    .iter()
                    .filter(|i| i.class != OutlineClass::Hovered)
                    .map(|i| i.slot)
                    .collect();
                if let Some(scene) = view.scene {
                    editor.selection_box = selected
                        .iter()
                        .filter_map(|&s| scene.slot_box(s))
                        .reduce(|(a, b), (lo, hi)| (a.min(lo), b.max(hi)));
                    if let Some(result) = self.picker.poll_surface(view.frame_number) {
                        let result = *result;
                        match editor.dropping.as_mut().and_then(|d| d.current.as_mut()) {
                            Some(one) if one.asked.is_some_and(|(f, _)| f == result.frame) => {
                                one.found = Some(result.hit);
                            }
                            _ => editor.surfaced = Some(result),
                        }
                    }
                    // Under the pointer, through the view's camera.
                    if let Some(request) = editor.surface_request.take() {
                        // A drawing lands on the selection too; a drag leaves it out.
                        let exclude: &[u32] = if editor.draw.is_some() {
                            &[]
                        } else {
                            &selected
                        };
                        self.picker.add_surface_pass(
                            pass.fg,
                            pass.ring,
                            pass.bindless,
                            view.geometry,
                            view.materials,
                            scene,
                            &pick_view,
                            request,
                            exclude,
                            view.frame_number,
                        );
                    }
                    // End: below the entity's box's bottom, looking down, a centimeter a
                    // pixel.
                    if let Some(one) = editor.dropping.as_mut().and_then(|d| d.current.as_mut())
                        && one.asked.is_none()
                        && let Some(slots) = &one.slots
                    {
                        let (lo, hi) = slots
                            .iter()
                            .filter_map(|&s| scene.slot_box(s))
                            .reduce(|(a, b), (lo, hi)| (a.min(lo), b.max(hi)))
                            .unwrap_or((one.place, one.place));
                        let below = Vec3::new((lo.x + hi.x) * 0.5, lo.y, (lo.z + hi.z) * 0.5);
                        let down = PickView {
                            clip_from_world: vulkan_orthographic(
                                -0.015, 0.015, -0.015, 0.015, 0.001, 10_000.0,
                            ) * look_at(below, below - Vec3::Y, Vec3::NEG_Z),
                            size: (3, 3),
                        };
                        let mut exclude = selected.clone();
                        exclude.extend_from_slice(slots);
                        let declared = self.picker.add_surface_pass(
                            pass.fg,
                            pass.ring,
                            pass.bindless,
                            view.geometry,
                            view.materials,
                            scene,
                            &down,
                            SurfaceRequest {
                                x: 1,
                                y: 1,
                                transparent: editor.pick_transparent,
                            },
                            &exclude,
                            view.frame_number,
                        );
                        match declared {
                            PickPass::Declared => one.asked = Some((view.frame_number, lo.y)),
                            // Nothing below: answered at once.
                            PickPass::Empty => {
                                one.asked = Some((view.frame_number, lo.y));
                                one.found = Some(None);
                            }
                            PickPass::Busy => {}
                        }
                    }
                }
                // Framing (F): the selection's world boxes (its outline's items but the
                // hovered one) with its places, or the scene's bounds.
                if let Some(points) = editor.frame_asked.take() {
                    let mut bounds = points;
                    if let Some(scene) = view.scene {
                        if points.is_none() {
                            let (lo, hi) = scene.bounds();
                            bounds = (lo.cmple(hi).all()).then_some((lo, hi));
                        } else {
                            for item in &editor.outline_items {
                                if item.class != OutlineClass::Hovered
                                    && let Some((lo, hi)) = scene.slot_box(item.slot)
                                {
                                    bounds = Some(
                                        bounds.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))),
                                    );
                                }
                            }
                        }
                    }
                    editor.framed = bounds;
                }
                // The square under the pointer last: drawn from the view's commands, it holds
                // the picker for the frame.
                if let Some(request) = editor.pick_request.take() {
                    let declared = match view.scene {
                        Some(scene) => self.picker.add_scene_pass(
                            pass.fg,
                            pass.ring,
                            pass.bindless,
                            view.geometry,
                            view.materials,
                            scene,
                            &pick_view,
                            request,
                            view.frame_number,
                        ),
                        None => self.picker.add_pass(
                            pass.fg,
                            pass.bindless,
                            view.geometry,
                            view.materials,
                            &view.frame.passes,
                            render,
                            request,
                            view.frame_number,
                        ),
                    };
                    // Nothing under the pixel: answered at once.
                    if declared == PickPass::Empty {
                        editor.picked = Some((request, view.frame_number, None));
                    }
                }
                // The outlines, below the panels (none in the game's view).
                if let Some(scene) = view.scene
                    && !editor.outline_items.is_empty()
                    && !editor.game_view()
                {
                    self.outliner.add_passes(
                        pass.fg,
                        pass.ring,
                        pass.bindless,
                        view.geometry,
                        view.materials,
                        scene,
                        &editor.outline_items,
                        &OutlineView {
                            target: pass.target,
                            rect,
                            clip_from_world: view.camera.clip_from_world(aspect),
                            near: view.camera.near,
                            far: view.camera.far,
                            scene_depth: view.frame.depth,
                            render: view.frame.render,
                        },
                        &OutlineStyle::default().scaled(editor.scale.max(0.5)),
                    );
                }
            }
            self.canvas.clear();
            // The window's color behind the docks: all but the viewport, which the 3D view
            // fills. Nothing else draws in the 1-pixel gaps between docks, and the output
            // holds whatever its memory held before.
            draw_window(
                editor.view_rect,
                pass.extent,
                editor.palette.window,
                &mut self.canvas,
            );
            if editor.key_transform.is_some()
                && let Some(rect) = editor.view_rect
            {
                draw_guides(
                    &editor.guides,
                    rect,
                    editor.scale.max(0.5),
                    &mut self.canvas,
                );
            }
            if editor.gizmo_shown
                && let Some(rect) = editor.view_rect
            {
                let lit = editor
                    .gizmo_drag
                    .as_ref()
                    .map(|d| d.handle)
                    .or(editor.hovered_handle);
                draw_gizmo(
                    &editor.gizmo,
                    lit,
                    rect,
                    editor.scale.max(0.5),
                    &mut self.canvas,
                );
            }
            if let Some(rect) = editor
                .view_rect
                .filter(|_| editor.in_view_window && editor.game_view())
            {
                draw_playing_elsewhere(editor, rect, &self.glyphs, &mut self.canvas);
            }
            if let Some(rect) = editor.view_rect.filter(|_| !editor.game_view()) {
                draw_overlays(editor, rect, &self.glyphs, &mut self.canvas);
                draw_navigation(editor, rect, &mut self.canvas);
                draw_colliders(editor, rect, &mut self.canvas);
                draw_sides(editor, rect, &self.glyphs, &mut self.canvas);
                draw_faces(editor, rect, &self.glyphs, &mut self.canvas);
                if editor.draw.is_some() {
                    draw_drawing(editor, rect, &self.glyphs, &mut self.canvas);
                }
            }
            if let Some(b) = &editor.box_drag
                && b.released.is_none()
                && let Some(rect) = editor.view_rect
            {
                draw_box(
                    (b.from, b.to),
                    editor.input.alt(),
                    rect,
                    editor.scale.max(0.5),
                    &mut self.canvas,
                );
            }
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
            // The thumbnails made since: into their cells, before the panels draw them.
            if !self.atlas.is_initialized() {
                let clear = vec![0u8; (thumbnails::ATLAS * thumbnails::ATLAS * 4) as usize];
                self.atlas
                    .upload(pass.fg, pass.ring, &clear)
                    .map_err(AppError::at("the thumbnails' atlas"))?;
            }
            for ((x, y), pixels) in editor.thumbnails.uploads() {
                let side = thumbnails::SIZE;
                self.atlas
                    .upload_region(pass.fg, pass.ring, (x, y, side, side), pixels)
                    .map_err(AppError::at("a thumbnail"))?;
            }
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
        self.outliner
            .set_output(setup.device, setup.bindless, output.format, output.white)
            .map_err(AppError::at("the editor's outlines"))?;
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
    fn h_disables_the_selection_with_what_is_below_and_undo_enables_it() {
        let mut world = World::new();
        let parent = world.spawn((SceneId::new(), Name::new("Parent"), Transform::IDENTITY));
        let child = world.spawn((SceneId::new(), Name::new("Child"), Transform::IDENTITY));
        world.set_parent(child, Some(parent)).unwrap();
        let mut ed = Editor::new(
            [1280, 720],
            std::env::temp_dir(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.select_only(parent);
        ed.run(&mut world, ed.ids.disable);
        assert!(world.get::<Disabled>(parent).is_some());
        assert!(
            world.get::<Disabled>(child).is_none(),
            "only the selection is disabled"
        );
        assert!(is_off(&world, child), "but the child is off below it");
        // Again: on.
        ed.run(&mut world, ed.ids.disable);
        assert!(world.get::<Disabled>(parent).is_none());
        // Off again, then undone: on; redone: off.
        ed.run(&mut world, ed.ids.disable);
        ed.run(&mut world, ed.ids.undo);
        assert!(world.get::<Disabled>(parent).is_none());
        ed.run(&mut world, ed.ids.redo);
        assert!(world.get::<Disabled>(parent).is_some());
        // The tree shows the eye shut and the child dim.
        ed.update(&mut world);
        let items: Vec<(bool, bool)> = ed
            .rows
            .iter()
            .map(|r| {
                (
                    world.get::<Disabled>(r.entity).is_some(),
                    is_off(&world, r.entity),
                )
            })
            .collect();
        assert!(
            items.contains(&(true, true)) && items.contains(&(false, true)),
            "{items:?}"
        );
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
    fn the_browser_lists_the_projects_assets_and_the_palette_finds_them() {
        let dir = std::env::temp_dir().join(format!("ofelia-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("props/rocks")).unwrap();
        std::fs::write(dir.join("props/crate.glb"), b"a crate").unwrap();
        std::fs::write(dir.join("props/rocks/rock.png"), b"a rock").unwrap();
        std::fs::write(dir.join("props/rocks/rock.glb"), b"a rock mesh").unwrap();
        std::fs::write(dir.join("theme.ogg"), b"music").unwrap();
        let mut world = World::new();
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        let report = ed.open_assets();
        assert_eq!(report.files, 4);
        // The folders in tree order, the top first; the top's listing: its subfolder, then
        // its file.
        let folders: Vec<&str> = ed.browser.folders.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(folders, ["", "props", "props/rocks"]);
        let listing: Vec<(&str, FileKind)> = ed
            .browser
            .listing
            .iter()
            .map(|l| (l.name.as_str(), l.kind))
            .collect();
        assert_eq!(
            listing,
            [("props", FileKind::Folder), ("theme.ogg", FileKind::Audio)]
        );
        assert_eq!(ed.browser.label, "res://");
        // A folder opened from the listing.
        ed.open_folder("props/rocks");
        let names: Vec<&str> = ed.browser.listing.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["rock.glb", "rock.png"]);
        // `#rock` finds both rocks; `#texture:rock` the image alone; Enter reveals it.
        ed.open_palette(&mut world, "#rock");
        assert_eq!(ed.command_palette.as_ref().unwrap().assets.len(), 2);
        ed.open_palette(&mut world, "#texture:rock");
        let cp = ed.command_palette.as_ref().unwrap();
        assert_eq!(cp.assets.len(), 1);
        let rock = cp.assets[0].id;
        ed.open_folder("");
        ed.palette_action(&mut world, PaletteAction::Run(0));
        assert_eq!(ed.browser.folder, "props/rocks");
        assert_eq!(ed.browser.selected, Some(rock));
        assert_eq!(ed.asset_db.get(rock).unwrap().path, "props/rocks/rock.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project with a glTF of one node (a crate, a unit box standing on its origin,
    /// in wood) in `props/`, and a red material file beside it; the editor over it with
    /// its assets read and `props/` shown, and a box with a model in the scene.
    fn drop_project(name: &str) -> (PathBuf, World, Editor, Entity) {
        let dir = folder(name);
        std::fs::create_dir_all(dir.join("props")).unwrap();
        let gltf = r#"{
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"name": "Crate", "mesh": 0}],
            "meshes": [{"name": "Crate", "primitives": [
                {"attributes": {"POSITION": 0}, "material": 0}]}],
            "materials": [{"name": "Wood"}],
            "accessors": [{"componentType": 5126, "count": 3, "type": "VEC3",
                "min": [-0.5, 0, -0.5], "max": [0.5, 1, 0.5]}]
        }"#;
        std::fs::write(dir.join("props/crate.gltf"), gltf).unwrap();
        let red = engine_app::MaterialFile {
            base_color: [0.8, 0.1, 0.1, 1.0],
            ..engine_app::MaterialFile::default()
        };
        std::fs::write(dir.join("props/red.material"), red.to_text(AssetId::new())).unwrap();
        let mut world = World::new();
        let boxed = world.spawn((
            SceneId::new(),
            Name::new("Box"),
            Transform::from_xyz(0.0, 0.0, -3.0),
            GlobalTransform::default(),
            Model::new(builtin::CUBE, builtin::MATERIAL),
            RootOrder(RootOrder::STEP),
        ));
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        assert_eq!(ed.open_assets().files, 2);
        ed.open_folder("props");
        ed.update(&mut world);
        (dir, world, ed, boxed)
    }

    /// The middle of rectangle `r`.
    fn middle(r: Rect) -> [f32; 2] {
        [r.pos[0] + r.size[0] / 2.0, r.pos[1] + r.size[1] / 2.0]
    }

    /// File `name` of the browser's listing pressed and dragged a little, a frame each:
    /// the drag begins.
    fn drag_file(ed: &mut Editor, world: &mut World, name: &str) {
        let at = middle(texts(ed, name)[0]);
        mouse(ed, at, None);
        ed.update(world);
        mouse(ed, at, Some(true));
        ed.update(world);
        mouse(ed, [at[0] + 12.0, at[1]], None);
        ed.update(world);
        assert!(ed.asset_drag.is_some(), "dragging {name}");
    }

    /// The viewport's middle (the window's pixels).
    fn view_middle(ed: &Editor) -> [f32; 2] {
        let [x, y, w, h] = ed.view_rect.unwrap();
        [(x + w / 2) as f32, (y + h / 2) as f32]
    }

    #[test]
    fn a_model_dragged_into_the_view_rests_on_the_surface_and_drops_as_one_step() {
        let (dir, mut world, mut ed, boxed) = drop_project("drop-view");
        let file = ed
            .browser
            .listing
            .iter()
            .find(|l| l.name == "crate.gltf")
            .unwrap();
        let id = file.asset.unwrap();
        ed.select_only(boxed);
        ed.snapping.on = true;
        drag_file(&mut ed, &mut world, "crate.gltf");
        assert_eq!(ed.browser.selected, Some(id), "the file picked");
        // Over the view: placed, selected (surface picks leave it out), the surface under
        // the pointer asked for.
        let at = view_middle(&ed);
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        let placed = engine_scene::find_named(&mut world, "crate").expect("placed");
        assert_eq!(ed.selection, [placed]);
        let model = *world.get::<Model>(placed).unwrap();
        assert_eq!(model.mesh, AssetRef::part(id, "Crate"));
        assert_eq!(model.material, AssetRef::part(id, "Wood"));
        assert_eq!(ed.assets.get(model.mesh), Some("props/crate.gltf#Crate"));
        assert!(ed.surface_request.is_some());
        // A floor found at y = 1: the crate stands on it, its place on the grid.
        ed.surfaced = Some(engine_render::pick::SurfaceResult {
            frame: 1,
            request: ed.surface_request.unwrap(),
            hit: Some(hit(Vec3::new(2.33, 1.0, -0.61), Vec3::Y)),
        });
        mouse(&mut ed, [at[0] + 1.0, at[1]], None);
        ed.update(&mut world);
        let t = world.get::<Transform>(placed).unwrap().translation;
        assert!((t - Vec3::new(2.3, 1.0, -0.6)).length() < 1e-5, "{t}");
        assert_eq!(ed.history.steps().count(), 1, "one step while it moves");
        // Let go: kept, one step, selected; undo takes it away.
        mouse(&mut ed, [at[0] + 1.0, at[1]], Some(false));
        ed.update(&mut world);
        assert!(ed.asset_drag.is_none());
        assert_eq!(ed.history.steps().count(), 1);
        assert_eq!(ed.selection, [placed]);
        ed.history.undo(&mut world);
        assert!(!world.contains(placed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escape_or_leaving_the_view_takes_a_dragged_model_back() {
        let (dir, mut world, mut ed, boxed) = drop_project("drop-escape");
        ed.select_only(boxed);
        drag_file(&mut ed, &mut world, "crate.gltf");
        let at = view_middle(&ed);
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        assert!(engine_scene::find_named(&mut world, "crate").is_some());
        // Back over the browser: taken back, the selection as it was.
        let listing = middle(texts(&ed, "red.material")[0]);
        mouse(&mut ed, listing, None);
        ed.update(&mut world);
        assert!(engine_scene::find_named(&mut world, "crate").is_none());
        assert_eq!(ed.selection, [boxed]);
        // Over the view again, then Escape: taken back, no step, the drag over.
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        assert!(engine_scene::find_named(&mut world, "crate").is_some());
        key(&mut ed, ESCAPE.0, ESCAPE.1, true);
        ed.update(&mut world);
        key(&mut ed, ESCAPE.0, ESCAPE.1, false);
        assert!(engine_scene::find_named(&mut world, "crate").is_none());
        assert!(ed.asset_drag.is_none());
        assert_eq!(ed.history.steps().count(), 0);
        assert_eq!(ed.selection, [boxed], "Escape went to the drag alone");
        // The release that follows drops nothing.
        mouse(&mut ed, at, Some(false));
        ed.update(&mut world);
        assert!(engine_scene::find_named(&mut world, "crate").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_drop_onto_tree_rows_and_inspector_fields() {
        let (dir, mut world, mut ed, boxed) = drop_project("drop-tree");
        let id_of = |ed: &Editor, name: &str| {
            ed.browser
                .listing
                .iter()
                .find(|l| l.name == name)
                .and_then(|l| l.asset)
                .unwrap()
        };
        let (crate_id, red) = (id_of(&ed, "crate.gltf"), id_of(&ed, "red.material"));
        // The tree's row of the box (left of the inspector's name field).
        let row = |ed: &Editor| {
            let r = texts(ed, "Box")
                .into_iter()
                .min_by(|a, b| a.pos[0].total_cmp(&b.pos[0]))
                .unwrap();
            middle(r)
        };
        // A material onto the row: the box's model takes it, as one step.
        drag_file(&mut ed, &mut world, "red.material");
        let r = row(&ed);
        mouse(&mut ed, r, None);
        ed.update(&mut world);
        assert_eq!(ed.asset_drag.as_ref().unwrap().over, DropTarget::Row(boxed));
        let r = row(&ed);
        mouse(&mut ed, r, Some(false));
        ed.update(&mut world);
        assert_eq!(
            world.get::<Model>(boxed).unwrap().material,
            AssetRef::file(red)
        );
        assert_eq!(ed.history.steps().count(), 1);
        // A model onto the row: under the box at its origin, selected.
        drag_file(&mut ed, &mut world, "crate.gltf");
        let r = row(&ed);
        mouse(&mut ed, r, None);
        ed.update(&mut world);
        let r = row(&ed);
        mouse(&mut ed, r, Some(false));
        ed.update(&mut world);
        let child = engine_scene::find_named(&mut world, "crate").unwrap();
        assert_eq!(world.parent(child), Some(boxed));
        assert_eq!(
            world.get::<Transform>(child).unwrap().translation,
            Vec3::ZERO
        );
        assert_eq!(ed.selection, [child]);
        assert_eq!(ed.history.steps().count(), 2);
        // The box selected: the model file onto its mesh field, the material file onto
        // its material field.
        ed.select_only(boxed);
        ed.update(&mut world);
        world.get_mut::<Model>(boxed).unwrap().material = builtin::MATERIAL;
        for (file, field) in [("crate.gltf", "mesh"), ("red.material", "material")] {
            drag_file(&mut ed, &mut world, file);
            let at = middle(texts(&ed, field)[0]);
            mouse(&mut ed, at, None);
            ed.update(&mut world);
            assert!(
                matches!(ed.asset_drag.as_ref().unwrap().over, DropTarget::Field(..)),
                "over {field}"
            );
            mouse(&mut ed, at, Some(false));
            ed.update(&mut world);
        }
        let model = *world.get::<Model>(boxed).unwrap();
        assert_eq!(model.mesh, AssetRef::part(crate_id, "Crate"));
        assert_eq!(model.material, AssetRef::file(red));
        // A material file is no mesh.
        drag_file(&mut ed, &mut world, "red.material");
        let at = middle(texts(&ed, "mesh")[0]);
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        mouse(&mut ed, at, Some(false));
        ed.update(&mut world);
        assert_eq!(world.get::<Model>(boxed).unwrap().mesh, model.mesh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_images_tile_shows_its_thumbnail_and_caches_it() {
        let dir = folder("thumbnails");
        std::fs::create_dir_all(dir.join("props")).unwrap();
        let rgba: Vec<u8> = (0..64 * 32)
            .flat_map(|i| [(i % 64) as u8 * 4, 90, 160, 255])
            .collect();
        let png =
            engine_render::png::encode_png(64, 32, engine_render::png::PngFormat::Rgba, &rgba)
                .unwrap();
        std::fs::write(dir.join("props/paint.png"), png).unwrap();
        std::fs::write(dir.join("props/notes.luau"), "return {}").unwrap();
        let mut world = World::new();
        let mut ed = Editor::new(
            [1280, 720],
            dir.clone(),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.open_assets();
        ed.open_folder("props");
        // The overlay's atlas, as its bindless index.
        ed.thumbnails.set_atlas(7);
        let images = |ed: &Editor| {
            ed.ui
                .last_frame()
                .commands
                .iter()
                .filter(|c| matches!(c, DrawCmd::Image { image, .. } if image.texture == 7))
                .count()
        };
        let started = Instant::now();
        loop {
            ed.update(&mut world);
            // Shown once its pixels went to the atlas (what the overlay does each frame).
            let uploaded = ed.thumbnails.uploads().len();
            if images(&ed) > 0 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "made in time ({uploaded} uploads)"
            );
        }
        // One picture: the image's; the script keeps its icon.
        assert_eq!(images(&ed), 1);
        let id = ed.asset_db.find("props/paint.png").unwrap().id;
        let cache = dir.join(".ofelia/thumbnails");
        assert!(cache.join(format!("{id}.png")).is_file());
        assert!(cache.join(format!("{id}.key")).is_file());
        // Ctrl+wheel over the tiles makes them larger.
        let before = ed.tile;
        let at = middle(texts(&ed, "paint.png")[0]);
        mouse(&mut ed, at, None);
        key(&mut ed, CTRL.0, CTRL.1, true);
        event(&mut ed, WindowEventKind::Wheel { x: 0.0, y: 2.0 });
        ed.update(&mut world);
        key(&mut ed, CTRL.0, CTRL.1, false);
        assert!(ed.tile > before, "{} > {before}", ed.tile);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_asset_field_is_picked_from_the_palette() {
        let (dir, mut world, mut ed, boxed) = drop_project("asset-field");
        let red = ed
            .browser
            .listing
            .iter()
            .find(|l| l.name == "red.material")
            .and_then(|l| l.asset)
            .unwrap();
        ed.select_only(boxed);
        ed.update(&mut world);
        ed.update(&mut world);
        // The material field's box, right of its name: a click opens the palette on the
        // project's materials.
        let label = texts(&ed, "material")
            .into_iter()
            .max_by(|a, b| a.pos[0].total_cmp(&b.pos[0]))
            .unwrap();
        let at = [label.pos[0] + 150.0, label.pos[1] + label.size[1] / 2.0];
        click(&mut ed, &mut world, at);
        let cp = ed.command_palette.as_ref().expect("the palette open");
        assert_eq!(cp.query, "#material:");
        assert!(cp.for_field.is_some());
        assert_eq!(cp.assets.len(), 1, "the one material file");
        // Enter: the field takes it, as one step; the dock's pick is not moved.
        let before = ed.browser.selected;
        ed.palette_action(&mut world, PaletteAction::Run(0));
        assert_eq!(
            world.get::<Model>(boxed).unwrap().material,
            AssetRef::file(red)
        );
        assert_eq!(ed.history.steps().count(), 1);
        assert_eq!(ed.browser.selected, before);
        // Opened the plain way, the palette's assets go to the dock again.
        ed.open_palette(&mut world, "#red");
        assert!(ed.command_palette.as_ref().unwrap().for_field.is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn brushes_from_the_add_menu_join_a_blockout_group() {
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
        // The menu's groups: the pointer over "Blockout" opens its brushes beside it.
        ed.add_open = true;
        ed.update(&mut world);
        assert!(
            texts(&ed, "Torus").is_empty(),
            "closed until the pointer comes"
        );
        let group = texts(&ed, "Blockout")[0];
        let over = [group.pos[0] + 4.0, group.pos[1] + group.size[1] / 2.0];
        mouse(&mut ed, over, None);
        ed.update(&mut world);
        ed.update(&mut world);
        let torus = texts(&ed, "Torus");
        assert!(!torus.is_empty(), "opened beside the menu");
        assert!(torus[0].pos[0] > group.pos[0] + 100.0, "to its right");
        // A click on it: a torus brush in a new "Blockout" group.
        let at = [
            torus[0].pos[0] + 4.0,
            torus[0].pos[1] + torus[0].size[1] / 2.0,
        ];
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        press_release(&mut ed, &mut world, at);
        assert!(!ed.add_open, "the menu closed");
        assert_eq!(names(&mut world), ["Blockout", "Torus Brush"]);
        let torus = ed.active().unwrap();
        let group = world.parent(torus).unwrap();
        assert!(world.get::<BrushGroup>(group).is_some());
        assert_eq!(world.get::<Brush>(torus).unwrap().shape, BrushShape::Torus);
        // The next brush, the torus selected, joins its group; a cut too.
        ed.add(&mut world, 13);
        let cone = ed.active().unwrap();
        assert_eq!(world.parent(cone), Some(group));
        assert_eq!(world.get::<Brush>(cone).unwrap().shape, BrushShape::Cone);
        // The tree names their kinds: a brush, a cut.
        world.get_mut::<Brush>(cone).unwrap().op = BrushOp::Cut;
        assert_eq!(kind_of(&world, cone).1, "Cut");
        assert_eq!(kind_of(&world, torus).1, "Brush");
        assert_eq!(kind_of(&world, group), (icons::BLOCKOUT, "Blockout"));
        // Undone, the cone goes; again, the torus and the group it made.
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(names(&mut world), ["Blockout", "Torus Brush"]);
        ed.run(&mut world, ed.ids.undo);
        assert!(names(&mut world).is_empty());
    }

    const B: (u16, u16) = (0x42, 0x30);
    const TAB: (u16, u16) = (0x09, 0x0F);

    /// An editor looking steeply down at the origin, snapping on (0.1 m steps), so that
    /// whole pixels land on exact corners.
    fn draw_editor(name: &str, world: &mut World) -> Editor {
        let mut ed = Editor::new(
            [1280, 720],
            folder(name),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.orbit.pitch = 1.2;
        ed.orbit.distance = 10.0;
        ed.snapping.on = true;
        ed.update(world);
        ed
    }

    /// Where world point `p` is in the editor's view, physical pixels.
    fn on_view(ed: &Editor, p: Vec3) -> [f32; 2] {
        ViewProj::new(&ed.orbit, ed.view_rect.unwrap())
            .screen(p)
            .unwrap()
            .to_array()
    }

    #[test]
    fn b_draws_a_box_on_the_ground_in_three_clicks_as_one_step() {
        let mut world = World::new();
        let mut ed = draw_editor("draw-box", &mut world);
        chord(&mut ed, &mut world, &[B]);
        assert!(ed.draw.is_some(), "B starts drawing");
        // Over nothing the mark is on the ground: the first corner there.
        let first = on_view(&ed, Vec3::new(1.0, 0.0, 1.0));
        mouse(&mut ed, first, None);
        ed.update(&mut world);
        assert!(ed.status.starts_with(draw::HELP), "{}", ed.status);
        assert_eq!(ed.hint, "Box: click the first corner  ·  B: next shape");
        assert_eq!(ed.draw_lines.len(), 6, "the mark: a square and a cross");
        assert!(!ed.gizmo_shown);
        press_release(&mut ed, &mut world, first);
        // The base follows the pointer, its sides' sizes shown.
        let second = on_view(&ed, Vec3::new(3.0, 0.0, 2.5));
        mouse(&mut ed, second, None);
        ed.update(&mut world);
        assert_eq!(ed.draw_labels.len(), 2);
        assert_eq!(ed.draw_labels[0].1, "2 m");
        assert!(names(&mut world).is_empty(), "no brush without a height");
        press_release(&mut ed, &mut world, second);
        // Its height typed: the brush is there as it is drawn, in a new group.
        type_chars(&mut ed, "3");
        ed.update(&mut world);
        assert_eq!(names(&mut world), ["Blockout", "Box Brush"]);
        let (e, _) = ed.drawn.unwrap();
        let brush = world.get::<Brush>(e).unwrap();
        assert_eq!((brush.shape, brush.op), (BrushShape::Box, BrushOp::Add));
        assert!(
            (brush.size - Vec3::new(2.0, 3.0, 1.5)).length() < 1e-4,
            "{}",
            brush.size
        );
        let t = *world.get::<Transform>(e).unwrap();
        assert!(t.translation.distance(Vec3::new(2.0, 0.0, 1.75)) < 1e-4);
        assert!((t.rotation * Vec3::Y).distance(Vec3::Y) < 1e-4, "upright");
        assert!(world.get::<BrushGroup>(world.parent(e).unwrap()).is_some());
        assert_eq!(ed.draw_lines.len(), 12, "its edges");
        assert_eq!(ed.hint, "Box 2 × 1.5 × [3] m");
        // The third click keeps it, selected; one undo step takes it and its group back.
        press_release(&mut ed, &mut world, second);
        assert!(ed.draw.is_none());
        assert_eq!(ed.selection, [e]);
        assert_eq!(ed.history.steps().count(), 1, "one step");
        ed.run(&mut world, ed.ids.undo);
        assert!(names(&mut world).is_empty());
    }

    #[test]
    fn shift_b_cuts_a_doorway_into_the_wall_under_the_pointer_in_its_group() {
        let mut world = World::new();
        let mut ed = draw_editor("draw-cut", &mut world);
        ed.add(&mut world, FIRST_BRUSH);
        let wall = ed.active().unwrap();
        let group = world.parent(wall).unwrap();
        chord(&mut ed, &mut world, &[SHIFT, B]);
        assert_eq!(ed.draw.as_ref().map(|d| d.op), Some(BrushOp::Cut));
        // The wall's face toward +z under the pointer, a little off the grid.
        let at = on_view(&ed, Vec3::new(-0.5, 0.0, 1.0));
        ed.surfaced = Some(engine_render::pick::SurfaceResult {
            frame: 0,
            request: SurfaceRequest {
                x: 0,
                y: 0,
                transparent: false,
            },
            hit: Some(hit(Vec3::new(-0.52, 0.01, 1.0), Vec3::Z)),
        });
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        assert!(
            ed.surface_request.is_some(),
            "the surface under the pointer is asked for"
        );
        press_release(&mut ed, &mut world, at);
        // The pointer up the wall and along it to −x (its first direction there): 1 m
        // across, Tab, 2.1 m up, Enter; 0.3 m into the wall, Enter.
        let up = on_view(&ed, Vec3::new(-1.0, 1.0, 1.0));
        mouse(&mut ed, up, None);
        type_chars(&mut ed, "1");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[TAB]);
        type_chars(&mut ed, "2.1");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ENTER]);
        type_chars(&mut ed, "0.3");
        ed.update(&mut world);
        assert_eq!(ed.hint, "Box cut 1 × 2.1 × [0.3] m");
        chord(&mut ed, &mut world, &[ENTER]);
        assert!(ed.draw.is_none(), "Enter makes it");
        let cut = ed.active().unwrap();
        assert_eq!(world.parent(cut), Some(group), "in the wall's group");
        assert_eq!(world.get::<Name>(cut).unwrap().as_str(), "Box Cut");
        let brush = world.get::<Brush>(cut).unwrap();
        assert_eq!(brush.op, BrushOp::Cut);
        assert!((brush.size - Vec3::new(1.0, 0.3, 2.1)).length() < 1e-4);
        let t = *world.get::<Transform>(cut).unwrap();
        // Into the wall (its +y along −z), from the corner on the grid at (−0.5, 0, 1).
        assert!((t.rotation * Vec3::Y).distance(Vec3::NEG_Z) < 1e-4);
        assert!(
            t.translation.distance(Vec3::new(-1.0, 1.05, 1.0)) < 1e-4,
            "{}",
            t.translation
        );
    }

    #[test]
    fn escape_or_a_right_click_drops_a_drawing_and_the_right_button_flies_before_it() {
        let mut world = World::new();
        let mut ed = draw_editor("draw-cancel", &mut world);
        chord(&mut ed, &mut world, &[B]);
        // Before the first click the right button flies the camera; the drawing waits.
        let first = on_view(&ed, Vec3::ZERO);
        mouse(&mut ed, first, None);
        right_button(&mut ed, true);
        ed.update(&mut world);
        assert!(ed.camera_drag && ed.draw.is_some());
        right_button(&mut ed, false);
        ed.update(&mut world);
        // A brush drawn up to its height, then a right click: gone, nothing to undo.
        press_release(&mut ed, &mut world, first);
        let second = on_view(&ed, Vec3::new(2.0, 0.0, 2.0));
        mouse(&mut ed, second, None);
        ed.update(&mut world);
        press_release(&mut ed, &mut world, second);
        type_chars(&mut ed, "2");
        ed.update(&mut world);
        assert_eq!(names(&mut world).len(), 2);
        right_button(&mut ed, true);
        ed.update(&mut world);
        right_button(&mut ed, false);
        assert!(ed.draw.is_none());
        assert!(names(&mut world).is_empty());
        assert_eq!(ed.history.steps().count(), 0);
        // Escape before the first click ends it too.
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[B]);
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert!(ed.draw.is_none());
    }

    const H: (u16, u16) = (0x48, 0x23);

    #[test]
    fn ctrl_h_makes_brushes_rooms_and_solid_again_and_draws_rooms() {
        let mut world = World::new();
        let mut ed = draw_editor("rooms", &mut world);
        ed.add(&mut world, FIRST_BRUSH);
        let e = ed.active().unwrap();
        chord(&mut ed, &mut world, &[CTRL, H]);
        assert_eq!(world.get::<Brush>(e).unwrap().hollow, ROOM_WALLS);
        assert_eq!(kind_of(&world, e), (icons::ROOM, "Room"));
        chord(&mut ed, &mut world, &[CTRL, H]);
        assert_eq!(world.get::<Brush>(e).unwrap().hollow, 0.0, "solid again");
        ed.run(&mut world, ed.ids.undo);
        assert!(world.get::<Brush>(e).unwrap().is_room(), "undone: a room");
        // Drawn: B, Ctrl+H, a base and a height make a room called so.
        chord(&mut ed, &mut world, &[B]);
        chord(&mut ed, &mut world, &[CTRL, H]);
        let first = on_view(&ed, Vec3::new(3.0, 0.0, 0.0));
        mouse(&mut ed, first, None);
        ed.update(&mut world);
        press_release(&mut ed, &mut world, first);
        let second = on_view(&ed, Vec3::new(7.0, 0.0, 3.0));
        mouse(&mut ed, second, None);
        ed.update(&mut world);
        press_release(&mut ed, &mut world, second);
        type_chars(&mut ed, "3");
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ENTER]);
        let room = ed.active().unwrap();
        assert_eq!(world.get::<Name>(room).unwrap().as_str(), "Room");
        let brush = world.get::<Brush>(room).unwrap();
        assert_eq!(brush.hollow, ROOM_WALLS);
        assert!((brush.size - Vec3::new(4.0, 3.0, 3.0)).length() < 1e-4);
    }

    #[test]
    fn a_brushs_side_handle_drags_its_side_as_one_step_and_escape_takes_it_back() {
        let mut world = World::new();
        let mut ed = draw_editor("sides", &mut world);
        ed.add(&mut world, FIRST_BRUSH);
        let e = ed.active().unwrap();
        // Its global transform, as the runtime makes it.
        let placed = world.get::<Transform>(e).unwrap().to_affine();
        world.insert(e, GlobalTransform(placed));
        ed.update(&mut world);
        ed.update(&mut world);
        let size = world.get::<Brush>(e).unwrap().size;
        let plus_x = Side {
            axis: 0,
            positive: true,
        };
        let at = |ed: &Editor| {
            ed.handles
                .at
                .iter()
                .find(|(s, _)| *s == plus_x)
                .map(|&(_, at)| at)
                .expect("the +x side faces the camera")
        };
        let grab = at(&ed);
        mouse(&mut ed, grab.to_array(), None);
        ed.update(&mut world);
        assert_eq!(ed.hovered_side, Some(plus_x));
        assert!(ed.hovered_handle.is_none(), "before the gizmo's");
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        assert!(ed.side_drag.is_some());
        // 1 m further out along +x, in two moves: snapping on, whole 0.1 m steps.
        let out = |ed: &Editor, dx: f32| {
            let p = placed.transform_point3(Vec3::new(size.x * 0.5 + dx, size.y * 0.5, 0.0));
            on_view(ed, p)
        };
        for dx in [0.5, 1.0] {
            let to = out(&ed, dx);
            mouse(&mut ed, to, None);
            ed.update(&mut world);
        }
        let b = world.get::<Brush>(e).unwrap().size;
        assert!((b.x - (size.x + 1.0)).abs() < 1e-4, "{b}");
        let t = world.get::<Transform>(e).unwrap().translation;
        assert!(
            (t.x - (placed.translation.x + 0.5)).abs() < 1e-4,
            "the −x side stays: {t}"
        );
        let to = out(&ed, 1.0);
        mouse(&mut ed, to, Some(false));
        ed.update(&mut world);
        assert!(ed.side_drag.is_none());
        assert_eq!(ed.history.steps().count(), 2, "the add and the resize");
        // Another drag taken back with Escape.
        world.insert(
            e,
            GlobalTransform(world.get::<Transform>(e).unwrap().to_affine()),
        );
        ed.update(&mut world);
        let grab = at(&ed);
        mouse(&mut ed, grab.to_array(), None);
        ed.update(&mut world);
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        let to = (grab + Vec2::new(40.0, 0.0)).to_array();
        mouse(&mut ed, to, None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert!((world.get::<Brush>(e).unwrap().size.x - (size.x + 1.0)).abs() < 1e-4);
        mouse(&mut ed, to, Some(false));
        ed.update(&mut world);
        assert_eq!(ed.history.steps().count(), 2, "nothing more");
    }

    #[test]
    fn view_colliders_switches_the_colliders_overlay() {
        let mut world = World::new();
        let mut ed = draw_editor("colliders", &mut world);
        assert!(!ed.show_colliders, "off at first");
        let id = ed.ids.colliders;
        ed.run(&mut world, id);
        assert!(ed.show_colliders);
        assert!(
            ed.message
                .as_ref()
                .is_some_and(|m| m.0.starts_with("Colliders shown"))
        );
        ed.run(&mut world, id);
        assert!(!ed.show_colliders);
    }

    const ALT: (u16, u16) = (0x12, 0x38);

    /// A drag with the left button from world point `from` to `to` in the editor's view, a
    /// frame for each of the press, the move and the release.
    fn drag_world(ed: &mut Editor, world: &mut World, from: Vec3, to: Vec3) {
        let a = on_view(ed, from);
        mouse(ed, a, None);
        ed.update(world);
        mouse(ed, a, Some(true));
        ed.update(world);
        let b = on_view(ed, to);
        mouse(ed, b, None);
        ed.update(world);
        mouse(ed, b, Some(false));
        ed.update(world);
    }

    #[test]
    fn tab_edits_faces_a_drag_moves_one_and_alt_pulls_a_brush_out_or_pushes_a_cut_in() {
        let mut world = World::new();
        let mut ed = draw_editor("faces", &mut world);
        ed.add(&mut world, FIRST_BRUSH);
        let e = ed.active().unwrap();
        let placed = world.get::<Transform>(e).unwrap().to_affine();
        world.insert(e, GlobalTransform(placed));
        // Tab over the viewport (over the panels it goes from field to field).
        let over = on_view(&ed, Vec3::new(0.3, 2.0, 0.2));
        mouse(&mut ed, over, None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[TAB]);
        assert!(ed.face_edit.is_some(), "Tab edits the selection's faces");
        // The pointer over the 2 m box's top.
        let on_top = Vec3::new(0.3, 2.0, 0.2);
        let over = on_view(&ed, on_top);
        mouse(&mut ed, over, None);
        ed.update(&mut world);
        let edit = ed.face_edit.as_ref().unwrap();
        let Some(Hover::Face(lit)) = edit.hovered else {
            panic!("the top lights: {:?}", edit.hovered)
        };
        assert!(edit.faces[lit].normal.y > 0.9);
        assert!(ed.status.contains(FACES_HELP), "{}", ed.status);
        assert!(
            !ed.gizmo_shown && ed.handles.at.is_empty(),
            "the gizmo and handles hide"
        );
        // Dragged 1 m up (snapping on, 0.1 m steps): planes now, 3 m high, one step.
        drag_world(&mut ed, &mut world, on_top, on_top + Vec3::Y);
        let b = world.get::<Brush>(e).unwrap();
        assert_eq!(b.shape, BrushShape::Planes);
        assert!(
            b.planes
                .iter()
                .any(|q| q.normal == [0, 1, 0] && q.offset == 3000),
            "{:?}",
            b.planes
        );
        assert_eq!(ed.history.steps().count(), 2, "the add and the move");
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(
            world.get::<Brush>(e).unwrap().shape,
            BrushShape::Box,
            "undone: a box"
        );
        // Alt+drag: a brush pulled out of the top, 0.5 m.
        key(&mut ed, ALT.0, ALT.1, true);
        drag_world(&mut ed, &mut world, on_top, on_top + Vec3::Y * 0.5);
        key(&mut ed, ALT.0, ALT.1, false);
        let pulled = engine_scene::find_named(&mut world, "Extrusion").unwrap();
        let b = world.get::<Brush>(pulled).unwrap();
        assert_eq!((b.op, b.shape), (BrushOp::Add, BrushShape::Planes));
        assert!(
            b.planes
                .iter()
                .any(|q| q.normal == [0, 1, 0] && q.offset == 2500)
        );
        assert_eq!(world.parent(pulled), world.parent(e), "in the same group");
        assert!(
            ed.face_edit.as_ref().unwrap().brushes.contains(&pulled),
            "edited too"
        );
        // Pushed in 0.3 m: a cut of the top's outline.
        key(&mut ed, ALT.0, ALT.1, true);
        drag_world(&mut ed, &mut world, on_top, on_top - Vec3::Y * 0.3);
        key(&mut ed, ALT.0, ALT.1, false);
        let cut = engine_scene::find_named(&mut world, "Cut").unwrap();
        assert_eq!(world.get::<Brush>(cut).unwrap().op, BrushOp::Cut);
        // Escape ends the editing.
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert!(ed.face_edit.is_none());
    }

    #[test]
    fn an_edge_dragged_onto_another_makes_a_wedge_as_one_step() {
        let mut world = World::new();
        let mut ed = draw_editor("edges", &mut world);
        // Looking from the front and a little above: an edge moves in the plane across z.
        ed.orbit.pitch = 0.3;
        ed.add(&mut world, FIRST_BRUSH);
        let e = ed.active().unwrap();
        let placed = world.get::<Transform>(e).unwrap().to_affine();
        world.insert(e, GlobalTransform(placed));
        let over = on_view(&ed, Vec3::new(0.3, 1.0, 1.0));
        mouse(&mut ed, over, None);
        ed.update(&mut world);
        chord(&mut ed, &mut world, &[TAB]);
        // The top's front edge (y = 2 m, z = +1 m): lit.
        let edge = Vec3::new(0.0, 2.0, 1.0);
        let at = on_view(&ed, edge);
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        let edit = ed.face_edit.as_ref().unwrap();
        assert!(
            matches!(edit.hovered, Some(Hover::Edge(..))),
            "{:?}",
            edit.hovered
        );
        assert_eq!(edit.points[0].grid.len(), 8, "its corners show");
        // Down 2 m onto the bottom's front edge: a wedge, five faces, one step.
        drag_world(&mut ed, &mut world, edge, edge - Vec3::Y * 2.0);
        let b = world.get::<Brush>(e).unwrap();
        assert_eq!(b.shape, BrushShape::Planes);
        assert_eq!(b.planes.len(), 5, "{:?}", b.planes);
        assert_eq!(ed.history.steps().count(), 2, "the add and the move");
        ed.run(&mut world, ed.ids.undo);
        assert_eq!(world.get::<Brush>(e).unwrap().shape, BrushShape::Box);
    }

    #[test]
    fn g_switches_snapping_in_our_keys_and_y_is_free() {
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
        let before = ed.snapping.on;
        chord(&mut ed, &mut world, &[G]);
        assert_ne!(ed.snapping.on, before, "G switches it");
        chord(&mut ed, &mut world, &[Y]);
        assert_ne!(ed.snapping.on, before, "Y leaves it");
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
        // The menu shows its kinds' icons and its groups' (each group's opens beside it on
        // hover).
        for glyph in [
            icons::NODE_3D,
            icons::MODEL,
            icons::BLOCKOUT,
            icons::LIGHT,
            icons::CAMERA,
        ] {
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

    /// An editor with `world`'s entity `e` selected, the gizmo laid out.
    fn gizmo_editor(name: &str, world: &mut World, e: Entity) -> Editor {
        let mut ed = Editor::new(
            [1280, 720],
            folder(name),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.select_only(e);
        ed.update(world);
        ed.update(world);
        assert!(ed.gizmo_shown, "the gizmo is shown for the selection");
        ed
    }

    /// The x arrow on the screen: a point on it and its direction.
    fn x_arrow(ed: &Editor) -> (Vec2, Vec2) {
        let (_, tip, dir) = ed
            .gizmo
            .tips
            .iter()
            .copied()
            .find(|t| t.0 == Handle::Axis(0))
            .unwrap();
        (tip - dir * 30.0, dir)
    }

    #[test]
    fn the_gizmo_moves_the_selection_along_an_axis_as_one_step() {
        let mut world = World::new();
        let cube = world.spawn((
            SceneId::new(),
            Name::new("Cube"),
            Transform::IDENTITY,
            GlobalTransform::default(),
        ));
        let mut ed = gizmo_editor("gizmo-move", &mut world, cube);
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), None);
        ed.update(&mut world);
        assert_eq!(ed.hovered_handle, Some(Handle::Axis(0)));
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        assert!(ed.gizmo_drag.is_some(), "the press takes the arrow");
        mouse(&mut ed, (grab + dir * 50.0).to_array(), None);
        ed.update(&mut world);
        let t = *world.get::<Transform>(cube).unwrap();
        assert!(t.translation.x > 0.1, "{:?}", t.translation);
        assert_eq!(
            (t.translation.y, t.translation.z),
            (0.0, 0.0),
            "along x only"
        );
        // The status bar shows it from the next frame (it is built before the gizmo's).
        ed.update(&mut world);
        assert!(ed.status.starts_with("Move ("), "{}", ed.status);
        mouse(&mut ed, (grab + dir * 80.0).to_array(), None);
        ed.update(&mut world);
        mouse(&mut ed, (grab + dir * 80.0).to_array(), Some(false));
        ed.update(&mut world);
        assert!(ed.gizmo_drag.is_none(), "the release ends it");
        assert_eq!(ed.history.steps().count(), 1, "one step for the drag");
        assert_eq!(
            world.get::<Transform>(cube).unwrap().rotation,
            Quat::IDENTITY,
            "a move keeps the rotation exact"
        );
        let undo = ed.ids.undo;
        ed.run(&mut world, undo);
        assert_eq!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::ZERO
        );
    }

    #[test]
    fn escape_takes_a_gizmo_drag_back_and_ctrl_rounds_to_the_grid() {
        let mut world = World::new();
        let cube = world.spawn((
            SceneId::new(),
            Transform::from_xyz(0.03, 0.0, 0.0),
            GlobalTransform(engine_core::math::Affine3A::from_translation(Vec3::new(
                0.03, 0.0, 0.0,
            ))),
        ));
        let mut ed = gizmo_editor("gizmo-escape", &mut world, cube);
        let x = |world: &World| world.get::<Transform>(cube).unwrap().translation.x;
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        mouse(&mut ed, (grab + dir * 40.0).to_array(), None);
        ed.update(&mut world);
        assert!(x(&world) > 0.1);
        key(&mut ed, 0x1B, 0x01, true);
        ed.update(&mut world);
        assert_eq!(x(&world), 0.03, "Escape: back where it was");
        mouse(&mut ed, (grab + dir * 70.0).to_array(), None);
        ed.update(&mut world);
        assert_eq!(
            x(&world),
            0.03,
            "the cancelled gesture changes nothing more"
        );
        mouse(&mut ed, (grab + dir * 70.0).to_array(), Some(false));
        ed.update(&mut world);
        assert_eq!(ed.history.steps().count(), 0);
        // Ctrl: the center lands on the grid (steps of 0.1).
        key(&mut ed, 0x1B, 0x01, false);
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        key(&mut ed, 0x11, 0x1D, true);
        mouse(&mut ed, (grab + dir * 47.0).to_array(), None);
        ed.update(&mut world);
        let snapped = x(&world);
        assert!(snapped > 0.1, "{snapped}");
        assert!(
            (snapped * 10.0 - (snapped * 10.0).round()).abs() < 1e-4,
            "on the grid: {snapped}"
        );
        key(&mut ed, 0x11, 0x1D, false);
        mouse(&mut ed, (grab + dir * 47.0).to_array(), Some(false));
        ed.update(&mut world);
    }

    #[test]
    fn a_turn_keeps_the_scale_exact_and_the_tools_change_the_handles() {
        let mut world = World::new();
        let cube = world.spawn((
            SceneId::new(),
            Transform::IDENTITY,
            GlobalTransform::default(),
        ));
        let mut ed = gizmo_editor("gizmo-turn", &mut world, cube);
        // Shift+R: the rotate tool, rings only.
        chord(&mut ed, &mut world, &[SHIFT, R]);
        ed.update(&mut world);
        assert_eq!(ed.active_tool, 2);
        assert!(ed.gizmo.tips.is_empty() && ed.gizmo.quads.is_empty());
        // The ring about y: grabbed at one of its pieces, dragged along it.
        let (_, a, b) = ed
            .gizmo
            .lines
            .iter()
            .copied()
            .find(|l| l.0 == Handle::Ring(1))
            .unwrap();
        let at = (a + b) * 0.5;
        mouse(&mut ed, at.to_array(), Some(true));
        ed.update(&mut world);
        assert_eq!(
            ed.gizmo_drag.as_ref().map(|d| d.handle),
            Some(Handle::Ring(1))
        );
        mouse(&mut ed, (at + (b - a).normalize() * 60.0).to_array(), None);
        ed.update(&mut world);
        let t = *world.get::<Transform>(cube).unwrap();
        let (axis, angle) = t.rotation.to_axis_angle();
        assert!(angle.abs() > 0.05, "turned: {angle}");
        assert!(axis.y.abs() > 0.999, "about y: {axis}");
        assert_eq!(t.scale, Vec3::ONE, "a turn keeps the scale exact");
        mouse(
            &mut ed,
            (at + (b - a).normalize() * 60.0).to_array(),
            Some(false),
        );
        ed.update(&mut world);
        // Shift+E: the scale tool; again: all of them.
        chord(&mut ed, &mut world, &[SHIFT, E]);
        assert_eq!(ed.active_tool, 3);
        chord(&mut ed, &mut world, &[SHIFT, E]);
        assert_eq!(ed.active_tool, 0);
    }

    #[test]
    fn a_child_of_a_turned_parent_moves_along_the_worlds_axis() {
        let mut world = World::new();
        let turn = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        let parent_world = engine_core::math::Affine3A::from_rotation_translation(turn, Vec3::ZERO);
        let parent = world.spawn((
            SceneId::new(),
            Transform::IDENTITY.with_rotation(turn),
            GlobalTransform(parent_world),
        ));
        // Its local +x lies along the world's −z.
        let child_local = engine_core::math::Affine3A::from_translation(Vec3::X);
        let child = world.spawn((
            SceneId::new(),
            Transform::from_xyz(1.0, 0.0, 0.0),
            GlobalTransform(parent_world * child_local),
        ));
        world.set_parent(child, Some(parent)).unwrap();
        let mut ed = gizmo_editor("gizmo-child", &mut world, child);
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        mouse(&mut ed, (grab + dir * 50.0).to_array(), None);
        ed.update(&mut world);
        // The world's +x is the parent's +z: the child's local z grows, its x stays.
        let t = *world.get::<Transform>(child).unwrap();
        assert!((t.translation.x - 1.0).abs() < 1e-4, "{:?}", t.translation);
        assert!(t.translation.z > 0.1, "{:?}", t.translation);
        mouse(&mut ed, (grab + dir * 50.0).to_array(), Some(false));
        ed.update(&mut world);
    }

    /// Characters typed, as the keyboard layout gives them.
    fn type_chars(ed: &mut Editor, text: &str) {
        for c in text.chars() {
            event(ed, WindowEventKind::Char(c));
        }
    }

    fn right_button(ed: &mut Editor, pressed: bool) {
        event(
            ed,
            WindowEventKind::MouseButton {
                button: MouseButton::Right,
                pressed,
            },
        );
    }

    // More keys: virtual key and scan code.
    const G: (u16, u16) = (0x47, 0x22);
    const Q: (u16, u16) = (0x51, 0x10);
    const E: (u16, u16) = (0x45, 0x12);
    const W: (u16, u16) = (0x57, 0x11);
    const R: (u16, u16) = (0x52, 0x13);
    const S: (u16, u16) = (0x53, 0x1F);
    const X: (u16, u16) = (0x58, 0x2D);
    const Y: (u16, u16) = (0x59, 0x15);
    const Z: (u16, u16) = (0x5A, 0x2C);

    fn cube_at(world: &mut World, at: Transform) -> Entity {
        world.spawn((
            SceneId::new(),
            Name::new("Cube"),
            at,
            GlobalTransform(at.to_affine()),
        ))
    }

    #[test]
    fn q_z_and_a_typed_value_move_the_selection_exactly_as_one_step() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("keys-move", &mut world, cube);
        mouse(&mut ed, [700.0, 300.0], None);
        chord(&mut ed, &mut world, &[Q]);
        assert!(ed.key_transform.is_some(), "Q starts a move");
        ed.update(&mut world);
        assert!(!ed.gizmo_shown, "the gizmo is hidden meanwhile");
        chord(&mut ed, &mut world, &[Z]);
        type_chars(&mut ed, "25");
        ed.update(&mut world);
        assert_eq!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::new(0.0, 0.0, 25.0)
        );
        chord(&mut ed, &mut world, &[BACKSPACE]);
        assert_eq!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::new(0.0, 0.0, 2.0)
        );
        assert_eq!(ed.hint, "Move along Z: 2 m");
        assert!(ed.status.starts_with(transform::HELP), "{}", ed.status);
        assert_eq!(ed.guides.len(), 1, "the z axis' line");
        chord(&mut ed, &mut world, &[ENTER]);
        assert!(ed.key_transform.is_none(), "Enter keeps it");
        assert_eq!(ed.history.steps().count(), 1, "one step");
        let t = *world.get::<Transform>(cube).unwrap();
        assert_eq!(t.translation, Vec3::new(0.0, 0.0, 2.0));
        assert_eq!((t.rotation, t.scale), (Quat::IDENTITY, Vec3::ONE));
        ed.update(&mut world);
        assert!(ed.gizmo_shown, "the gizmo is back");
        let undo = ed.ids.undo;
        ed.run(&mut world, undo);
        assert_eq!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::ZERO
        );
    }

    #[test]
    fn r_and_e_turn_and_scale_by_typed_values_with_exact_parts() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::from_xyz(1.0, 0.0, 0.0));
        let mut ed = gizmo_editor("keys-turn", &mut world, cube);
        mouse(&mut ed, [700.0, 300.0], None);
        // R Z 90: a quarter turn about z, about the cube's own center.
        chord(&mut ed, &mut world, &[R]);
        chord(&mut ed, &mut world, &[Z]);
        type_chars(&mut ed, "90");
        chord(&mut ed, &mut world, &[ENTER]);
        let t = *world.get::<Transform>(cube).unwrap();
        assert!(
            (t.rotation * Vec3::X - Vec3::Y).length() < 1e-6,
            "{}",
            t.rotation
        );
        assert_eq!(t.scale, Vec3::ONE, "a turn keeps the scale exact");
        assert_eq!(t.translation, Vec3::new(1.0, 0.0, 0.0), "about its center");
        // The world's transforms follow the scene's for the next transform.
        world.insert(cube, GlobalTransform(t.to_affine()));
        // E Shift+Z 0.5: halved across z, the rotation exact.
        chord(&mut ed, &mut world, &[E]);
        chord(&mut ed, &mut world, &[SHIFT, Z]);
        type_chars(&mut ed, "0,5");
        ed.update(&mut world);
        assert_eq!(ed.hint, "Scale on XY: ×0.5");
        chord(&mut ed, &mut world, &[ENTER]);
        let s = *world.get::<Transform>(cube).unwrap();
        assert_eq!(s.scale, Vec3::new(0.5, 0.5, 1.0));
        assert_eq!(s.rotation, t.rotation, "a scale keeps the rotation exact");
        assert_eq!(ed.history.steps().count(), 2);
    }

    #[test]
    fn the_pointer_moves_and_escape_a_right_click_or_focus_loss_take_it_back() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("keys-escape", &mut world, cube);
        let at = |world: &World| world.get::<Transform>(cube).unwrap().translation;
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), None);
        chord(&mut ed, &mut world, &[Q]);
        chord(&mut ed, &mut world, &[X]);
        mouse(&mut ed, (grab + dir * 50.0).to_array(), None);
        ed.update(&mut world);
        let moved = at(&world);
        assert!(moved.x > 0.1, "{moved}");
        assert_eq!((moved.y, moved.z), (0.0, 0.0), "along x only");
        assert!(ed.hint.starts_with("Move along X: "), "{}", ed.hint);
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert!(ed.key_transform.is_none());
        assert_eq!(at(&world), Vec3::ZERO, "Escape: back where it was");
        assert_eq!(ed.history.steps().count(), 0);
        // A right click: taken back, and the camera does not look around.
        chord(&mut ed, &mut world, &[Q]);
        mouse(&mut ed, (grab + dir * 80.0).to_array(), None);
        ed.update(&mut world);
        assert_ne!(at(&world), Vec3::ZERO, "free: in the view's plane");
        right_button(&mut ed, true);
        ed.update(&mut world);
        assert!(ed.key_transform.is_none() && !ed.camera_drag);
        assert_eq!(at(&world), Vec3::ZERO);
        right_button(&mut ed, false);
        ed.update(&mut world);
        // The window losing the keyboard.
        chord(&mut ed, &mut world, &[Q]);
        mouse(&mut ed, (grab + dir * 30.0).to_array(), None);
        ed.update(&mut world);
        event(&mut ed, WindowEventKind::Focused(false));
        ed.update(&mut world);
        assert!(ed.key_transform.is_none());
        assert_eq!(at(&world), Vec3::ZERO);
        assert_eq!(ed.history.steps().count(), 0);
    }

    #[test]
    fn a_left_click_keeps_it_and_grabs_no_handle() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("keys-click", &mut world, cube);
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), None);
        chord(&mut ed, &mut world, &[Q]);
        chord(&mut ed, &mut world, &[X]);
        // The gizmo moves with the cube: the pointer stays on its x arrow.
        mouse(&mut ed, (grab + dir * 40.0).to_array(), None);
        ed.update(&mut world);
        mouse(&mut ed, (grab + dir * 40.0).to_array(), Some(true));
        ed.update(&mut world);
        assert!(ed.key_transform.is_none(), "the click keeps it");
        assert!(ed.gizmo_drag.is_none(), "and is not the gizmo's");
        assert!(world.get::<Transform>(cube).unwrap().translation.x > 0.1);
        mouse(&mut ed, (grab + dir * 40.0).to_array(), Some(false));
        ed.update(&mut world);
        assert_eq!(ed.history.steps().count(), 1);
        assert_eq!(ed.selection, [cube], "the selection stays");
    }

    #[test]
    fn godots_keys_pick_the_scale_tool_with_r_and_switch_a_move_to_a_turn() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("keys-godot", &mut world, cube);
        ed.use_keys(KeyPreset::Godot);
        chord(&mut ed, &mut world, &[R]);
        assert_eq!(ed.active_tool, 3, "R: the scale tool");
        assert!(ed.key_transform.is_none());
        chord(&mut ed, &mut world, &[S]);
        assert!(ed.key_transform.is_none(), "S is free in Godot's keys");
        // Q: the arrows again.
        chord(&mut ed, &mut world, &[Q]);
        ed.update(&mut world);
        let (grab, dir) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), None);
        chord(&mut ed, &mut world, &[G]);
        mouse(&mut ed, (grab + dir * 50.0).to_array(), None);
        ed.update(&mut world);
        assert_ne!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::ZERO
        );
        // R during it: the move taken back, a turn from here.
        chord(&mut ed, &mut world, &[R]);
        assert_eq!(
            ed.key_transform.as_ref().map(|t| t.kind),
            Some(transform::Kind::Turn)
        );
        assert_eq!(
            world.get::<Transform>(cube).unwrap().translation,
            Vec3::ZERO
        );
        chord(&mut ed, &mut world, &[Y]);
        type_chars(&mut ed, "-90");
        chord(&mut ed, &mut world, &[ENTER]);
        let t = *world.get::<Transform>(cube).unwrap();
        assert!(
            (t.rotation * Vec3::X - Vec3::Z).length() < 1e-6,
            "{}",
            t.rotation
        );
        assert_eq!(ed.history.steps().count(), 1, "the turn only");
        // Blender's keys again: R turns, T picks the scale tool.
        ed.use_keys(KeyPreset::Blender);
        assert_eq!(
            ed.shortcuts.get(&ed.ids.tools[3]).map(String::as_str),
            Some("T")
        );
        assert_eq!(
            ed.shortcuts.get(&ed.ids.transforms[1]).map(String::as_str),
            Some("R")
        );
    }

    #[test]
    fn the_key_map_is_picked_in_the_palette_and_kept() {
        let dir = project("keys-kept");
        let (mut world, mut ed) = start(&dir);
        assert_eq!(ed.key_preset, KeyPreset::Ofelia, "ours by default");
        let keys = ed.ids.keys;
        ed.run(&mut world, keys);
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
        assert_eq!(labels, KeyPreset::ALL.map(|p| String::from(p.label())));
        ed.palette_action(&mut world, PaletteAction::Run(2));
        assert_eq!(ed.key_preset, KeyPreset::Godot);
        // The flight's speed is kept too.
        ed.fly_speed = 7.5;
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert_eq!(ed.key_preset, KeyPreset::Godot);
        assert_eq!(ed.fly_speed, 7.5);
        assert_eq!(
            ed.shortcuts.get(&ed.ids.tools[3]).map(String::as_str),
            Some("R")
        );
        assert_eq!(ed.shortcuts.get(&ed.ids.transforms[1]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn our_keys_start_transforms_with_q_e_r_and_pick_the_handles_with_shift() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("keys-ours", &mut world, cube);
        let kind = |ed: &Editor| ed.key_transform.as_ref().map(|t| t.kind);
        mouse(&mut ed, [700.0, 300.0], None);
        chord(&mut ed, &mut world, &[E]);
        assert_eq!(kind(&ed), Some(transform::Kind::Scale), "E scales");
        chord(&mut ed, &mut world, &[R]);
        assert_eq!(
            kind(&ed),
            Some(transform::Kind::Turn),
            "R during it: a turn"
        );
        chord(&mut ed, &mut world, &[Q]);
        assert_eq!(kind(&ed), Some(transform::Kind::Move), "Q: a move");
        chord(&mut ed, &mut world, &[ESCAPE]);
        assert_eq!(kind(&ed), None);
        // G and S are free in our keys.
        chord(&mut ed, &mut world, &[G]);
        chord(&mut ed, &mut world, &[S]);
        assert_eq!(kind(&ed), None);
        // Shift+Q: the arrows; again: all the handles.
        chord(&mut ed, &mut world, &[SHIFT, Q]);
        assert_eq!(ed.active_tool, 1);
        chord(&mut ed, &mut world, &[SHIFT, Q]);
        assert_eq!(ed.active_tool, 0);
        assert_eq!(ed.history.steps().count(), 0);
    }

    #[test]
    fn the_right_button_flies_the_camera_with_wasd_and_the_wheel_sets_its_speed() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("fly", &mut world, cube);
        let [x, y, w, h] = ed.view_rect.unwrap();
        mouse(&mut ed, [(x + w / 2) as f32, (y + h / 2) as f32], None);
        right_button(&mut ed, true);
        ed.update(&mut world);
        assert!(ed.camera_drag);
        // A frame of a second, counted as a tenth: a stall does not throw the camera.
        let long_frame = |ed: &mut Editor, world: &mut World| {
            ed.last_frame = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            ed.update(world);
        };
        let eye = ed.orbit.eye();
        let ahead = (ed.orbit.target - eye).normalize();
        key(&mut ed, W.0, W.1, true);
        long_frame(&mut ed, &mut world);
        key(&mut ed, W.0, W.1, false);
        let moved = ed.orbit.eye() - eye;
        assert!((moved - ahead * FLY_SPEED * 0.1).length() < 1e-3, "{moved}");
        // E up, three times as fast with Shift; the letters are the flight's, not shortcuts.
        let eye = ed.orbit.eye();
        key(&mut ed, SHIFT.0, SHIFT.1, true);
        key(&mut ed, E.0, E.1, true);
        long_frame(&mut ed, &mut world);
        key(&mut ed, E.0, E.1, false);
        key(&mut ed, SHIFT.0, SHIFT.1, false);
        let up = ed.orbit.eye() - eye;
        assert!(
            (up - Vec3::Y * FLY_SPEED * FLY_FAST * 0.1).length() < 1e-3,
            "{up}"
        );
        assert!(ed.key_transform.is_none() && ed.active_tool == 0);
        // The wheel sets the speed while flying, and zooms again after.
        let distance = ed.orbit.distance;
        event(&mut ed, WindowEventKind::Wheel { x: 0.0, y: 1.0 });
        ed.update(&mut world);
        ed.update(&mut world);
        assert!((ed.fly_speed - FLY_SPEED * FLY_NOTCH).abs() < 1e-4);
        assert_eq!(ed.orbit.distance, distance);
        assert!(ed.status.starts_with("Flying at 6.0 m/s"), "{}", ed.status);
        right_button(&mut ed, false);
        ed.update(&mut world);
        event(&mut ed, WindowEventKind::Wheel { x: 0.0, y: 1.0 });
        ed.update(&mut world);
        assert!(ed.orbit.distance < distance, "the wheel zooms");
    }

    /// A click at `at` (with Shift held, `shift`), answered by a pick that saw `slot`:
    /// `slots` gives the entities' slots, as the extraction does.
    fn view_click(
        ed: &mut Editor,
        world: &mut World,
        at: [f32; 2],
        shift: bool,
        slot: Option<u32>,
        slots: impl Fn(Entity) -> Option<u32>,
    ) {
        if shift {
            key(ed, SHIFT.0, SHIFT.1, true);
        }
        mouse(ed, at, Some(true));
        ed.update(world);
        mouse(ed, at, Some(false));
        ed.update(world);
        if shift {
            key(ed, SHIFT.0, SHIFT.1, false);
        }
        let (request, toggle, from) = ed.pending_click.expect("a click");
        assert_eq!(toggle, shift);
        assert_eq!(ed.pick_request, Some(request), "its pixel is picked");
        ed.picked = Some((request, from, slot));
        ed.resolve_pick(world, slots);
        assert!(ed.pending_click.is_none(), "answered");
    }

    #[test]
    fn a_click_in_the_viewport_selects_what_its_pick_saw() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let lamp = cube_at(&mut world, Transform::from_xyz(3.0, 0.0, 0.0));
        let slots = move |e: Entity| (e == cube).then_some(7).or((e == lamp).then_some(9));
        let mut ed = Editor::new(
            [1280, 720],
            folder("pick-click"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.update(&mut world);
        let [x, y, w, h] = ed.view_rect.unwrap();
        // A quarter of the way in: away from the gizmo, which sits in the middle.
        let at = [(x + w / 4) as f32, (y + h / 4) as f32];
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        let request = ed.pick_request.expect("the pointer's pixel is picked");
        assert_eq!(
            (request.x, request.y),
            (w / 4, h / 4),
            "in the view's pixels"
        );
        // The overlay drew it; its pick saw the cube.
        ed.picked = Some((request, 1, Some(7)));
        ed.resolve_pick(&world, slots);
        assert_eq!(ed.hovered, Some((request, Some(cube))), "hovered");
        ed.update(&mut world);
        assert!(
            ed.pick_request.is_none(),
            "nothing changed: not picked again"
        );
        // A click: answered only by a pick from the frame after it.
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, at, Some(false));
        ed.update(&mut world);
        let (request, _, from) = ed.pending_click.unwrap();
        ed.picked = Some((request, from - 1, Some(9)));
        ed.resolve_pick(&world, slots);
        assert!(
            ed.pending_click.is_some(),
            "an older pick does not answer it"
        );
        ed.picked = Some((request, from, Some(7)));
        ed.resolve_pick(&world, slots);
        assert_eq!(ed.selection, [cube]);
        // Shift: the lamp added, then the cube taken out.
        view_click(&mut ed, &mut world, at, true, Some(9), slots);
        assert_eq!(ed.selection, [cube, lamp]);
        view_click(&mut ed, &mut world, at, true, Some(7), slots);
        assert_eq!(ed.selection, [lamp]);
        // On nothing: with Shift the selection stays, without it goes.
        view_click(&mut ed, &mut world, at, true, None, slots);
        assert_eq!(ed.selection, [lamp]);
        view_click(&mut ed, &mut world, at, false, None, slots);
        assert!(ed.selection.is_empty());
        // A plain click selects only what it saw.
        view_click(&mut ed, &mut world, at, false, Some(9), slots);
        assert_eq!(ed.selection, [lamp]);
    }

    /// A press and a release at `at`, a frame each.
    fn press_release(ed: &mut Editor, world: &mut World, at: [f32; 2]) {
        mouse(ed, at, Some(true));
        ed.update(world);
        mouse(ed, at, Some(false));
        ed.update(world);
    }

    /// Answers the list a waiting click asked for with `slots`, front to back (`slot_of`
    /// gives the entities' slots, as the extraction does), and tells what it was for.
    fn answer_list(
        ed: &mut Editor,
        world: &World,
        slots: &[u32],
        slot_of: impl Fn(Entity) -> Option<u32>,
    ) -> ListUse {
        let (request, used, from) = ed.pending_list.expect("a list asked for");
        assert_eq!(ed.list_request, Some(request), "its pixel is listed");
        assert_eq!(
            request,
            ListRequest::around(
                request.rect[0] + PICK_RADIUS,
                request.rect[1] + PICK_RADIUS,
                PICK_RADIUS,
                request.transparent
            )
        );
        ed.listed = Some((request, from, slots.to_vec()));
        ed.resolve_list(world, slot_of);
        assert!(ed.pending_list.is_none(), "answered");
        used
    }

    /// Three cubes under the same spot, nearest first, at slots 5, 6 and 7; an editor; the
    /// spot, a quarter of the way into the view (away from the gizmo), hovered.
    fn under_one_spot(name: &str) -> (World, Editor, [Entity; 3], [f32; 2]) {
        let mut world = World::new();
        let cubes = ["Front", "Middle", "Back"].map(|n| {
            let e = cube_at(&mut world, Transform::IDENTITY);
            world.insert(e, Name::new(n));
            e
        });
        let mut ed = Editor::new(
            [1280, 720],
            folder(name),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.update(&mut world);
        let [x, y, w, h] = ed.view_rect.unwrap();
        let at = [(x + w / 4) as f32, (y + h / 4) as f32];
        mouse(&mut ed, at, None);
        ed.update(&mut world);
        (world, ed, cubes, at)
    }

    #[test]
    fn clicking_again_at_the_same_spot_cycles_through_what_lies_under_it() {
        let (mut world, mut ed, [front, middle, back], at) = under_one_spot("pick-cycle");
        let slots = move |e: Entity| {
            [front, middle, back]
                .iter()
                .position(|&x| x == e)
                .map(|i| i as u32 + 5)
        };
        // The first click takes the nearest surface's.
        view_click(&mut ed, &mut world, at, false, Some(5), slots);
        assert_eq!(ed.selection, [front]);
        // Again there: the one behind, the farthest, then round to the nearest.
        for expected in [middle, back, front] {
            press_release(&mut ed, &mut world, at);
            assert!(ed.pending_click.is_none(), "not a pick of the nearest");
            assert_eq!(
                answer_list(&mut ed, &world, &[5, 6, 7], slots),
                ListUse::Cycle
            );
            assert_eq!(ed.selection, [expected]);
        }
        // An active entity no longer under the spot: the nearest.
        press_release(&mut ed, &mut world, at);
        answer_list(&mut ed, &world, &[6, 7], slots);
        assert_eq!(ed.selection, [middle]);
        // Elsewhere: a plain click.
        let elsewhere = [at[0] + 40.0, at[1]];
        mouse(&mut ed, elsewhere, None);
        ed.update(&mut world);
        view_click(&mut ed, &mut world, elsewhere, false, Some(7), slots);
        assert_eq!(ed.selection, [back]);
        // After a Shift+click a plain click does not cycle: it selects what it hits alone.
        view_click(&mut ed, &mut world, elsewhere, true, Some(5), slots);
        assert_eq!(ed.selection, [back, front]);
        view_click(&mut ed, &mut world, elsewhere, false, Some(7), slots);
        assert_eq!(ed.selection, [back]);
    }

    #[test]
    fn alt_click_offers_what_lies_under_the_pointer_in_the_palette() {
        let (mut world, mut ed, [front, middle, back], at) = under_one_spot("pick-offer");
        let slots = move |e: Entity| {
            [front, middle, back]
                .iter()
                .position(|&x| x == e)
                .map(|i| i as u32 + 5)
        };
        const ALT: (u16, u16) = (0x12, 0x38);
        key(&mut ed, ALT.0, ALT.1, true);
        press_release(&mut ed, &mut world, at);
        key(&mut ed, ALT.0, ALT.1, false);
        assert!(ed.pending_click.is_none());
        // The middle cube's surface is hidden by nothing here: the list's order is its own.
        assert_eq!(answer_list(&mut ed, &world, &[7, 5], slots), ListUse::Offer);
        assert!(ed.selection.is_empty(), "nothing picked yet");
        ed.update(&mut world);
        let cp = ed.command_palette.as_ref().expect("offered in the palette");
        let labels: Vec<&str> = cp
            .choices
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(labels, ["Back", "Front"], "nearest first");
        ed.palette_action(&mut world, PaletteAction::Run(1));
        assert_eq!(ed.selection, [front]);
        // Nothing there: said so, nothing offered.
        key(&mut ed, ALT.0, ALT.1, true);
        press_release(&mut ed, &mut world, at);
        key(&mut ed, ALT.0, ALT.1, false);
        answer_list(&mut ed, &world, &[], slots);
        ed.update(&mut world);
        assert!(ed.command_palette.is_none());
        assert_eq!(ed.selection, [front]);
    }

    /// A box dragged from `from` to `to` with `keys` held (virtual key and scan code),
    /// released, and its last box answered with `slots`, front to back (`slot_of` gives the
    /// entities' slots): the box asked for.
    fn boxed(
        ed: &mut Editor,
        world: &mut World,
        (from, to): ([f32; 2], [f32; 2]),
        keys: &[(u16, u16)],
        slots: &[u32],
        slot_of: impl Fn(Entity) -> Option<u32>,
    ) -> ListRequest {
        for &(vk, sc) in keys {
            key(ed, vk, sc, true);
        }
        mouse(ed, from, Some(true));
        ed.update(world);
        mouse(ed, to, None);
        ed.update(world);
        mouse(ed, to, Some(false));
        ed.update(world);
        let request = ed.list_request.expect("the last box asked for");
        let (_, after) = ed.box_drag.as_ref().unwrap().released.expect("released");
        ed.listed = Some((request, after, slots.to_vec()));
        ed.resolve_list(world, slot_of);
        assert!(ed.box_drag.is_none(), "applied");
        for &(vk, sc) in keys {
            key(ed, vk, sc, false);
        }
        request
    }

    #[test]
    fn a_box_takes_what_shows_in_it_by_its_modifiers() {
        let (mut world, mut ed, [front, middle, back], at) = under_one_spot("pick-box");
        let slots = move |e: Entity| {
            [front, middle, back]
                .iter()
                .position(|&x| x == e)
                .map(|i| i as u32 + 5)
        };
        let to = [at[0] + 60.0, at[1] + 40.0];
        // A press, then the pointer past 4 pixels: a box.
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, to, None);
        ed.update(&mut world);
        let asked = ed.list_request.expect("what it takes is asked for");
        assert!(asked.visible, "what shows in it");
        assert!(asked.rect[0] < asked.rect[2] && asked.rect[1] < asked.rect[3]);
        // One box at a time: moved again before the answer, nothing asked.
        let further = [to[0] + 5.0, to[1]];
        mouse(&mut ed, further, None);
        ed.update(&mut world);
        assert!(
            ed.list_request.is_none(),
            "the first one's answer is to come"
        );
        // Its answer is outlined as the box would take it; then the box as it is is asked.
        ed.listed = Some((asked, 0, vec![6, 5]));
        ed.resolve_list(&world, slots);
        assert_eq!(ed.box_drag.as_ref().unwrap().taken, [middle, front]);
        ed.outline(&world, slots);
        let item = |slot| OutlineItem {
            slot,
            class: OutlineClass::Hovered,
        };
        assert_eq!(ed.outline_items, [item(6), item(5)]);
        ed.update(&mut world);
        assert!(
            ed.list_request.is_some_and(|r| r != asked),
            "the box as it is now"
        );
        // Released: its last box is asked again, and its answer replaces the selection,
        // the nearest active.
        mouse(&mut ed, further, Some(false));
        ed.update(&mut world);
        let last = ed.list_request.expect("the last box");
        let (mode, after) = ed.box_drag.as_ref().unwrap().released.unwrap();
        assert_eq!(mode, BoxMode::Replace);
        ed.listed = Some((last, after, vec![6, 5]));
        ed.resolve_list(&world, slots);
        assert!(ed.box_drag.is_none());
        assert!(ed.pending_click.is_none(), "no click");
        assert_eq!(ed.selection, [front, middle]);
        // Shift adds, Ctrl takes out, Alt takes hidden objects too.
        boxed(&mut ed, &mut world, (at, to), &[SHIFT], &[7], slots);
        assert_eq!(ed.selection, [front, middle, back]);
        boxed(&mut ed, &mut world, (at, to), &[CTRL], &[5, 7], slots);
        assert_eq!(ed.selection, [middle]);
        const ALT: (u16, u16) = (0x12, 0x38);
        let hidden = boxed(&mut ed, &mut world, (at, to), &[ALT], &[5, 6, 7], slots);
        assert!(!hidden.visible);
        assert_eq!(ed.selection, [back, middle, front]);
        // Escape takes a box back: nothing changes, and no click.
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, to, None);
        ed.update(&mut world);
        assert!(ed.box_drag.is_some());
        key(&mut ed, 0x1B, 0x01, true);
        ed.update(&mut world);
        key(&mut ed, 0x1B, 0x01, false);
        assert!(ed.box_drag.is_none(), "taken back");
        mouse(&mut ed, to, Some(false));
        ed.update(&mut world);
        assert!(ed.box_drag.is_none() && ed.pending_click.is_none());
        assert_eq!(ed.selection, [back, middle, front]);
    }

    /// Ends the camera's move at once: the next update puts the camera where it goes.
    fn settle(ed: &mut Editor, world: &mut World) {
        if let Some((_, _, since)) = &mut ed.camera_move {
            *since -= std::time::Duration::from_secs(1);
        }
        ed.update(world);
        assert!(ed.camera_move.is_none(), "settled");
    }

    #[test]
    fn f_frames_the_selection_or_the_scene() {
        let mut world = World::new();
        let near = cube_at(&mut world, Transform::from_xyz(1.0, 0.0, 0.0));
        let child = cube_at(&mut world, Transform::from_xyz(3.0, 2.0, 0.0));
        world.set_parent(child, Some(near)).unwrap();
        let mut ed = Editor::new(
            [1280, 720],
            folder("frame"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.update(&mut world);
        // Nothing selected: the scene's bounds, which the overlay knows.
        let frame = ed.ids.frame;
        ed.run(&mut world, frame);
        assert_eq!(ed.frame_asked, Some(None));
        assert_eq!(ed.shortcuts.get(&frame).map(String::as_str), Some("F"));
        // Selected: its places and its child's.
        ed.selection = vec![near];
        ed.run(&mut world, frame);
        assert_eq!(
            ed.frame_asked,
            Some(Some((Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 2.0, 0.0))))
        );
        // The overlay's answer (with the meshes' boxes): the camera flies there, looking
        // the same way, near enough for the box to fill the view.
        let (yaw, pitch) = (ed.orbit.yaw, ed.orbit.pitch);
        ed.framed = Some((Vec3::new(0.5, -0.5, -0.5), Vec3::new(3.5, 2.5, 0.5)));
        ed.update(&mut world);
        assert!(ed.camera_move.is_some(), "a move");
        settle(&mut ed, &mut world);
        assert!((ed.orbit.target - Vec3::new(2.0, 1.0, 0.0)).length() < 1e-4);
        assert_eq!((ed.orbit.yaw, ed.orbit.pitch), (yaw, pitch));
        let radius = Vec3::new(3.0, 3.0, 1.0).length() * 0.5;
        assert!(ed.orbit.distance > radius && ed.orbit.distance < radius * 4.0);
        // A point (a light) gets a little room.
        ed.framed = Some((Vec3::splat(5.0), Vec3::splat(5.0)));
        ed.update(&mut world);
        settle(&mut ed, &mut world);
        assert!(ed.orbit.distance > FRAME_RADIUS);
    }

    #[test]
    fn shift_f_follows_the_active_entity_until_it_is_asked_again() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = Editor::new(
            [1280, 720],
            folder("follow"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        let follow = ed.ids.follow;
        ed.run(&mut world, follow);
        assert!(ed.follow.is_none(), "nothing to follow");
        ed.selection = vec![cube];
        ed.run(&mut world, follow);
        assert!(ed.follow.is_some());
        assert_eq!(
            ed.shortcuts.get(&follow).map(String::as_str),
            Some("Shift+F")
        );
        let target = ed.orbit.target;
        world.insert(
            cube,
            GlobalTransform(Transform::from_xyz(2.0, 0.0, -1.0).to_affine()),
        );
        ed.update(&mut world);
        assert!((ed.orbit.target - (target + Vec3::new(2.0, 0.0, -1.0))).length() < 1e-5);
        ed.run(&mut world, follow);
        assert!(ed.follow.is_none(), "again: it stops");
        world.insert(cube, GlobalTransform(Transform::IDENTITY.to_affine()));
        ed.update(&mut world);
        assert!((ed.orbit.target - (target + Vec3::new(2.0, 0.0, -1.0))).length() < 1e-5);
    }

    #[test]
    fn bookmarks_are_set_with_ctrl_gone_to_and_kept() {
        let dir = project("bookmarks");
        let (mut world, mut ed) = start(&dir);
        ed.update(&mut world);
        let (go, set) = (ed.ids.go_bookmark[2], ed.ids.set_bookmark[2]);
        assert_eq!(ed.shortcuts.get(&go).map(String::as_str), Some("3"));
        assert_eq!(ed.shortcuts.get(&set).map(String::as_str), Some("Ctrl+3"));
        ed.run(&mut world, go);
        assert!(ed.camera_move.is_none(), "none set yet");
        ed.orbit.target = Vec3::new(4.0, 1.0, -2.0);
        ed.orbit.yaw = 2.0;
        ed.orbit.distance = 7.0;
        let marked = ed.orbit;
        ed.run(&mut world, set);
        // Elsewhere, then back: the camera flies there.
        ed.orbit.target = Vec3::ZERO;
        ed.orbit.yaw = -1.0;
        ed.orbit.distance = 30.0;
        ed.run(&mut world, go);
        settle(&mut ed, &mut world);
        assert!((ed.orbit.target - marked.target).length() < 1e-4);
        assert!((ed.orbit.yaw - marked.yaw).abs() < 1e-4);
        assert!((ed.orbit.distance - marked.distance).abs() < 1e-3);
        // The session keeps them with the scene's view.
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        let kept = ed.bookmarks[2].expect("kept");
        assert!((kept.target - marked.target).length() < 1e-5);
        assert!(
            ed.bookmarks
                .iter()
                .enumerate()
                .all(|(n, b)| (n == 2) == b.is_some())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A surface hit at `point` facing `normal`, as the overlay's pick would give it.
    fn hit(point: Vec3, normal: Vec3) -> engine_render::pick::SurfaceHit {
        engine_render::pick::SurfaceHit {
            instance: 0,
            point,
            normal,
        }
    }

    #[test]
    fn end_drops_each_selected_entity_onto_the_surface_below_as_one_step() {
        let mut world = World::new();
        let high = cube_at(&mut world, Transform::from_xyz(0.0, 3.0, 0.0));
        let low = cube_at(&mut world, Transform::from_xyz(4.0, 1.0, 0.0));
        let mut ed = Editor::new(
            [1280, 720],
            folder("drop"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.selection = vec![high, low];
        let drop = ed.ids.drop;
        assert_eq!(ed.shortcuts.get(&drop).map(String::as_str), Some("End"));
        ed.run(&mut world, drop);
        // One after the other: the overlay names each one's instances, finds the box's
        // bottom and the surface below it.
        for (e, bottom, floor) in [(high, 2.5, 0.0), (low, 0.5, -2.0)] {
            ed.update(&mut world);
            let one = ed.dropping.as_mut().unwrap().current.as_mut().unwrap();
            assert_eq!(one.entity, e);
            one.slots = Some(Vec::new());
            one.asked = Some((1, bottom));
            one.found = Some(Some(hit(Vec3::new(0.0, floor, 0.0), Vec3::Y)));
        }
        ed.update(&mut world);
        assert!(ed.dropping.is_none(), "all dropped");
        let y = |world: &World, e| world.get::<Transform>(e).unwrap().translation.y;
        assert!((y(&world, high) - 0.5).abs() < 1e-5, "{}", y(&world, high));
        assert!((y(&world, low) - -1.5).abs() < 1e-5, "{}", y(&world, low));
        // One undo step takes both back.
        ed.history.undo(&mut world);
        assert!((y(&world, high) - 3.0).abs() < 1e-5 && (y(&world, low) - 1.0).abs() < 1e-5);
        // Nothing below: said so, left where it is.
        ed.history.redo(&mut world);
        ed.selection = vec![high];
        ed.run(&mut world, drop);
        ed.update(&mut world);
        let one = ed.dropping.as_mut().unwrap().current.as_mut().unwrap();
        one.slots = Some(Vec::new());
        one.asked = Some((1, 0.0));
        one.found = Some(None);
        ed.update(&mut world);
        assert!(ed.status.contains("Nothing below") || ed.message.is_some());
        assert!((y(&world, high) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn ctrl_shift_drags_the_selection_over_surfaces() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::from_xyz(0.0, 2.0, 0.0));
        let mut ed = gizmo_editor("surface-drag", &mut world, cube);
        // The cube's world box as the overlay found it: a unit cube about its place.
        ed.selection_box = Some((Vec3::new(-0.5, 1.5, -0.5), Vec3::new(0.5, 2.5, 0.5)));
        // The x arrow, Ctrl+Shift held: pressed, then moved.
        let (center, _) = x_arrow(&ed);
        mouse(&mut ed, center.to_array(), None);
        ed.update(&mut world);
        key(&mut ed, CTRL.0, CTRL.1, true);
        key(&mut ed, SHIFT.0, SHIFT.1, true);
        mouse(&mut ed, center.to_array(), Some(true));
        ed.update(&mut world);
        assert!(ed.gizmo_drag.is_some());
        mouse(&mut ed, [center.x + 30.0, center.y], None);
        ed.update(&mut world);
        assert!(
            ed.surface_request.is_some(),
            "the surface under the pointer is asked for"
        );
        // Found: a floor at y = -1. The cube sits on it.
        ed.surfaced = Some(engine_render::pick::SurfaceResult {
            frame: 1,
            request: ed.surface_request.unwrap(),
            hit: Some(hit(Vec3::new(3.0, -1.0, 2.0), Vec3::Y)),
        });
        mouse(&mut ed, [center.x + 31.0, center.y], None);
        ed.update(&mut world);
        let at = world.get::<Transform>(cube).unwrap().translation;
        assert!((at - Vec3::new(3.0, -0.5, 2.0)).length() < 1e-4, "{at}");
        // A wall facing +X: its side on it.
        ed.surfaced = Some(engine_render::pick::SurfaceResult {
            frame: 2,
            request: ed.surface_request.unwrap(),
            hit: Some(hit(Vec3::new(5.0, 1.0, 0.0), Vec3::X)),
        });
        mouse(&mut ed, [center.x + 32.0, center.y], None);
        ed.update(&mut world);
        let at = world.get::<Transform>(cube).unwrap().translation;
        assert!((at - Vec3::new(5.5, 1.0, 0.0)).length() < 1e-4, "{at}");
        key(&mut ed, CTRL.0, CTRL.1, false);
        key(&mut ed, SHIFT.0, SHIFT.1, false);
        mouse(&mut ed, [center.x + 32.0, center.y], Some(false));
        ed.update(&mut world);
        assert!(ed.gizmo_drag.is_none());
    }

    #[test]
    fn snapping_steps_are_picked_from_their_values_and_kept() {
        let dir = project("steps");
        let (mut world, mut ed) = start(&dir);
        assert_eq!(ed.step_labels, ["0.1 m", "45°", "0.1"]);
        // The palette's list of the position step's values, the current one marked.
        let position = ed.ids.steps[0];
        ed.run(&mut world, position);
        let choices = ed
            .command_palette
            .as_ref()
            .unwrap()
            .choices
            .as_ref()
            .unwrap();
        let labels: Vec<&str> = choices.items.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, STEP_LABELS[0]);
        assert_eq!(choices.items[2].detail, "now");
        ed.palette_action(&mut world, PaletteAction::Run(3));
        assert_eq!(ed.snapping.position, 0.25);
        assert_eq!(ed.step_labels[0], "0.25 m");
        // The tool bar's menu of the angle's.
        ed.step_menu = Some(1);
        ed.set_step(&world, 1, 22.5);
        assert_eq!(ed.step_labels[1], "22.5°");
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert_eq!((ed.snapping.position, ed.snapping.angle), (0.25, 22.5));
        assert_eq!(ed.step_labels, ["0.25 m", "22.5°", "0.1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pivots_turn_about_the_middle_each_origin_or_the_active_entity() {
        let dir = project("pivots");
        let (mut world, mut ed) = start(&dir);
        let a = cube_at(&mut world, Transform::IDENTITY);
        let b = cube_at(&mut world, Transform::from_xyz(4.0, 0.0, 0.0));
        ed.selection = vec![a, b];
        let quarter = Quat::from_rotation_y(core::f32::consts::FRAC_PI_2);
        let turn = |ed: &mut Editor, world: &mut World| {
            let pivot = ed.selection_pivot(world).unwrap();
            let targets = ed.transform_targets(world);
            ed.gestures += 1;
            let merge = ed.gestures;
            ed.set_targets(
                world,
                "Rotate",
                merge,
                &targets,
                Change::Turn(quarter),
                (pivot, [Vec3::X, Vec3::Y, Vec3::Z]),
            );
            let at = |e| world.get::<Transform>(e).unwrap().translation;
            let places = (at(a), at(b));
            ed.history.undo(world);
            places
        };
        let close = |p: Vec3, q: Vec3| (p - q).length() < 1e-4;
        // The middle (2, 0, 0): both swing round it.
        assert_eq!(ed.pivot, Pivot::Median);
        let (pa, pb) = turn(&mut ed, &mut world);
        assert!(close(pa, Vec3::new(2.0, 0.0, 2.0)) && close(pb, Vec3::new(2.0, 0.0, -2.0)));
        // Their own origins: they stay where they are.
        let next = ed.ids.pivot;
        ed.run(&mut world, next);
        assert_eq!(ed.pivot, Pivot::Origins);
        let (pa, pb) = turn(&mut ed, &mut world);
        assert!(close(pa, Vec3::ZERO) && close(pb, Vec3::new(4.0, 0.0, 0.0)));
        // The active one (the last picked, at 4, 0, 0): it stays, the other swings.
        ed.run(&mut world, next);
        assert_eq!(ed.pivot, Pivot::Active);
        assert_eq!(ed.selection_pivot(&world), Some(Vec3::new(4.0, 0.0, 0.0)));
        let (pa, pb) = turn(&mut ed, &mut world);
        assert!(close(pb, Vec3::new(4.0, 0.0, 0.0)) && close(pa, Vec3::new(4.0, 0.0, 4.0)));
        assert_eq!(ed.shortcuts.get(&next).map(String::as_str), Some("Period"));
        // Kept by the session.
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert_eq!(ed.pivot, Pivot::Active);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An entity with `component` at `at`, as a scene file gives it.
    fn thing_at(
        world: &mut World,
        name: &str,
        at: Vec3,
        component: impl engine_ecs::Component,
    ) -> Entity {
        let t = Transform::from_xyz(at.x, at.y, at.z);
        world.spawn((
            SceneId::new(),
            Name::new(name),
            t,
            GlobalTransform(t.to_affine()),
            component,
        ))
    }

    #[test]
    fn lights_and_cameras_show_as_icons_a_click_selects_them() {
        let mut world = World::new();
        let lamp = thing_at(
            &mut world,
            "Lamp",
            Vec3::ZERO,
            PointLight::new(Vec3::ONE, 3.0),
        );
        let _crate = cube_at(&mut world, Transform::from_xyz(2.0, 0.0, 0.0));
        let mut ed = Editor::new(
            [1280, 720],
            folder("icons"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.update(&mut world);
        // The lamp only: the crate has nothing to show, the editor's camera is its own.
        let icons: Vec<Entity> = ed.view_icons.iter().map(|i| i.entity).collect();
        assert_eq!(icons, [lamp]);
        let at = ed.view_icons[0].at;
        let [x, y, w, h] = ed.view_rect.unwrap();
        let middle = Vec2::new((x + w / 2) as f32, (y + h / 2) as f32);
        assert!(
            at.distance(middle) < 2.0,
            "the orbit's target is the lamp: {at} {middle}"
        );
        // Hovered, then clicked beside its middle: selected.
        let beside = [at.x + 6.0, at.y - 5.0];
        mouse(&mut ed, beside, None);
        ed.update(&mut world);
        assert_eq!(ed.icon_hover, Some(lamp));
        press_release(&mut ed, &mut world, beside);
        assert_eq!(ed.selection, [lamp]);
        assert!(ed.pending_click.is_none(), "no pick needed");
        // Again there: through to what lies under it (with the rotate tool: the select
        // tool's gizmo, at the lamp, would take the press).
        ed.active_tool = 2;
        ed.update(&mut world);
        press_release(&mut ed, &mut world, beside);
        assert!(
            ed.pending_list
                .is_some_and(|(_, used, _)| used == ListUse::Cycle),
            "cycles to the geometry under the icon"
        );
        ed.pending_list = None;
        // Overlays off: no icons, nothing to click.
        let overlays = ed.ids.overlays;
        ed.run(&mut world, overlays);
        ed.update(&mut world);
        assert!(ed.view_icons.is_empty() && ed.reach_lines.is_empty());
        ed.run(&mut world, overlays);
        // A box around it takes it (nothing selected: no gizmo under the press).
        ed.selection.clear();
        ed.update(&mut world);
        mouse(&mut ed, [at.x - 30.0, at.y - 30.0], Some(true));
        ed.update(&mut world);
        mouse(&mut ed, [at.x + 30.0, at.y + 30.0], None);
        ed.update(&mut world);
        mouse(&mut ed, [at.x + 30.0, at.y + 30.0], Some(false));
        ed.update(&mut world);
        let request = ed.list_request.expect("the box asked for");
        let (_, after) = ed.box_drag.as_ref().unwrap().released.unwrap();
        ed.listed = Some((request, after, Vec::new()));
        ed.resolve_list(&world, |_| None);
        assert_eq!(ed.selection, [lamp], "the box took the icon");
    }

    #[test]
    fn the_selected_lights_and_cameras_show_their_reach() {
        let mut world = World::new();
        let point = thing_at(
            &mut world,
            "Point",
            Vec3::ZERO,
            PointLight::new(Vec3::ONE, 3.0),
        );
        let spot = thing_at(
            &mut world,
            "Spot",
            Vec3::new(1.0, 2.0, 0.0),
            SpotLight::new(Vec3::ONE, 4.0, 0.3, 0.5),
        );
        let camera = thing_at(
            &mut world,
            "Camera",
            Vec3::new(-2.0, 1.0, 1.0),
            Camera::perspective(1.0, 0.1, 100.0),
        );
        let mut ed = Editor::new(
            [1280, 720],
            folder("reach"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.update(&mut world);
        ed.update(&mut world);
        assert!(ed.reach_lines.is_empty(), "nothing selected");
        for (e, lines, of_camera) in [
            (point, 3 * 48, false),
            (spot, 48 + 4, false),
            (camera, 12, true),
        ] {
            ed.selection = vec![e];
            ed.update(&mut world);
            assert_eq!(ed.reach_lines.len(), lines, "{:?}", world.get::<Name>(e));
            assert!(ed.reach_lines.iter().all(|l| l.2 == of_camera));
        }
        // The session keeps the overlays off.
        let dir = project("overlays-kept");
        let (mut world, mut ed) = start(&dir);
        let overlays = ed.ids.overlays;
        ed.run(&mut world, overlays);
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert!(!ed.overlays);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picking_transparent_surfaces_is_a_switch_the_session_keeps() {
        let dir = project("pick-transparent");
        let (mut world, mut ed) = start(&dir);
        assert!(ed.pick_transparent, "on by default");
        ed.update(&mut world);
        ed.update(&mut world);
        let [x, y, w, h] = ed.view_rect.unwrap();
        mouse(&mut ed, [(x + w / 4) as f32, (y + h / 4) as f32], None);
        ed.update(&mut world);
        assert!(ed.pick_request.unwrap().transparent);
        let id = ed.ids.pick_transparent;
        ed.run(&mut world, id);
        assert!(!ed.pick_transparent);
        // The pointer's pixel is picked again, through transparent surfaces.
        ed.update(&mut world);
        assert_eq!(ed.pick_request.map(|r| r.transparent), Some(false));
        ed.end_session(&mut world);
        let (_, ed) = start(&dir);
        assert!(!ed.pick_transparent, "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_selection_its_children_and_the_hovered_entity_are_outlined() {
        let mut world = World::new();
        let parent = cube_at(&mut world, Transform::IDENTITY);
        let child = cube_at(&mut world, Transform::from_xyz(1.0, 0.0, 0.0));
        world.set_parent(child, Some(parent)).unwrap();
        let other = cube_at(&mut world, Transform::from_xyz(3.0, 0.0, 0.0));
        let third = cube_at(&mut world, Transform::from_xyz(6.0, 0.0, 0.0));
        let slot = move |e: Entity| {
            [parent, child, other, third]
                .iter()
                .position(|&x| x == e)
                .map(|i| i as u32 + 10)
        };
        let mut ed = Editor::new(
            [1280, 720],
            folder("outline-items"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        ed.selection = vec![parent, other];
        let request = PickRequest {
            x: 1,
            y: 1,
            transparent: true,
        };
        ed.hovered = Some((request, Some(third)));
        ed.outline(&world, slot);
        let item = |slot, class| OutlineItem { slot, class };
        assert_eq!(
            ed.outline_items,
            [
                item(10, OutlineClass::Selected),
                item(11, OutlineClass::Selected),
                item(12, OutlineClass::Active),
                item(13, OutlineClass::Hovered),
            ]
        );
        // Hovering a selected entity adds nothing.
        ed.hovered = Some((request, Some(child)));
        ed.outline(&world, slot);
        assert_eq!(ed.outline_items.len(), 3);
    }

    #[test]
    fn a_press_on_a_handle_or_a_drag_is_no_click() {
        let mut world = World::new();
        let cube = cube_at(&mut world, Transform::IDENTITY);
        let mut ed = gizmo_editor("pick-handle", &mut world, cube);
        // On the gizmo's x arrow: its drag, no click.
        let (grab, _) = x_arrow(&ed);
        mouse(&mut ed, grab.to_array(), None);
        ed.update(&mut world);
        mouse(&mut ed, grab.to_array(), Some(true));
        ed.update(&mut world);
        assert!(ed.gizmo_drag.is_some());
        mouse(&mut ed, grab.to_array(), Some(false));
        ed.update(&mut world);
        assert!(ed.pending_click.is_none(), "the handle had the press");
        // Away from it, moved more than 4 pixels: a drag, no click.
        let [x, y, w, h] = ed.view_rect.unwrap();
        let at = [(x + w / 4) as f32, (y + h / 4) as f32];
        mouse(&mut ed, at, Some(true));
        ed.update(&mut world);
        mouse(&mut ed, [at[0] + 12.0, at[1]], Some(false));
        ed.update(&mut world);
        assert!(ed.pending_click.is_none(), "a drag");
        assert_eq!(ed.selection, [cube]);
        // The pointer outside the viewport: nothing picked or hovered.
        mouse(&mut ed, [2.0, 2.0], None);
        ed.update(&mut world);
        assert!(ed.pick_request.is_none() && ed.hovered.is_none());
    }

    /// The editor's frame on the CPU (`Editor::update`, 1600 × 900) with 200 entities:
    /// nothing selected, then one with the gizmo, then a keyboard move following the
    /// pointer; and the gizmo's layout and hit test alone: `cargo test -p engine_editor
    /// --release --lib editor_frame_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn editor_frame_cost() {
        let mut world = World::new();
        let mut first = None;
        for i in 0..200 {
            let e = world.spawn((
                SceneId::new(),
                Name::new(format!("Cube {i}")),
                Transform::from_xyz(i as f32 * 0.5, 0.0, 0.0),
                GlobalTransform::default(),
                Model::new(builtin::CUBE, builtin::MATERIAL),
            ));
            first.get_or_insert(e);
        }
        let mut ed = Editor::new(
            [1600, 900],
            folder("frame-cost"),
            None,
            AssetId::new(),
            AssetPaths::new(),
        )
        .unwrap();
        let time = |ed: &mut Editor, world: &mut World| {
            for _ in 0..100 {
                ed.update(world);
            }
            let t = Instant::now();
            for _ in 0..2000 {
                ed.update(world);
            }
            t.elapsed().as_secs_f64() * 1e6 / 2000.0
        };
        let none = time(&mut ed, &mut world);
        ed.select_only(first.unwrap());
        let one = time(&mut ed, &mut world);
        assert!(ed.gizmo_shown);
        // The pointer somewhere else every frame: a change set in the undo step each time.
        let step = |ed: &mut Editor, world: &mut World, i: usize| {
            mouse(ed, [800.0 + (i % 50) as f32, 450.0], None);
            ed.update(world);
        };
        mouse(&mut ed, [800.0, 450.0], None);
        let g = ed.ids.transforms[0];
        ed.run(&mut world, g);
        for i in 0..100 {
            step(&mut ed, &mut world, i);
        }
        let t = Instant::now();
        for i in 0..2000 {
            step(&mut ed, &mut world, i);
        }
        let moving = t.elapsed().as_secs_f64() * 1e6 / 2000.0;
        assert!(ed.key_transform.is_some());
        assert_eq!(ed.history.steps().count(), 1, "one step for the whole move");
        let view = ViewProj::new(&ed.orbit, ed.view_rect.unwrap());
        let mut gizmo = Gizmo::default();
        let t = Instant::now();
        for _ in 0..10_000 {
            gizmo.layout(
                &view,
                Mode::All,
                Vec3::ZERO,
                [Vec3::X, Vec3::Y, Vec3::Z],
                1.0,
            );
            std::hint::black_box(gizmo.hit(Vec2::new(800.0, 450.0), 1.0));
        }
        let layout = t.elapsed().as_secs_f64() * 1e6 / 10_000.0;
        println!(
            "editor frame: nothing selected {none:.1} µs, one selected with the gizmo {one:.1} µs, \
             a keyboard move {moving:.1} µs; the gizmo's layout and hit test {layout:.2} µs \
             ({} lines)",
            gizmo.lines.len()
        );
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
