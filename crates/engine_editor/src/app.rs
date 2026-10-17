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
//!   hierarchy and order; a click selects, a click on the selected row opens or closes it.
//!   Its `+` button adds an entity (a model, a light, a camera) where the camera looks.
//! - **The inspector** edits the selected entity's components ([`crate::inspector`]):
//!   numbers are dragged sideways ([`crate::Snapping`]: Shift inverts the default, Ctrl
//!   rounds to the grid), checkboxes and enums are clicked.
//! - **Every edit is undoable** ([`History`]). Commands and their shortcuts: Ctrl+Z undo,
//!   Ctrl+Y or Ctrl+Shift+Z redo, Ctrl+S save, Delete, Ctrl+D duplicate, Ctrl+A add, Y
//!   snapping by default.

use crate::camera::OrbitCamera;
use crate::icons;
use crate::input::{Button, InputCollector};
use crate::inspector::{self, Inspector, Kind, Property};
use crate::paint::{UiText, paint};
use crate::shell::{
    Field, FieldAction, FieldEvent, Palette, Section, ShellModel, TreeItem, Widget, build,
    dismissed,
};
use crate::snap::{Quantity, Snapping};
use crate::{Chord, CommandId, Commands, History, KeyContext, Keymap};
use engine_app::{
    AppError, Game, Model, Overlay, OverlayPass, Player, PlayerSettings, Window, builtin,
};
use engine_core::math::{Quat, Vec3};
use engine_ecs::{ComponentId, Entity, GlobalTransform, QueryState, Transform, World};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind};
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

/// How long a message stays in the status bar, in seconds.
const MESSAGE_SECONDS: f32 = 4.0;

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
    let format = player.output_format();
    let setup = player.setup();
    let renderer = Renderer2d::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        format,
        QuadMode::for_device(setup.device),
    )?;
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
}

/// The editor's commands and their default shortcuts.
fn commands() -> (Commands, Keymap, CommandIds) {
    let mut c = Commands::new();
    let ids = CommandIds {
        undo: c.register("edit.undo", "Edit", "Undo"),
        redo: c.register("edit.redo", "Edit", "Redo"),
        save: c.register("file.save", "File", "Save Scene"),
        delete: c.register("scene.delete", "Scene", "Delete Selection"),
        duplicate: c.register("scene.duplicate", "Scene", "Duplicate Selection"),
        add: c.register("scene.add", "Scene", "Add Entity"),
        snap: c.register("edit.snap", "Edit", "Snap by Default"),
    };
    let mut keymap = Keymap::new();
    for (chord, id) in [
        ("Ctrl+Z", ids.undo),
        ("Ctrl+Y", ids.redo),
        ("Ctrl+Shift+Z", ids.redo),
        ("Ctrl+S", ids.save),
        ("Delete", ids.delete),
        ("Ctrl+D", ids.duplicate),
        ("Ctrl+A", ids.add),
        ("Y", ids.snap),
    ] {
        if let Some(chord) = Chord::parse(chord) {
            let _ = keymap.bind(chord, KeyContext::Global, id);
        }
    }
    (c, keymap, ids)
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
    /// The property's numbers as shown when the drag started (degrees for angles).
    start: [f64; 4],
    /// The drag's undo step: one per gesture.
    merge: u64,
    /// The step's name (`Edit Transform.translation`).
    name: String,
    /// Escape took the drag back: the rest of the gesture changes nothing.
    cancelled: bool,
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
    selected: Option<Entity>,
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
    snapping: Snapping,
    inspector: Inspector,
    drag: Option<Drag>,
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
    } else if world.get::<PointLight>(e).is_some() || world.get::<SpotLight>(e).is_some() {
        (icons::LIGHT, "Light")
    } else if world.get::<Model>(e).is_some() {
        (icons::MESH_INSTANCE, "Model")
    } else {
        (icons::NODE_3D, "Entity")
    }
}

