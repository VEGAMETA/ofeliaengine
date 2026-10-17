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
//!   The inspector shows the selected entity's components as the scene file writes them.

use crate::camera::OrbitCamera;
use crate::icons;
use crate::input::{Button, InputCollector};
use crate::paint::{UiText, paint};
use crate::shell::{Field, Palette, Section, ShellModel, TreeItem, build};
use engine_app::{AppError, Game, Model, Overlay, OverlayPass, Player, PlayerSettings, Window};
use engine_core::math::Vec3;
use engine_ecs::{Entity, QueryState, Transform, World};
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind};
use engine_render2d::{Canvas, GlyphTexture, QuadMode, Renderer2d, View2d};
use engine_runtime::{Camera, PointLight, SpotLight, Sun, look_rotation};
use engine_scene::{AssetPaths, Name, SceneId};
use engine_ui::{Theme, Ui};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::Instant;

type Error = Box<dyn std::error::Error>;

/// How the game is made when Play starts (ADR 0107, phase E3): given the player with the
/// scene loaded.
pub type Start<G> = fn(&mut Player) -> Result<G, Error>;

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
    let palette = Palette::default();
    let ui = Ui::new(Theme {
        text: palette.text,
        panel: palette.dock,
        ..Theme::default()
    });
    let text = UiText::new()?;
    let format = player.output_format();
    let setup = player.setup();
    let renderer = Renderer2d::new(
        setup.device,
        setup.allocator,
        setup.bindless,
        format,
        QuadMode::for_device(setup.device),
    )?;
    let glyphs = GlyphTexture::new(setup.device, setup.allocator, setup.bindless, &text.atlas)?;
    let scene = settings
        .scene
        .as_ref()
        .and_then(|p| p.file_name())
        .map_or_else(
            || String::from("untitled"),
            |n| n.to_string_lossy().into_owned(),
        );
    let shared = Rc::new(RefCell::new(Editor {
        ui,
        text,
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
        scenes: vec![scene],
        assets: player.assets().paths().clone(),
        tools: [
            format!("{} Select", icons::TOOL_SELECT),
            format!("{} Move", icons::TOOL_MOVE),
            format!("{} Rotate", icons::TOOL_ROTATE),
            format!("{} Scale", icons::TOOL_SCALE),
        ],
        active_tool: 0,
        status: String::new(),
        lines: String::new(),
        rows: Vec::new(),
        scene_ids: None,
        frames: 0,
        since: Instant::now(),
        fps: 0.0,
    }));
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
    scenes: Vec<String>,
    /// Where the player's assets are, for showing references by path.
    assets: AssetPaths,
    tools: [String; 4],
    active_tool: usize,
    status: String,
    /// The selected entity's components as scene lines.
    lines: String,
    rows: Vec<Row>,
    scene_ids: Option<QueryState<(Entity, &'static SceneId)>>,
    frames: u64,
    since: Instant,
    fps: f64,
}

/// An asset reference shown by its path (`builtin#cube`), the id left to the file.
fn asset_path(value: &str) -> &str {
    value
        .strip_prefix("asset(\"")
        .and_then(|v| v.split_once("\", "))
        .map_or(
            value,
            |(path, _)| if path.is_empty() { value } else { path },
        )
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

impl Editor {
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
                pressed,
                ..
            } => input.key(virtual_key, pressed),
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

    /// The frame's editing: the UI from the world, the camera from the viewport's drags.
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

        // The inspector: the selected entity's reflected components, as the scene writes
        // them.
        if self
            .selected
            .is_some_and(|e| world.get::<SceneId>(e).is_none())
        {
            self.selected = None;
        }
        self.lines.clear();
        let mut sections_at: Vec<(&'static str, usize, usize)> = Vec::new();
        if let Some(e) = self.selected
            && let Some(ids) = world.component_ids_of(e)
        {
            for &id in ids {
                let Some(info) = world.components().type_info(id) else {
                    continue;
                };
                if info.attrs.hidden || info.attrs.skip {
                    continue;
                }
                let Some(ptr) = world.get_raw(e, id) else {
                    continue;
                };
                let start = self.lines.len();
                // SAFETY: the entity's live value of the component `info` describes.
                unsafe {
                    engine_scene::write_lines(
                        info,
                        ptr.as_ptr().cast_const(),
                        Some(&self.assets),
                        &mut self.lines,
                    );
                };
                sections_at.push((info.name, start, self.lines.len()));
            }
            sections_at.sort_by_key(|s| s.0);
        }
        let mut fields: Vec<Field<'_>> = Vec::new();
        let mut spans: Vec<(&'static str, usize, usize)> = Vec::with_capacity(sections_at.len());
        for &(title, start, end) in &sections_at {
            let first = fields.len();
            for line in self.lines[start..end].lines() {
                let (name, value) = line.split_once(" = ").unwrap_or(("", line));
                fields.push(Field {
                    name,
                    value: asset_path(value),
                });
            }
            spans.push((title, first, fields.len()));
        }
        let sections: Vec<Section<'_>> = spans
            .iter()
            .map(|&(title, a, b)| Section {
                title,
                fields: &fields[a..b],
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
        let _ = write!(
            self.status,
            "ofeliaengine  ·  {} entities  ·  {:.0} FPS  ·  {}×{}",
            self.rows.len(),
            self.fps,
            self.size[0],
            self.size[1]
        );
        let scenes: Vec<&str> = self.scenes.iter().map(String::as_str).collect();
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
        };
        let scale = self.scale.max(0.5);
        let logical = [self.size[0] as f32 / scale, self.size[1] as f32 / scale];
        self.ui.begin_frame(self.input.ui_input(scale), logical);
        let response = build(&mut self.ui, &model, &self.palette);
        self.ui.end_frame(&mut self.text);

        if let Some(i) = response.tool {
            self.active_tool = i;
        }
        if let Some(row) = response.tree_row.and_then(|i| self.rows.get(i).copied()) {
            // A click on the selected row opens or closes it.
            if self.selected == Some(row.entity)
                && row.children
                && !self.collapsed.remove(&row.entity)
            {
                self.collapsed.insert(row.entity);
            }
            self.selected = Some(row.entity);
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
