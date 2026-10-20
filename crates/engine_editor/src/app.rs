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

use crate::camera::OrbitCamera;
use crate::icons;
use crate::input::{Button, InputCollector};
use crate::inspector::{self, Defaults, Inspector, Kind, Property};
use crate::paint::{UiText, paint};
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
use engine_scene::{AssetId, AssetPaths, Name, SaveOptions, SceneId, SceneRegistry};
use engine_ui::{Theme, Ui, UiKey};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

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

/// [`run`] for `frames` frames at most (smoke runs): how many ran.
///
/// # Errors
///
/// As [`run`].
pub fn run_frames<G: Game>(
    mut settings: PlayerSettings,
    start: Start<G>,
    frames: Option<u64>,
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
    let mut player = Player::new(&settings)?;
    let (mut game, mut overlay) = editor(&mut player, &settings, start)?;
    let mut count = 0;
    let mut result: Result<(), Error> = Ok(());
    while !player.closing() && frames.is_none_or(|n| count < n) {
        if let Err(e) = player.frame_overlaid(&mut game, Some(&mut overlay)) {
            result = Err(e.into());
            break;
        }
        player.set_view_rect(game.view_rect());
        game.sync_assets(&player);
        count += 1;
    }
    overlay.destroy(&mut player)?;
    result.map(|()| count)
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

/// The command palette while it is open: one box for every command and, after `@`, the
/// scene's objects (`docs/research/ux/command-palette.md`).
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
}

impl CommandPalette {
    /// Whether the query looks for the scene's objects (`@`) rather than commands.
    fn finds_objects(&self) -> bool {
        self.query.trim_start().starts_with('@')
    }

    /// How many results there are.
    fn len(&self) -> usize {
        if self.finds_objects() {
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
    stack.sort_unstable_by_key(|e| core::cmp::Reverse(e.index()));
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
    /// differs.
    saved_head: u64,
    /// The scene tab: the file's name, then `*` with unsaved changes.
    scene_label: String,
    /// Where the player's assets are, for showing and saving references by path.
    assets: AssetPaths,
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
            scene_label: String::new(),
            assets,
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
            WindowEventKind::Focused(false) => input.focus_lost(),
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
    }

    /// The scene's entities in tree order, under open rows only.
    fn collect_rows(&mut self, world: &mut World) {
        let ids = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        let world = &*world;
        let mut roots: Vec<Entity> = ids
            .iter_readonly(world)
            .map(|(e, _)| e)
            .filter(|&e| {
                world
                    .parent(e)
                    .is_none_or(|p| world.get::<SceneId>(p).is_none())
            })
            .collect();
        // Spawn order: the scene file's order.
        roots.sort_unstable_by_key(|e| e.index());
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
        // The command palette's results in view.
        let mut palette_rows: Vec<PaletteRow<'_>> = Vec::new();
        if let Some(cp) = &self.command_palette {
            let end = (cp.first + PALETTE_ROWS).min(cp.len());
            for i in cp.first..end {
                palette_rows.push(if cp.finds_objects() {
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
            hint: "Type a command, or @ and an object's name",
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
        if let Some(name) = query.strip_prefix('@') {
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
                if cp.finds_objects() {
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
            None => {
                let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
                let world = &*world;
                q.iter_readonly(world)
                    .map(|(e, _)| e)
                    .filter(|&e| {
                        world
                            .parent(e)
                            .is_none_or(|p| world.get::<SceneId>(p).is_none())
                    })
                    .collect()
            }
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
        self.history.begin(&format!("Add {}", ADD[what]), None);
        let e = spawn(world, what, &name, at);
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
        } else if id == ids.save {
            match self.save(world) {
                Ok(path) => self.say(format!("Saved {}", path.display())),
                Err(e) => self.say(format!("Not saved: {e}")),
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
        let registry = SceneRegistry::from_program();
        let mut text = String::new();
        let options = SaveOptions {
            id: Some(self.scene_id),
            assets: Some(&self.assets),
        };
        engine_scene::save_with(world, &registry, &options, &mut text)
            .map_err(|e| e.to_string())?;
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
        Ok(path)
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

    /// Learns the player's asset paths again when they changed (a scene loaded since):
    /// the inspector shows references by path.
    pub fn sync_assets(&mut self, player: &Player) {
        let paths = player.assets().paths();
        let mut shared = self.shared.borrow_mut();
        if shared.assets.len() != paths.len() {
            shared.assets = paths.clone();
        }
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
            text.starts_with(&format!("[scene version=2 id={id}]")),
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
}