/// A new entity of the add menu's kind `what`, at `at`.
fn spawn(world: &mut World, what: usize, at: Vec3) -> Entity {
    let mut transform = Transform::from_translation(at);
    let e = world.spawn((
        SceneId::new(),
        Name::new(ADD[what]),
        GlobalTransform::default(),
    ));
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
        let (commands, keymap, ids) = commands();
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
            selected: None,
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
            snapping: Snapping::default(),
            inspector: Inspector::new(),
            drag: None,
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
        let mut stack: Vec<(Entity, u16)> = roots.iter().rev().map(|&e| (e, 0)).collect();
        while let Some((e, depth)) = stack.pop() {
            let mut children = world
                .children(e)
                .filter(|&c| world.get::<SceneId>(c).is_some())
                .peekable();
            let has_children = children.peek().is_some();
            self.rows.push(Row {
                entity: e,
                depth,
                children: has_children,
            });
            if has_children && !self.collapsed.contains(&e) {
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

        // The inspector: the selected entity's components, field by field.
        if self
            .selected
            .is_some_and(|e| world.get::<SceneId>(e).is_none())
        {
            self.selected = None;
        }
        match self.selected {
            Some(e) => self.inspector.build(world, e, &self.assets),
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
            values.extend(self.inspector.values(p));
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
        let fields: Vec<Field<'_>> = properties
            .iter()
            .zip(&spans)
            .map(|(p, &(a, b))| {
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
                    },
                    Kind::Bool => Widget::Check(first == "true"),
                    Kind::Choice => Widget::Choice {
                        current: first,
                        open: (open == Some((p.component, p.offset))).then_some(options.as_slice()),
                    },
                    Kind::Text => Widget::Text(first),
                };
                Field {
                    name: self.inspector.name(p),
                    widget,
                }
            })
            .collect();
        let sections: Vec<Section<'_>> = self
            .inspector
            .sections()
            .iter()
            .map(|s| Section {
                title: s.title,
                fields: &fields[s.properties.clone()],
            })
            .collect();

        // The tree.
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
                    selected: Some(row.entity) == self.selected,
                }
            })
            .collect();
        let selection = self.selected.map_or("", |e| {
            world
                .get::<Name>(e)
                .map_or(kind_of(world, e).1, Name::as_str)
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
        let scenes = [self.scene_label.as_str()];
        let tools = self.tools.each_ref().map(String::as_str);
        let model = ShellModel {
            scenes: &scenes,
            active_scene: 0,
            tools: &tools,
            active_tool: self.active_tool,
            tree: &items,
            selection,
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
            add_menu: self.add_open.then_some(&ADD[..]),
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
        if dismissed(&self.ui, &response) {
            self.add_open = false;
            self.open_choice = None;
        }
        if response.add {
            self.add_open = !self.add_open;
        }
        if let Some(i) = response.add_item {
            self.add(world, i);
        }
        if let Some(row) = response.tree_row.and_then(|i| self.rows.get(i).copied()) {
            // A click on the selected row opens or closes it.
            if self.selected == Some(row.entity)
                && row.children
                && !self.collapsed.remove(&row.entity)
            {
                self.collapsed.insert(row.entity);
            }
            if self.selected != Some(row.entity) {
                self.open_choice = None;
            }
            self.selected = Some(row.entity);
        }
        if let Some(event) = response.field {
            self.edit(world, event);
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
        // The shortcuts.
        for i in 0..self.input.chords().len() {
            let chord = self.input.chords()[i];
            if let Some(id) = self.keymap.lookup(chord, &[]) {
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

    /// Applies an inspector field's action to the selected entity, as an undoable step.
    fn edit(&mut self, world: &mut World, event: FieldEvent) {
        let Some(e) = self.selected else {
            return;
        };
        let Some(section) = self.inspector.sections().get(event.section) else {
            return;
        };
        let Some(&p) = self
            .inspector
            .properties()
            .get(section.properties.start + event.field)
        else {
            return;
        };
        let title = format!("{}.{}", section.title, self.inspector.name(&p));
        match event.action {
            FieldAction::Drag {
                part,
                travel,
                start,
            } => {
                if start || self.drag.is_none() {
                    let Some(ptr) = world.get_raw(e, p.component) else {
                        return;
                    };
                    // SAFETY: the entity's live value of the property's component.
                    let values = unsafe { inspector::read_numbers(&p, ptr.as_ptr()) };
                    self.gestures += 1;
                    self.drag = Some(Drag {
                        property: p,
                        part: usize::from(part),
                        start: values,
                        merge: self.gestures,
                        name: format!("Edit {title}"),
                        cancelled: false,
                    });
                    return;
                }
                let Some(drag) = self.drag.as_ref().filter(|d| !d.cancelled) else {
                    return;
                };
                let quantity = match drag.property.kind {
                    Kind::Numbers { quantity, .. } => quantity,
                    Kind::Rotation => Quantity::Angle,
                    _ => return,
                };
                let mut values = drag.start;
                values[drag.part] = self.snapping.drag(
                    quantity,
                    drag.start[drag.part],
                    travel,
                    self.input.shift(),
                    self.input.ctrl(),
                );
                let (property, merge, name) = (drag.property, drag.merge, drag.name.clone());
                self.history.begin(&name, Some(merge));
                self.history.touch(world, e);
                if let Some(ptr) = world.get_raw_mut(e, property.component) {
                    // SAFETY: the entity's live value of the property's component.
                    unsafe { inspector::write_numbers(&property, ptr.as_ptr(), values) };
                }
                self.history.commit(world);
            }
            FieldAction::Release => self.drag = None,
            FieldAction::Toggle => {
                self.history.begin(&format!("Toggle {title}"), None);
                self.history.touch(world, e);
                if let Some(ptr) = world.get_raw_mut(e, p.component) {
                    // SAFETY: the entity's live value of the property's component.
                    unsafe { inspector::toggle(&p, ptr.as_ptr()) };
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
                self.history.touch(world, e);
                if let Some(ptr) = world.get_raw_mut(e, p.component) {
                    // SAFETY: the entity's live value of the property's component.
                    unsafe { inspector::choose(&p, ptr.as_ptr(), i) };
                }
                self.history.commit(world);
            }
        }
    }

    /// Adds an entity of the add menu's kind `what` where the camera looks (on the grid),
    /// and selects it.
    fn add(&mut self, world: &mut World, what: usize) {
        self.add_open = false;
        let step = self.snapping.position as f32;
        let at = (self.orbit.target / step).round() * step;
        self.history.begin(&format!("Add {}", ADD[what]), None);
        let e = spawn(world, what, at);
        self.history.touch_new(world, e);
        self.history.commit(world);
        self.selected = Some(e);
    }

    /// Runs a command.
    fn run(&mut self, world: &mut World, id: CommandId) {
        self.commands.ran(id);
        let ids = self.ids;
        if id == ids.undo || id == ids.redo {
            self.drag = None;
            self.open_choice = None;
            let selected = self.selected.and_then(|e| world.get::<SceneId>(e).copied());
            let done = if id == ids.undo {
                self.history.undo(world).map(|name| format!("Undo: {name}"))
            } else {
                self.history.redo(world).map(|name| format!("Redo: {name}"))
            };
            if let Some(message) = done {
                self.say(message);
            }
            self.reselect(world, selected);
        } else if id == ids.save {
            match self.save(world) {
                Ok(path) => self.say(format!("Saved {}", path.display())),
                Err(e) => self.say(format!("Not saved: {e}")),
            }
        } else if id == ids.delete {
            if let Some(e) = self.selected.take() {
                self.history.begin("Delete", None);
                self.history.delete_subtree(world, e);
                self.history.commit(world);
            }
        } else if id == ids.duplicate {
            if let Some(e) = self.selected {
                self.history.begin("Duplicate", None);
                let copy = self.history.duplicate_subtree(world, e);
                self.history.commit(world);
                self.selected = copy.or(self.selected);
            }
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
        let (component, rest) = engine_scene::text::component_of(path);
        let (id, info) = world
            .component_ids_of(entity)
            .ok_or("no such entity")?
            .iter()
            .find_map(|&id| {
                let info = world.components().type_info(id)?;
                (info.name == component).then_some((id, info))
            })
            .ok_or_else(|| format!("the entity has no `{component}`"))?;
        let segs = engine_scene::text::segments(rest)?;
        let index: HashMap<SceneId, Entity> = {
            let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
            q.iter_readonly(world).map(|(e, &id)| (id, e)).collect()
        };
        let mut refs = IndexRefs { index: &index };
        self.history.begin(&format!("Set {path}"), None);
        self.history.touch(world, entity);
        let ptr = world
            .get_raw_mut(entity, id)
            .ok_or("the component is gone")?
            .as_ptr();
        // SAFETY: `ptr` is the entity's live value of the component `info` describes; the
        // path and the text are checked against that description.
        let result = unsafe {
            engine_scene::text::resolve(&segs, info, ptr, &mut refs).and_then(|target| {
                let (ty, attrs, at) =
                    target.ok_or_else(|| format!("`{component}` has no `{rest}`"))?;
                engine_scene::text::set_text(ty, &attrs, at, value, &mut refs)
            })
        };
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

    /// Selects the entity with scene id `id` again (undo and redo may have made it anew).
    fn reselect(&mut self, world: &mut World, id: Option<SceneId>) {
        let Some(id) = id else {
            return;
        };
        if self
            .selected
            .is_some_and(|e| world.get::<SceneId>(e) == Some(&id))
        {
            return;
        }
        let q = self.scene_ids.get_or_insert_with(|| QueryState::new(world));
        self.selected = q
            .iter_readonly(world)
            .find(|&(_, &s)| s == id)
            .map(|(e, _)| e);
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

    /// The entity selected in the scene tree.
    pub fn selected(&self) -> Option<Entity> {
        self.shared.borrow().selected
    }

    /// Selects an entity (or nothing), as a click in the tree does.
    pub fn select(&mut self, entity: Option<Entity>) {
        self.shared.borrow_mut().selected = entity;
    }

    /// Runs an editor command by its stable name (`edit.undo`, `edit.redo`, `file.save`,
    /// `scene.delete`, `scene.duplicate`, `scene.add`, `edit.snap`), as its shortcut does.
    /// False if there is no such command.
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
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
        ed.selected = Some(cube);
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
        ed.selected = Some(e);
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
        let cube = ed.selected.unwrap();
        assert_eq!(world.get::<Name>(cube).map(Name::as_str), Some("Cube"));
        assert_eq!(world.get::<Model>(cube).unwrap().mesh, builtin::CUBE);
        ed.run(&mut world, ed.ids.duplicate);
        let copy = ed.selected.unwrap();
        assert_ne!(copy, cube);
        assert_eq!(models(&mut world), 2);
        ed.run(&mut world, ed.ids.delete);
        assert!(ed.selected.is_none());
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
}
