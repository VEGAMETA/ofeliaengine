//! The editor window's layout (`docs/research/ux/editor-layout.md`): the team's Godot setup
//! as the default, built with `engine_ui` every frame from a [`ShellModel`].
//!
//! ```text
//! ┌ scene tabs ········· quiet ► ‖ ■ ─────────┬──────────────┬──────────────┐
//! │ tool bar                                  │ Scene        │ Inspector    │
//! │                                           │ filter       │ filter       │
//! │ viewport                                  │ node tree    │ properties   │
//! ├───────────────────────────────────────────┤              │              │
//! │ file browser: folders │ files             │              │              │
//! ├───────────────────────────────────────────┴──────────────┴──────────────┤
//! │ Output · Debugger · Audio · Animation · Shader                   status │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The viewport area is left transparent: the 3D view is drawn below the UI into the
//! rectangle [`ShellResponse::viewport`] had in the last layout ([`Ui::rect_of`]). While the
//! game plays, a thin colored edge around it shows the state instead of big buttons.

use crate::icons;
use engine_ui::{
    Align, Color, Direction, Size, Style, TextField, TextFieldResponse, Ui, UiImage, UiKey,
};

/// A color from sRGB bytes, as the linear value the renderer blends in.
pub fn srgb(r: u8, g: u8, b: u8) -> Color {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    [lin(r), lin(g), lin(b), 1.0]
}

/// Colors of the shell: design tokens (CLAUDE.md §12), dark by default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Behind the docks: the 1-pixel gaps between them.
    pub window: Color,
    /// Dock backgrounds.
    pub dock: Color,
    /// Tab strips and bars.
    pub header: Color,
    /// Input and value boxes.
    pub field: Color,
    /// A value box under the mouse or being dragged.
    pub field_hover: Color,
    /// Selected rows and the active tool.
    pub selection: Color,
    /// Rows selected besides the active one.
    pub selection_dim: Color,
    /// Main text.
    pub text: Color,
    /// Secondary text: labels, inactive tabs, hints.
    pub text_dim: Color,
    /// Accents: the active tab's edge, focus.
    pub accent: Color,
    /// The edge around the viewport while the game plays.
    pub playing: Color,
    /// Errors in the Output, with their mark (not by color alone).
    pub error: Color,
    /// Warnings in the Output.
    pub warning: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            window: srgb(18, 18, 22),
            dock: srgb(33, 34, 40),
            header: srgb(26, 27, 32),
            field: srgb(45, 47, 55),
            field_hover: srgb(60, 63, 74),
            selection: srgb(52, 78, 120),
            selection_dim: srgb(40, 54, 80),
            text: srgb(222, 224, 230),
            text_dim: srgb(140, 144, 156),
            accent: srgb(96, 150, 240),
            playing: srgb(90, 200, 120),
            error: srgb(236, 104, 96),
            warning: srgb(230, 184, 84),
        }
    }
}

/// The Output panel (ADR 0107 E6): the log's rows shown, its filter and level toggles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputView<'a> {
    /// The rows shown, oldest first.
    pub rows: &'a [OutputRow<'a>],
    /// The text filter.
    pub filter: &'a str,
    /// Which levels show: errors, warnings, information, the rest.
    pub levels: [bool; 4],
    /// How many records of each level came.
    pub counts: [usize; 4],
    /// The newest row is kept in view.
    pub follow: bool,
}

/// A row of the Output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputRow<'a> {
    /// The level's place (0 errors, 1 warnings, 2 information, 3 the rest); none for a
    /// line marking an event (a Play).
    pub level: Option<usize>,
    /// When it last came.
    pub time: &'a str,
    /// Its target (empty for a marking line).
    pub target: &'a str,
    /// Its message, or the marking line's words.
    pub message: &'a str,
    /// How many times it came, folded into this row.
    pub count: u32,
    /// Its text names its cause: a click shows it.
    pub link: bool,
}

/// A row of the scene tree or of the folder tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeItem<'a> {
    /// Nesting level (0: root).
    pub depth: u16,
    /// Icon glyph before the name (see [`crate::icons`]); empty for none.
    pub icon: &'a str,
    /// Shown name.
    pub name: &'a str,
    /// Type shown in a dim badge (`Node3D`, `MeshInstance`…); empty for none.
    pub kind: &'a str,
    /// `Some(expanded)` for rows with children.
    pub expanded: Option<bool>,
    /// Selected.
    pub selected: bool,
    /// The active one of the selection (the inspector shows it); drawn brighter.
    pub active: bool,
    /// Its name is being edited in place (F2, a double click).
    pub renaming: bool,
    /// It is disabled: its eye is shut.
    pub disabled: bool,
    /// It is off (disabled, or below a disabled entity): drawn dim.
    pub off: bool,
}

/// An entry of a menu of groups (the add menu): a kind to pick, or a group of them opened
/// beside it on hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuEntry<'a> {
    /// Glyph before the label.
    pub icon: &'a str,
    /// What the entry says.
    pub label: &'a str,
    /// What a click on it reports (an entry without `children`).
    pub kind: usize,
    /// A group's entries; empty for a kind.
    pub children: &'a [MenuEntry<'a>],
}

/// An entry of a menu: an icon (see [`crate::icons`]; empty for none) and a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem<'a> {
    /// Glyph before the label.
    pub icon: &'a str,
    /// What the entry says.
    pub label: &'a str,
}

/// How a property is shown in the inspector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Widget<'a> {
    /// Text that cannot be edited here.
    Text(&'a str),
    /// Text edited in a field (strings, names).
    Edit(&'a str),
    /// Numbers dragged sideways: one, or a vector's parts with their labels (`x y z`).
    Numbers {
        /// The shown values.
        values: &'a [&'a str],
        /// A label per value (none for a single number).
        labels: &'a [&'a str],
        /// The part being typed (a click without a drag): a text field instead.
        typing: Option<u8>,
    },
    /// A checkbox; `None` while the selected entities differ.
    Check(Option<bool>),
    /// One of several names; `open` lists them all while the list is open.
    Choice {
        /// The current one.
        current: &'a str,
        /// Every one, while open.
        open: Option<&'a [&'a str]>,
    },
    /// An asset, as Godot's pickers show one (ADR 0107 E4): its preview, or its kind's
    /// icon until there is one, and its name; a click picks another, a file dropped on it
    /// takes its place.
    Asset {
        /// Its preview.
        preview: Option<UiImage>,
        /// Its kind's icon (none for no asset).
        icon: &'a str,
        /// Its name (the file and part), or `none`.
        name: &'a str,
    },
}

/// One property in the inspector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field<'a> {
    /// Name.
    pub name: &'a str,
    /// How it is shown.
    pub widget: Widget<'a>,
    /// It differs from its type's default: a reset button (↺) follows it.
    pub changed: bool,
}

/// A row of the command palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaletteRow<'a> {
    /// Glyph before the label (see [`crate::icons`]); empty for none.
    pub icon: &'a str,
    /// "Category: Title", or an object's name.
    pub label: &'a str,
    /// The label's characters (indices) the query matched: highlighted.
    pub matched: &'a [u16],
    /// The first characters that are the category: dimmed.
    pub category: usize,
    /// At the right end: the shortcut, or what the object is.
    pub detail: &'a str,
}

/// The command palette while it is open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaletteView<'a> {
    /// What is typed.
    pub query: &'a str,
    /// The hint while nothing is typed.
    pub hint: &'a str,
    /// The best matches, best first.
    pub rows: &'a [PaletteRow<'a>],
    /// The highlighted row (Enter runs it).
    pub selected: usize,
}

/// What the user did in the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    /// The query changed (`Ui::edited_text`).
    Query,
    /// Up or Down moved the highlight by this many rows.
    Move(i32),
    /// Run row `i` (Enter on the highlighted one, or a click).
    Run(usize),
    /// Close it (Escape, a click elsewhere).
    Close,
}

/// What the user did to an inspector field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldAction {
    /// Number `part` of the field is dragged: the pointer's horizontal travel since the
    /// press, in logical pixels; `start` on the frame of the press.
    Drag {
        /// Which number of a vector.
        part: u8,
        /// Travel since the press.
        travel: f32,
        /// The press happened this frame.
        start: bool,
    },
    /// The drag ended.
    Release,
    /// Typing into number `part` ended: committed (Enter, Tab, a click elsewhere; the text
    /// is `Ui::edited_text`) or cancelled (Escape).
    Typed {
        /// Which number of a vector.
        part: u8,
        /// Kept, not cancelled.
        commit: bool,
    },
    /// Editing the field's text ended: committed or cancelled.
    Text {
        /// Kept, not cancelled.
        commit: bool,
    },
    /// The reset button (↺): back to the type's default.
    Reset,
    /// The checkbox was clicked.
    Toggle,
    /// The choice was clicked: its list opens or closes.
    Open,
    /// An asset field was clicked: another is picked.
    Pick,
    /// Option `i` of the open list was picked.
    Choose(usize),
}

/// An inspector field's action, with where the field is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldEvent {
    /// The section (component).
    pub section: usize,
    /// The field in it.
    pub field: usize,
    /// What happened.
    pub action: FieldAction,
}

/// A group of properties (a component).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Section<'a> {
    /// Component name.
    pub title: &'a str,
    /// Its fields.
    pub fields: &'a [Field<'a>],
}

/// What a file is, for its icon color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A folder.
    Folder,
    /// A scene.
    Scene,
    /// A Luau script.
    Script,
    /// A texture.
    Texture,
    /// A model or mesh.
    Mesh,
    /// Sound.
    Audio,
    /// Anything else.
    Other,
}

/// A file in the file browser.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileItem<'a> {
    /// File name.
    pub name: &'a str,
    /// Kind.
    pub kind: FileKind,
    /// Picked.
    pub selected: bool,
    /// Its thumbnail (ADR 0112), once made; its kind's icon until then.
    pub thumbnail: Option<UiImage>,
    /// Its name is being edited in place (F2).
    pub renaming: bool,
}

/// Everything the shell shows.
#[derive(Debug, Clone, Copy)]
pub struct ShellModel<'a> {
    /// Open scenes (tabs); an unsaved one ends with `*`.
    pub scenes: &'a [&'a str],
    /// The current scene tab.
    pub active_scene: usize,
    /// Tools in the tool bar, and the active one.
    pub tools: &'a [&'a str],
    /// The active tool.
    pub active_tool: usize,
    /// Scene tree rows (only the visible ones: children of collapsed rows are left out).
    pub tree: &'a [TreeItem<'a>],
    /// Inspector heading: the selection's name and type.
    pub selection: &'a str,
    /// The selection is in a prefab's instance: the heading offers to open the prefab.
    pub prefab: bool,
    /// Inspector sections.
    pub inspector: &'a [Section<'a>],
    /// The current folder's path.
    pub path: &'a str,
    /// Folder tree rows.
    pub folders: &'a [TreeItem<'a>],
    /// Files in the current folder, shown as tiles.
    pub files: &'a [FileItem<'a>],
    /// The side of a tile's picture (logical pixels).
    pub tile: f32,
    /// The tiles a row holds ([`tile_columns`] of the listing's width).
    pub columns: usize,
    /// Bottom panel tabs, and the open one (`None`: all closed).
    pub bottom_tabs: &'a [&'a str],
    /// The Output panel above the bottom bar, while its tab is open.
    pub output: Option<OutputView<'a>>,
    /// The open bottom panel.
    pub active_bottom: Option<usize>,
    /// The game is running.
    pub playing: bool,
    /// The game is paused.
    pub paused: bool,
    /// Right end of the bottom bar.
    pub status: &'a str,
    /// Dragged values snap by default (the tool bar's grid button).
    pub snap: bool,
    /// The steps beside it: position, angle and scale, as they read.
    pub steps: [&'a str; 3],
    /// A step's menu of values, while open: which step, and the values.
    pub step_menu: Option<(usize, &'a [MenuItem<'a>])>,
    /// What rotating and scaling turn about (the tool bar's pivot button).
    pub pivot: &'a str,
    /// The scene dock's add menu, while open: what it offers, and the group open beside it.
    pub add_menu: Option<(&'a [MenuEntry<'a>], Option<usize>)>,
    /// The scene tree's row to scroll into view.
    pub reveal_row: Option<usize>,
    /// The scene tree's row a file dragged from the FileSystem dock would drop onto
    /// (outlined).
    pub drop_row: Option<usize>,
    /// What the scene tree is filtered by.
    pub tree_filter: &'a str,
    /// What the inspector is filtered by.
    pub inspector_filter: &'a str,
    /// The command palette, while open.
    pub palette: Option<PaletteView<'a>>,
    /// A hint by the pointer, and where (logical pixels): a keyboard transform's.
    pub hint: Option<(&'a str, [f32; 2])>,
}

/// What happened in the shell this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShellResponse {
    /// Id of the viewport area; its rectangle after layout comes from [`Ui::rect_of`].
    pub viewport: u64,
    /// A scene tab was clicked.
    pub scene_tab: Option<usize>,
    /// A scene tab's close mark was clicked.
    pub close_tab: Option<usize>,
    /// A tool was clicked.
    pub tool: Option<usize>,
    /// A scene tree row was clicked.
    pub tree_row: Option<usize>,
    /// A scene tree row's arrow was clicked: open or close it.
    pub tree_toggle: Option<usize>,
    /// A scene tree row's eye was clicked: turn it off or on.
    pub tree_eye: Option<usize>,
    /// The mouse is over the scene tree (where its keys go after a click).
    pub tree_hovered: bool,
    /// Renaming in the tree ended: committed (the name is `Ui::edited_text`) or cancelled.
    pub rename: Option<bool>,
    /// The tree's filter field was used (`Ui::edited_text` while it changes).
    pub tree_filter: Option<TextFieldResponse>,
    /// The inspector's filter field was used.
    pub inspector_filter: Option<TextFieldResponse>,
    /// The inspector heading's Open was clicked: the selection's prefab in a tab.
    pub open_prefab: bool,
    /// The inspector field under the mouse (section, field): Backspace resets it, Ctrl+C
    /// and Ctrl+V copy and paste its value.
    pub hovered_field: Option<(usize, usize)>,
    /// The command palette was used.
    pub palette: Option<PaletteAction>,
    /// A folder was clicked.
    pub folder: Option<usize>,
    /// A file was clicked.
    pub file: Option<usize>,
    /// A file is dragged out of the listing: held and moved past [`DRAG_START`].
    pub file_drag: Option<usize>,
    /// The listing's scroll area: its rectangle and scroll after the frame say which tiles
    /// show.
    pub listing: u64,
    /// The mouse is over the listing (where F2 renames a file after a click in it).
    pub listing_hovered: bool,
    /// The listing's tile under the pointer, whatever the mouse holds (a folder's: where
    /// a dragged file would move to).
    pub listing_under: Option<usize>,
    /// The folder tree's row under the pointer, the same way.
    pub folder_under: Option<usize>,
    /// Renaming a file in place ended: committed (the name is `Ui::edited_text`) or
    /// cancelled.
    pub file_rename: Option<bool>,
    /// The scene tree's row under the pointer, whatever the mouse holds: where a drop
    /// lands.
    pub tree_under: Option<usize>,
    /// A bottom panel tab was clicked.
    pub bottom_tab: Option<usize>,
    /// The Output's filter field was used (`Ui::edited_text` while it changes).
    pub output_filter: Option<TextFieldResponse>,
    /// An Output level toggle was clicked (0 errors … 3 the rest).
    pub output_level: Option<usize>,
    /// The Output's Follow was clicked.
    pub output_follow: bool,
    /// The Output's Clear was clicked.
    pub output_clear: bool,
    /// An Output row was clicked: its place among the rows shown.
    pub output_row: Option<usize>,
    /// Play (F5).
    pub play: bool,
    /// Pause (F7).
    pub pause: bool,
    /// Stop (F8).
    pub stop: bool,
    /// An inspector field was used.
    pub field: Option<FieldEvent>,
    /// The scene dock's add button.
    pub add: bool,
    /// A kind picked in the open add menu.
    pub add_item: Option<usize>,
    /// The add menu's group the pointer is over: opened beside it.
    pub add_group: Option<usize>,
    /// The tool bar's snapping button.
    pub snap: bool,
    /// A step beside it (0 position, 1 angle, 2 scale): open or close its menu.
    pub step: Option<usize>,
    /// A value of the open step's menu.
    pub step_item: Option<usize>,
    /// The tool bar's pivot button: the next pivot.
    pub pivot: bool,
    /// The popup open this frame (the add menu, a choice's list) and its opener, for
    /// [`dismissed`].
    pub popup: Option<(u64, u64)>,
    /// A menu opened beside the popup (the add menu's group), part of it for [`dismissed`].
    pub popup_beside: Option<u64>,
}

/// UI text size in pixels.
const TEXT: f32 = 13.0;
/// Small text: badges, hints.
const SMALL: f32 = 11.0;
/// Icon glyph size (Material Design icons fill the em box, so they read larger than text).
const ICON: f32 = 14.0;
/// Width of the scene and inspector docks.
const SCENE_WIDTH: f32 = 270.0;
const INSPECTOR_WIDTH: f32 = 320.0;
/// Height of the file browser under the viewport.
const FILES_HEIGHT: f32 = 190.0;
/// Indentation per tree level.
const INDENT: f32 = 14.0;
/// How far a held file moves before it is dragged, in logical pixels.
pub const DRAG_START: f32 = 4.0;
/// The height of an asset field's row, and the side of its preview.
const ASSET_ROW: f32 = 46.0;
const ASSET_PREVIEW: f32 = 40.0;

/// A tile's padding around its picture and name, the space between tiles, and the height
/// of its name's line.
const TILE_PAD: f32 = 4.0;
const TILE_GAP: f32 = 6.0;
const TILE_LABEL: f32 = 15.0;

/// How many tiles of picture side `tile` a row of a listing `width` wide holds (one at
/// least).
pub fn tile_columns(width: f32, tile: f32) -> usize {
    let room = width - 2.0 * TILE_GAP + TILE_GAP;
    ((room / (tile + 2.0 * TILE_PAD + TILE_GAP)).floor() as usize).max(1)
}

/// The height of a row of tiles of picture side `tile`, its gap included.
pub fn tile_row_height(tile: f32) -> f32 {
    tile + 2.0 * TILE_PAD + 2.0 + TILE_LABEL + TILE_GAP
}

fn text(ui: &mut Ui, key: &str, s: &str, size: f32, color: Color) -> u64 {
    ui.text_node(key, Style::default(), s, size, color, false)
}

/// An icon glyph in a column of one width, so the labels after icons line up (one text
/// node: no container around it).
fn icon(ui: &mut Ui, key: &str, glyph: &str, color: Color) {
    let column = Style {
        size: [Size::Fixed(ICON + 2.0), Size::Fit],
        ..Style::default()
    };
    ui.text_node(key, column, glyph, ICON, color, false);
}

/// Builds the shell into `ui` (between `begin_frame` and `end_frame`).
pub fn build(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette) -> ShellResponse {
    engine_profiler::zone!("editor::shell");
    let mut out = ShellResponse::default();
    // No background: the gaps between docks show the window's clear color
    // (`Palette::window`), and the viewport area stays open for the 3D view under the UI.
    let root = Style {
        direction: Direction::Column,
        gap: 1.0,
        ..Style::fill()
    };
    let mut popups = Popups::default();
    ui.container("shell", root, |ui| {
        ui.row(
            "main",
            Style {
                gap: 1.0,
                ..Style::fill()
            },
            |ui| {
                center(ui, model, p, &mut out, &mut popups);
                scene_dock(ui, model, p, &mut out, &mut popups);
                inspector_dock(ui, model, p, &mut out, &mut popups);
            },
        );
        if let Some(view) = &model.output {
            output_panel(ui, view, p, &mut out);
        }
        bottom_bar(ui, model, p, &mut out);
        if let Some(view) = &model.palette {
            palette(ui, view, p, &mut out, &mut popups);
        }
        if let Some((s, at)) = model.hint {
            hint(ui, s, at, p);
        }
    });
    out.popup = popups.open;
    out.popup_beside = popups.beside;
    out
}

/// Whether the mouse went down this frame outside the open popup and its opener: the
/// popup closes. Call after `Ui::end_frame`, when the popup has its place in this frame's
/// layout.
pub fn dismissed(ui: &Ui, response: &ShellResponse) -> bool {
    response.popup.is_some_and(|(popup, opener)| {
        ui.input().mouse_pressed
            && !ui.hovers(popup)
            && !ui.hovers(opener)
            && !response.popup_beside.is_some_and(|b| ui.hovers(b))
    })
}

/// The popup open this frame, if any: its id and its opener's, and a menu opened beside it.
#[derive(Default)]
struct Popups {
    open: Option<(u64, u64)>,
    beside: Option<u64>,
}

/// Scene tabs with the play controls, tool bar, viewport and file browser.
fn center(
    ui: &mut Ui,
    model: &ShellModel<'_>,
    p: &Palette,
    out: &mut ShellResponse,
    popups: &mut Popups,
) {
    let column = Style {
        direction: Direction::Column,
        gap: 1.0,
        ..Style::fill()
    };
    ui.container("center", column, |ui| {
        ui.row("scene tabs", bar(p, 30.0), |ui| {
            scene_tabs(ui, model, p, out);
            ui.spacer("fill");
            play_controls(ui, model.playing, model.paused, p, out);
        });
        ui.row("tool bar", bar(p, 30.0).background(p.dock), |ui| {
            for (i, tool) in model.tools.iter().enumerate() {
                ui.push_id(i as u64);
                let selected = i == model.active_tool;
                let id = ui.container(
                    "tool",
                    Style {
                        padding: [8.0, 4.0, 8.0, 4.0],
                        radius: 3.0,
                        background: selected.then_some(p.selection),
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        let color = if selected { p.text } else { p.text_dim };
                        text(ui, "label", tool, TEXT, color);
                    },
                );
                if ui.sense(id, false).clicked {
                    out.tool = Some(i);
                }
                ui.pop_id();
            }
            // Snapping by default: the grid button (Shift inverts it while dragging, Ctrl
            // rounds to the grid).
            let id = ui.container(
                "snap",
                Style {
                    padding: [8.0, 4.0, 8.0, 4.0],
                    radius: 3.0,
                    background: model.snap.then_some(p.selection),
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    let color = if model.snap { p.text } else { p.text_dim };
                    text(ui, "icon", icons::SNAP, ICON, color);
                },
            );
            if ui.sense(id, false).clicked {
                out.snap = true;
            }
            // The steps it snaps to, each a menu of values (Unreal's tool bar), dim while
            // snapping is off.
            let glyphs = [icons::TOOL_MOVE, icons::TOOL_ROTATE, icons::TOOL_SCALE];
            for (i, (&glyph, &label)) in glyphs.iter().zip(&model.steps).enumerate() {
                ui.push_id(i as u64);
                let open = model.step_menu.filter(|&(k, _)| k == i);
                let opener = ui.container(
                    "step",
                    Style {
                        padding: [6.0, 4.0, 6.0, 4.0],
                        gap: 3.0,
                        radius: 3.0,
                        background: open.is_some().then_some(p.selection),
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        text(ui, "icon", glyph, ICON, p.text_dim);
                        let color = if model.snap { p.text } else { p.text_dim };
                        text(ui, "label", label, TEXT, color);
                        if let Some((_, items)) = open {
                            let items = items.iter().map(|m| (m.icon, m.label));
                            let popup = menu(ui, items, p, [0.0, 26.0], &mut out.step_item);
                            popups.open = Some((popup, 0));
                        }
                    },
                );
                if open.is_some()
                    && let Some((popup, _)) = popups.open
                {
                    popups.open = Some((popup, opener));
                }
                if ui.sense(opener, false).clicked {
                    out.step = Some(i);
                }
                ui.pop_id();
            }
            ui.spacer("fill");
            let id = ui.container(
                "pivot",
                Style {
                    padding: [8.0, 4.0, 8.0, 4.0],
                    radius: 3.0,
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "label", model.pivot, TEXT, p.text_dim);
                },
            );
            if ui.sense(id, false).clicked {
                out.pivot = true;
            }
        });
        let edge = model.playing.then_some((p.playing, 2.0));
        out.viewport = ui.container(
            "viewport",
            Style {
                border: edge,
                padding: [8.0; 4],
                ..Style::fill()
            },
            |ui| {
                ui.row(
                    "view menu",
                    Style {
                        padding: [8.0, 3.0, 8.0, 3.0],
                        radius: 3.0,
                        background: Some([0.0, 0.0, 0.0, 0.45]),
                        ..Style::default()
                    },
                    |ui| {
                        text(ui, "label", "Perspective", SMALL, p.text);
                    },
                );
            },
        );
        file_browser(ui, model, p, out);
    });
}

/// A header bar: fixed height, centered children, padding.
fn bar(p: &Palette, height: f32) -> Style {
    Style {
        size: [Size::Grow(1.0), Size::Fixed(height)],
        padding: [4.0, 0.0, 6.0, 0.0],
        gap: 2.0,
        align_cross: Align::Center,
        background: Some(p.header),
        ..Style::default()
    }
}

/// A strip of tabs; returns the clicked one.
fn tabs(ui: &mut Ui, labels: &[&str], active: Option<usize>, p: &Palette) -> Option<usize> {
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        ui.push_id(i as u64);
        let on = active == Some(i);
        let id = ui.container(
            "tab",
            Style {
                padding: [10.0, 5.0, 10.0, 5.0],
                radius: 3.0,
                background: on.then_some(p.dock),
                align_cross: Align::Center,
                ..Style::default()
            },
            |ui| {
                let color = if on { p.text } else { p.text_dim };
                text(ui, "label", label, TEXT, color);
            },
        );
        if ui.sense(id, false).clicked {
            clicked = Some(i);
        }
        ui.pop_id();
    }
    clicked
}

/// The scene tabs: each a label and, while several are open, a close mark beside it.
fn scene_tabs(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    let closable = model.scenes.len() > 1;
    for (i, label) in model.scenes.iter().enumerate() {
        ui.push_id(i as u64);
        let on = model.active_scene == i;
        let mut close = None;
        let id = ui.container(
            "tab",
            Style {
                padding: [10.0, 5.0, 8.0, 5.0],
                gap: 6.0,
                radius: 3.0,
                background: on.then_some(p.dock),
                align_cross: Align::Center,
                ..Style::default()
            },
            |ui| {
                let color = if on { p.text } else { p.text_dim };
                text(ui, "label", label, TEXT, color);
                if closable {
                    close = Some(ui.container(
                        "close",
                        Style {
                            align_cross: Align::Center,
                            ..Style::default()
                        },
                        |ui| {
                            text(ui, "icon", icons::CLOSE, ICON, p.text_dim);
                        },
                    ));
                }
            },
        );
        if close.is_some_and(|c| ui.sense(c, false).clicked) {
            out.close_tab = Some(i);
        } else if ui.sense(id, false).clicked {
            out.scene_tab = Some(i);
        }
        ui.pop_id();
    }
}

/// Small monochrome play, pause and stop at the end of the tab row (not a colored group in
/// the middle of the window).
fn play_controls(ui: &mut Ui, playing: bool, paused: bool, p: &Palette, out: &mut ShellResponse) {
    let button = Style {
        size: [Size::Fixed(24.0), Size::Fixed(22.0)],
        radius: 3.0,
        align_main: Align::Center,
        align_cross: Align::Center,
        ..Style::default()
    };
    let play_color = if playing { p.playing } else { p.text_dim };
    let id = ui.container("play", button, |ui| {
        text(ui, "icon", icons::PLAY, ICON, play_color);
    });
    out.play = ui.sense(id, false).clicked;
    let pause_color = if paused { p.playing } else { p.text_dim };
    let id = ui.container("pause", button, |ui| {
        text(ui, "icon", icons::PAUSE, ICON, pause_color);
    });
    out.pause = ui.sense(id, false).clicked;
    let id = ui.container("stop", button, |ui| {
        text(ui, "icon", icons::STOP, ICON, p.text_dim);
    });
    out.stop = ui.sense(id, false).clicked;
}

/// Folder tree and the current folder's files, under the viewport.
fn file_browser(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    let row = Style {
        size: [Size::Grow(1.0), Size::Fixed(FILES_HEIGHT)],
        gap: 1.0,
        ..Style::default()
    };
    ui.row("files", row, |ui| {
        dock_column(ui, "folders", Size::Fixed(220.0), p, |ui| {
            dock_title(ui, "FileSystem", p);
            ui.scroll_column("tree", |ui| {
                let rows = tree_rows(ui, model.folders, p, None, None, false, true);
                out.folder = rows.clicked;
                out.folder_under = rows.under;
            });
        });
        dock_column(ui, "listing", Size::Grow(1.0), p, |ui| {
            dock_title(ui, model.path, p);
            out.listing = ui.scroll_column("list", |ui| {
                let columns = model.columns.max(1);
                for (r, row) in model.files.chunks(columns).enumerate() {
                    ui.push_id(r as u64);
                    let tiles = Style {
                        size: [Size::Grow(1.0), Size::Fit],
                        padding: [TILE_GAP, TILE_GAP / 2.0, TILE_GAP, TILE_GAP / 2.0],
                        gap: TILE_GAP,
                        ..Style::default()
                    };
                    ui.row("tiles", tiles, |ui| {
                        for (k, file) in row.iter().enumerate() {
                            let i = r * columns + k;
                            ui.push_id(k as u64);
                            let id = file_tile(ui, file, model.tile, p, &mut out.file_rename);
                            ui.mark_region(id);
                            if ui.in_region(id) {
                                out.listing_under = Some(i);
                            }
                            if ui.sense(id, false).clicked {
                                out.file = Some(i);
                            }
                            if ui
                                .drag_delta(id)
                                .is_some_and(|[x, y]| x * x + y * y > DRAG_START * DRAG_START)
                            {
                                out.file_drag = Some(i);
                            }
                            ui.pop_id();
                        }
                    });
                    ui.pop_id();
                }
            });
            ui.mark_region(out.listing);
            out.listing_hovered = ui.in_region(out.listing);
        });
    });
}

/// An asset field's box: its preview (its kind's icon until there is one) beside its name.
/// Whether it was clicked.
fn asset_box(ui: &mut Ui, preview: Option<UiImage>, icon: &str, name: &str, p: &Palette) -> bool {
    let id = ui.row(
        "asset",
        Style {
            size: [Size::Grow(1.0), Size::Fixed(ASSET_PREVIEW + 4.0)],
            padding: [2.0, 2.0, 6.0, 2.0],
            gap: 8.0,
            radius: 3.0,
            background: Some(p.header),
            align_cross: Align::Center,
            clip: true,
            ..Style::default()
        },
        |ui| {
            let picture = Style {
                size: [Size::Fixed(ASSET_PREVIEW), Size::Fixed(ASSET_PREVIEW)],
                radius: 2.0,
                background: Some(p.dock),
                align_main: Align::Center,
                align_cross: Align::Center,
                image: preview,
                ..Style::default()
            };
            ui.row("preview", picture, |ui| {
                if preview.is_none() && !icon.is_empty() {
                    text(ui, "icon", icon, ICON * 1.4, p.text_dim);
                }
            });
            text(ui, "name", name, TEXT, p.text);
        },
    );
    ui.sense(id, false).clicked
}

/// A file's tile: its thumbnail (its kind's icon until there is one) over its name, the
/// name a text field while it is renamed (`rename` says how that ended).
fn file_tile(
    ui: &mut Ui,
    file: &FileItem<'_>,
    tile: f32,
    p: &Palette,
    rename: &mut Option<bool>,
) -> u64 {
    ui.column(
        "tile",
        Style {
            size: [Size::Fixed(tile + 2.0 * TILE_PAD), Size::Fit],
            padding: [TILE_PAD; 4],
            gap: 2.0,
            align_cross: Align::Center,
            radius: 4.0,
            background: file.selected.then_some(p.selection),
            ..Style::default()
        },
        |ui| {
            let picture = Style {
                size: [Size::Fixed(tile), Size::Fixed(tile)],
                align_main: Align::Center,
                align_cross: Align::Center,
                image: file.thumbnail,
                ..Style::default()
            };
            ui.row("picture", picture, |ui| {
                if file.thumbnail.is_none() {
                    let (glyph, color) = file_icon(file.kind, p);
                    text(ui, "icon", glyph, (tile * 0.45).max(ICON), color);
                }
            });
            if file.renaming {
                let style = Style {
                    size: [
                        Size::Fixed(tile + 2.0 * TILE_PAD),
                        Size::Fixed(TILE_LABEL + 2.0),
                    ],
                    padding: [3.0, 0.0, 3.0, 0.0],
                    radius: 2.0,
                    background: Some(p.field),
                    align_cross: Align::Center,
                    ..Style::default()
                };
                let r = ui.text_field("rename", file.name, &field_look(p, style, "", true));
                if r.committed || r.cancelled {
                    *rename = Some(r.committed);
                }
                return;
            }
            ui.row(
                "label",
                Style {
                    size: [Size::Fixed(tile), Size::Fixed(TILE_LABEL)],
                    align_main: Align::Center,
                    align_cross: Align::Center,
                    clip: true,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "name", file.name, SMALL, p.text);
                },
            );
        },
    )
}

/// Icon glyph and its color for a file kind.
fn file_icon(kind: FileKind, p: &Palette) -> (&'static str, Color) {
    match kind {
        FileKind::Folder => (icons::FOLDER, srgb(214, 170, 90)),
        FileKind::Scene => (icons::SCENE, p.accent),
        FileKind::Script => (icons::SCRIPT, srgb(222, 196, 96)),
        FileKind::Texture => (icons::TEXTURE, srgb(110, 190, 130)),
        FileKind::Mesh => (icons::MESH, srgb(222, 140, 90)),
        FileKind::Audio => (icons::AUDIO, srgb(180, 130, 220)),
        FileKind::Other => (icons::FILE, p.text_dim),
    }
}

/// A dock: a column with the dock background.
fn dock_column(ui: &mut Ui, key: &str, width: Size, p: &Palette, f: impl FnOnce(&mut Ui)) -> u64 {
    ui.container(
        key,
        Style {
            size: [width, Size::Grow(1.0)],
            direction: Direction::Column,
            background: Some(p.dock),
            ..Style::default()
        },
        f,
    )
}

/// A dock's small heading line.
fn dock_title(ui: &mut Ui, title: &str, p: &Palette) {
    ui.row(
        "title",
        Style {
            size: [Size::Grow(1.0), Size::Fixed(24.0)],
            padding: [8.0, 0.0, 8.0, 0.0],
            align_cross: Align::Center,
            ..Style::default()
        },
        |ui| {
            text(ui, "text", title, SMALL, p.text_dim);
        },
    );
}

/// A text field in the shell's look: `Style` for the box, the theme's text.
fn field_look<'a>(p: &Palette, style: Style, hint: &'a str, autofocus: bool) -> TextField<'a> {
    TextField {
        style,
        size: TEXT,
        color: p.text,
        hint,
        hint_color: p.text_dim,
        autofocus,
        search: false,
    }
}

/// What a text field did, if anything worth reporting.
fn used(r: TextFieldResponse) -> Option<TextFieldResponse> {
    (r.started || r.changed || r.committed || r.cancelled).then_some(r)
}

/// A filter box with a hint: a text field the caller filters by as it changes.
fn filter(ui: &mut Ui, text_now: &str, hint: &str, p: &Palette) -> Option<TextFieldResponse> {
    let mut response = None;
    ui.container(
        "filter row",
        Style {
            size: [Size::Grow(1.0), Size::Fit],
            padding: [6.0, 4.0, 6.0, 6.0],
            ..Style::default()
        },
        |ui| {
            ui.row(
                "filter",
                Style {
                    size: [Size::Grow(1.0), Size::Fixed(24.0)],
                    padding: [8.0, 0.0, 4.0, 0.0],
                    gap: 6.0,
                    radius: 3.0,
                    background: Some(p.field),
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", icons::SEARCH, ICON, p.text_dim);
                    let style = Style {
                        size: [Size::Grow(1.0), Size::Fixed(22.0)],
                        align_cross: Align::Center,
                        ..Style::default()
                    };
                    response =
                        used(ui.text_field("text", text_now, &field_look(p, style, hint, false)));
                },
            );
        },
    );
    response
}

/// What the tree's rows reported.
#[derive(Default)]
struct RowsResponse {
    clicked: Option<usize>,
    /// The row under the pointer (by the last layout, whatever the mouse holds).
    under: Option<usize>,
    toggled: Option<usize>,
    eye: Option<usize>,
    rename: Option<bool>,
}

/// Tree rows: a click on a row, on its arrow, on its eye (`eyes`: the scene tree's rows
/// end in one), and renaming in place; row `target` outlined as where a drop lands; with
/// `regions` each row a hover region (the row under a drag, [`RowsResponse::under`]).
fn tree_rows(
    ui: &mut Ui,
    rows: &[TreeItem<'_>],
    p: &Palette,
    reveal: Option<usize>,
    target: Option<usize>,
    eyes: bool,
    regions: bool,
) -> RowsResponse {
    let mut out = RowsResponse::default();
    for (i, item) in rows.iter().enumerate() {
        ui.push_id(i as u64);
        let id = ui.row(
            "row",
            Style {
                size: [Size::Grow(1.0), Size::Fixed(22.0)],
                padding: [6.0 + INDENT * f32::from(item.depth), 0.0, 6.0, 0.0],
                gap: 5.0,
                align_cross: Align::Center,
                background: if item.active {
                    Some(p.selection)
                } else {
                    item.selected.then_some(p.selection_dim)
                },
                border: (target == Some(i)).then_some((p.accent, 1.0)),
                ..Style::default()
            },
            |ui| {
                let arrow = match item.expanded {
                    Some(true) => icons::EXPANDED,
                    Some(false) => icons::COLLAPSED,
                    None => "",
                };
                let toggle = ui.container(
                    "arrow",
                    Style {
                        size: [Size::Fixed(ICON), Size::Fit],
                        ..Style::default()
                    },
                    |ui| {
                        if !arrow.is_empty() {
                            text(ui, "glyph", arrow, ICON, p.text_dim);
                        }
                    },
                );
                if !arrow.is_empty() && ui.sense(toggle, false).clicked {
                    out.toggled = Some(i);
                }
                if !item.icon.is_empty() {
                    icon(ui, "icon", item.icon, p.text_dim);
                }
                if item.renaming {
                    let style = Style {
                        size: [Size::Grow(1.0), Size::Fixed(20.0)],
                        padding: [4.0, 0.0, 4.0, 0.0],
                        radius: 3.0,
                        background: Some(p.field),
                        align_cross: Align::Center,
                        ..Style::default()
                    };
                    let r = ui.text_field("rename", item.name, &field_look(p, style, "", true));
                    if r.committed || r.cancelled {
                        out.rename = Some(r.committed);
                    }
                } else {
                    let color = if item.off { p.text_dim } else { p.text };
                    text(ui, "name", item.name, TEXT, color);
                }
                if !item.kind.is_empty() {
                    text(ui, "kind", item.kind, SMALL, p.text_dim);
                }
                if !eyes {
                    return;
                }
                ui.spacer("fill");
                // The eye: on or off, as Godot's.
                let eye = ui.container(
                    "eye",
                    Style {
                        size: [Size::Fixed(ICON + 4.0), Size::Fit],
                        align_main: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        let glyph = if item.disabled {
                            icons::EYE_OFF
                        } else {
                            icons::EYE
                        };
                        text(ui, "glyph", glyph, ICON, p.text_dim);
                    },
                );
                if ui.sense(eye, false).clicked {
                    out.eye = Some(i);
                }
            },
        );
        if ui.sense(id, false).clicked && out.toggled != Some(i) && out.eye != Some(i) {
            out.clicked = Some(i);
        }
        if reveal == Some(i) {
            ui.reveal(id);
        }
        // Hover regions: where a dragged file would drop.
        if regions {
            ui.mark_region(id);
            if ui.in_region(id) {
                out.under = Some(i);
            }
        }
        ui.pop_id();
    }
    out
}

/// Scene tree dock: tabs and the add button, filter, tree.
fn scene_dock(
    ui: &mut Ui,
    model: &ShellModel<'_>,
    p: &Palette,
    out: &mut ShellResponse,
    popups: &mut Popups,
) {
    dock_column(ui, "scene dock", Size::Fixed(SCENE_WIDTH), p, |ui| {
        ui.row("tabs", bar(p, 30.0), |ui| {
            tabs(ui, &["Scene", "History"], Some(0), p);
            ui.spacer("fill");
            let opener = ui.container(
                "add",
                Style {
                    size: [Size::Fixed(24.0), Size::Fixed(22.0)],
                    radius: 3.0,
                    background: model.add_menu.is_some().then_some(p.selection),
                    align_main: Align::Center,
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", icons::PLUS, ICON, p.text);
                    if let Some((entries, open)) = model.add_menu {
                        let (popup, beside) = group_menu(
                            ui,
                            entries,
                            open,
                            p,
                            [-150.0, 24.0],
                            &mut out.add_item,
                            &mut out.add_group,
                        );
                        popups.open = Some((popup, ui.id("add")));
                        popups.beside = beside;
                    }
                },
            );
            if let Some((popup, _)) = popups.open {
                popups.open = Some((popup, opener));
            }
            if ui.sense(opener, false).clicked {
                out.add = true;
            }
        });
        out.tree_filter = filter(ui, model.tree_filter, "Filter nodes", p);
        let tree = ui.scroll_column("tree", |ui| {
            let rows = tree_rows(
                ui,
                model.tree,
                p,
                model.reveal_row,
                model.drop_row,
                true,
                true,
            );
            out.tree_row = rows.clicked;
            out.tree_under = rows.under;
            out.tree_toggle = rows.toggled;
            out.tree_eye = rows.eye;
            out.rename = rows.rename;
        });
        ui.mark_region(tree);
        out.tree_hovered = ui.in_region(tree);
    });
}

/// A floating hint at `at`: what a keyboard transform does.
fn hint(ui: &mut Ui, s: &str, at: [f32; 2], p: &Palette) {
    ui.row(
        "hint",
        Style {
            floating: Some(at),
            padding: [6.0, 3.0, 6.0, 3.0],
            radius: 4.0,
            background: Some(p.header),
            border: Some((p.field_hover, 1.0)),
            align_cross: Align::Center,
            ..Style::default()
        },
        |ui| {
            text(ui, "text", s, TEXT, p.text);
        },
    );
}

/// The command palette: a floating box at the top of the window, its query field and the
/// best matches, the matched characters highlighted and each command's shortcut beside it.
fn palette(
    ui: &mut Ui,
    view: &PaletteView<'_>,
    p: &Palette,
    out: &mut ShellResponse,
    popups: &mut Popups,
) {
    const WIDTH: f32 = 560.0;
    let x = ((ui.viewport()[0] - WIDTH) / 2.0).max(8.0);
    let mut action = None;
    let panel = ui.column(
        "palette",
        Style {
            floating: Some([x, 38.0]),
            size: [Size::Fixed(WIDTH), Size::Fit],
            padding: [6.0; 4],
            gap: 4.0,
            radius: 6.0,
            background: Some(p.header),
            border: Some((p.accent, 1.0)),
            ..Style::default()
        },
        |ui| {
            let style = Style {
                size: [Size::Grow(1.0), Size::Fixed(26.0)],
                padding: [8.0, 0.0, 8.0, 0.0],
                radius: 4.0,
                background: Some(p.field),
                align_cross: Align::Center,
                ..Style::default()
            };
            let look = TextField {
                search: true,
                ..field_look(p, style, view.hint, true)
            };
            let r = ui.text_field("query", view.query, &look);
            if r.changed {
                action = Some(PaletteAction::Query);
            }
            // Enter runs the highlighted row. A click elsewhere also ends the edit: on a
            // row, the row runs when the click ends; outside, the palette is dismissed
            // like any popup ([`dismissed`]). Meanwhile the box takes the focus again.
            if r.committed && ui.input().keys.contains(&UiKey::Enter) {
                action = Some(PaletteAction::Run(view.selected));
            }
            if r.cancelled {
                action = Some(PaletteAction::Close);
            }
            if r.editing && action.is_none() {
                for key in &ui.input().keys {
                    match key {
                        UiKey::Up => action = Some(PaletteAction::Move(-1)),
                        UiKey::Down => action = Some(PaletteAction::Move(1)),
                        _ => {}
                    }
                }
            }
            if view.rows.is_empty() && !view.query.trim().is_empty() {
                ui.row(
                    "none",
                    Style {
                        size: [Size::Grow(1.0), Size::Fixed(24.0)],
                        padding: [8.0, 0.0, 8.0, 0.0],
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        text(ui, "text", "No matches", TEXT, p.text_dim);
                    },
                );
            }
            for (i, row) in view.rows.iter().enumerate() {
                ui.push_id(i as u64);
                let id = ui.id("row");
                let background = if i == view.selected {
                    Some(p.selection)
                } else {
                    ui.is_hovered(id).then_some(p.field_hover)
                };
                ui.row(
                    "row",
                    Style {
                        size: [Size::Grow(1.0), Size::Fixed(24.0)],
                        padding: [8.0, 0.0, 8.0, 0.0],
                        radius: 3.0,
                        background,
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        if !row.icon.is_empty() {
                            icon(ui, "icon", row.icon, p.text_dim);
                            ui.container(
                                "gap",
                                Style::default().size(Size::Fixed(6.0), Size::Fit),
                                |_| {},
                            );
                        }
                        label_runs(ui, row, p);
                        ui.spacer("fill");
                        text(ui, "detail", row.detail, SMALL, p.text_dim);
                    },
                );
                if ui.sense(id, false).clicked {
                    action = Some(PaletteAction::Run(i));
                }
                ui.pop_id();
            }
        },
    );
    popups.open = Some((panel, panel));
    out.palette = action;
}

/// A palette row's label as runs: the category dimmed, matched characters in the accent.
fn label_runs(ui: &mut Ui, row: &PaletteRow<'_>, p: &Palette) {
    let label = row.label;
    let mut start = 0;
    let mut run = 0;
    let class = |i: usize| {
        if row.matched.contains(&(i as u16)) {
            2
        } else if i < row.category {
            0
        } else {
            1
        }
    };
    let mut current = None;
    for (i, (at, _)) in label.char_indices().enumerate() {
        let c = class(i);
        if current != Some(c) {
            if let Some(prev) = current {
                // Spaces ending a run start the next one: a text's trailing spaces take no
                // width, its leading ones do.
                let end = start + label[start..at].trim_end().len();
                if end > start {
                    run_text(ui, run, &label[start..end], prev, p);
                    run += 1;
                    start = end;
                }
            }
            current = Some(c);
        }
    }
    if let Some(prev) = current {
        run_text(ui, run, &label[start..], prev, p);
    }
}

fn run_text(ui: &mut Ui, run: u64, s: &str, class: u8, p: &Palette) {
    let color = match class {
        0 => p.text_dim,
        2 => p.accent,
        _ => p.text,
    };
    ui.push_id(run);
    text(ui, "run", s, TEXT, color);
    ui.pop_id();
}

/// A floating list of items under its opener, `at` from the opener's corner; reports
/// the clicked item. Returns the list's id.
fn menu<'a>(
    ui: &mut Ui,
    items: impl IntoIterator<Item = (&'a str, &'a str)>,
    p: &Palette,
    at: [f32; 2],
    clicked: &mut Option<usize>,
) -> u64 {
    ui.column(
        "menu",
        Style {
            floating: Some(at),
            size: [Size::Fixed(174.0), Size::Fit],
            padding: [0.0, 4.0, 0.0, 4.0],
            radius: 4.0,
            background: Some(p.header),
            border: Some((p.field_hover, 1.0)),
            ..Style::default()
        },
        |ui| {
            for (i, (glyph, label)) in items.into_iter().enumerate() {
                ui.push_id(i as u64);
                let id = ui.id("item");
                let hovered = ui.is_hovered(id);
                ui.row(
                    "item",
                    Style {
                        size: [Size::Grow(1.0), Size::Fixed(24.0)],
                        padding: [10.0, 0.0, 10.0, 0.0],
                        align_cross: Align::Center,
                        background: hovered.then_some(p.selection),
                        gap: 8.0,
                        ..Style::default()
                    },
                    |ui| {
                        if !glyph.is_empty() {
                            icon(ui, "icon", glyph, p.text_dim);
                        }
                        text(ui, "label", label, TEXT, p.text);
                    },
                );
                if ui.sense(id, false).clicked {
                    *clicked = Some(i);
                }
                ui.pop_id();
            }
        },
    )
}

/// The width of a menu.
const MENU_WIDTH: f32 = 174.0;

/// A menu of kinds and groups at `at`: a group's entries open beside it while the pointer
/// is over it (`open`, reported through `hovered`). Returns the menu's id and the open
/// group's.
fn group_menu(
    ui: &mut Ui,
    entries: &[MenuEntry<'_>],
    open: Option<usize>,
    p: &Palette,
    at: [f32; 2],
    clicked: &mut Option<usize>,
    hovered: &mut Option<usize>,
) -> (u64, Option<u64>) {
    let mut beside = None;
    let id = ui.column(
        "menu",
        Style {
            floating: Some(at),
            size: [Size::Fixed(MENU_WIDTH), Size::Fit],
            padding: [0.0, 4.0, 0.0, 4.0],
            radius: 4.0,
            background: Some(p.header),
            border: Some((p.field_hover, 1.0)),
            ..Style::default()
        },
        |ui| {
            for (i, entry) in entries.iter().enumerate() {
                ui.push_id(i as u64);
                let item = ui.id("item");
                let is_open = open == Some(i) && !entry.children.is_empty();
                let lit = ui.is_hovered(item) || is_open;
                ui.row(
                    "item",
                    Style {
                        size: [Size::Grow(1.0), Size::Fixed(24.0)],
                        padding: [10.0, 0.0, 8.0, 0.0],
                        align_cross: Align::Center,
                        background: lit.then_some(p.selection),
                        gap: 8.0,
                        ..Style::default()
                    },
                    |ui| {
                        if !entry.icon.is_empty() {
                            icon(ui, "icon", entry.icon, p.text_dim);
                        }
                        text(ui, "label", entry.label, TEXT, p.text);
                        if !entry.children.is_empty() {
                            ui.spacer("fill");
                            text(ui, "more", icons::COLLAPSED, ICON, p.text_dim);
                        }
                        if is_open {
                            let mut picked = None;
                            let sub = menu(
                                ui,
                                entry.children.iter().map(|c| (c.icon, c.label)),
                                p,
                                [MENU_WIDTH, -4.0],
                                &mut picked,
                            );
                            beside = Some(sub);
                            if let Some(k) = picked {
                                *clicked = Some(entry.children[k].kind);
                            }
                        }
                    },
                );
                if ui.is_hovered(item) && !entry.children.is_empty() {
                    *hovered = Some(i);
                }
                if ui.sense(item, false).clicked {
                    if entry.children.is_empty() {
                        *clicked = Some(entry.kind);
                    } else {
                        *hovered = Some(i);
                    }
                }
                ui.pop_id();
            }
        },
    );
    (id, beside)
}

/// A field's numbers, dragged sideways; reports the drag. The part being typed is a
/// text field instead.
fn numbers(
    ui: &mut Ui,
    values: &[&str],
    labels: &[&str],
    typing: Option<u8>,
    p: &Palette,
    action: &mut Option<FieldAction>,
) {
    const AXES: [(u8, u8, u8); 4] = [
        (230, 96, 96),
        (130, 200, 96),
        (96, 150, 240),
        (170, 170, 180),
    ];
    ui.row(
        "numbers",
        Style {
            size: [Size::Grow(1.0), Size::Fixed(20.0)],
            gap: 3.0,
            ..Style::default()
        },
        |ui| {
            for (i, value) in values.iter().enumerate() {
                ui.push_id(i as u64);
                if typing == Some(i as u8) {
                    let style = Style {
                        size: [Size::Grow(1.0), Size::Fixed(20.0)],
                        padding: [5.0, 0.0, 5.0, 0.0],
                        radius: 3.0,
                        background: Some(p.field_hover),
                        border: Some((p.accent, 1.0)),
                        align_cross: Align::Center,
                        ..Style::default()
                    };
                    let r = ui.text_field("typed", value, &field_look(p, style, "", true));
                    if r.committed || r.cancelled {
                        *action = Some(FieldAction::Typed {
                            part: i as u8,
                            commit: r.committed,
                        });
                    }
                    ui.pop_id();
                    continue;
                }
                let id = ui.id("number");
                let lit = ui.is_hovered(id) || ui.drag_delta(id).is_some();
                ui.row(
                    "number",
                    Style {
                        size: [Size::Grow(1.0), Size::Fixed(20.0)],
                        padding: [5.0, 0.0, 5.0, 0.0],
                        gap: 4.0,
                        radius: 3.0,
                        background: Some(if lit { p.field_hover } else { p.field }),
                        align_cross: Align::Center,
                        clip: true,
                        ..Style::default()
                    },
                    |ui| {
                        if let Some(label) = labels.get(i) {
                            let (r, g, b) = AXES[i.min(3)];
                            text(ui, "axis", label, SMALL, srgb(r, g, b));
                        }
                        text(ui, "value", value, TEXT, p.text);
                    },
                );
                let r = ui.sense(id, false);
                let part = i as u8;
                if r.pressed {
                    *action = Some(FieldAction::Drag {
                        part,
                        travel: 0.0,
                        start: true,
                    });
                } else if let Some([dx, _]) = ui.drag_delta(id) {
                    *action = Some(FieldAction::Drag {
                        part,
                        travel: dx,
                        start: false,
                    });
                } else if r.released {
                    *action = Some(FieldAction::Release);
                }
                ui.pop_id();
            }
        },
    );
}

/// A checkbox; reports a click.
fn checkbox(ui: &mut Ui, on: Option<bool>, p: &Palette, action: &mut Option<FieldAction>) {
    let checked = on == Some(true);
    let id = ui.container(
        "check",
        Style {
            size: [Size::Fixed(16.0), Size::Fixed(16.0)],
            radius: 3.0,
            background: Some(if checked { p.accent } else { p.field }),
            border: (!checked).then_some((p.text_dim, 1.0)),
            align_main: Align::Center,
            align_cross: Align::Center,
            ..Style::default()
        },
        |ui| match on {
            Some(true) => {
                text(ui, "mark", icons::CHECK, 13.0, p.text);
            }
            None => {
                text(ui, "mark", icons::MIXED, 13.0, p.text_dim);
            }
            Some(false) => {}
        },
    );
    if ui.sense(id, false).clicked {
        *action = Some(FieldAction::Toggle);
    }
}

/// An enum's variant with its list while open; reports a click or a pick.
fn choice(
    ui: &mut Ui,
    current: &str,
    open: Option<&[&str]>,
    p: &Palette,
    action: &mut Option<FieldAction>,
    popups: &mut Popups,
) {
    let id = ui.id("choice");
    let lit = ui.is_hovered(id) || open.is_some();
    ui.row(
        "choice",
        Style {
            size: [Size::Grow(1.0), Size::Fixed(20.0)],
            padding: [6.0, 0.0, 4.0, 0.0],
            radius: 3.0,
            background: Some(if lit { p.field_hover } else { p.field }),
            align_cross: Align::Center,
            ..Style::default()
        },
        |ui| {
            text(ui, "value", current, TEXT, p.text);
            ui.spacer("fill");
            text(ui, "arrow", icons::MENU_DOWN, ICON, p.text_dim);
            if let Some(options) = open {
                let mut picked = None;
                let items = options.iter().map(|&o| ("", o));
                let popup = menu(ui, items, p, [0.0, 22.0], &mut picked);
                popups.open = Some((popup, id));
                if let Some(i) = picked {
                    *action = Some(FieldAction::Choose(i));
                }
            }
        },
    );
    if ui.sense(id, false).clicked && action.is_none() {
        *action = Some(FieldAction::Open);
    }
}

/// Inspector dock: tabs, the selection, filter, sections of fields.
#[allow(clippy::too_many_lines)]
fn inspector_dock(
    ui: &mut Ui,
    model: &ShellModel<'_>,
    p: &Palette,
    out: &mut ShellResponse,
    popups: &mut Popups,
) {
    dock_column(
        ui,
        "inspector dock",
        Size::Fixed(INSPECTOR_WIDTH),
        p,
        |ui| {
            ui.row("tabs", bar(p, 30.0), |ui| {
                tabs(ui, &["Inspector", "Signals"], Some(0), p);
            });
            ui.row(
                "selection",
                Style {
                    size: [Size::Grow(1.0), Size::Fixed(28.0)],
                    padding: [10.0, 0.0, 10.0, 0.0],
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "name", model.selection, 14.0, p.text);
                    if model.prefab {
                        ui.spacer("fill");
                        let id = ui.container(
                            "open prefab",
                            Style {
                                padding: [8.0, 3.0, 8.0, 3.0],
                                gap: 4.0,
                                radius: 3.0,
                                background: Some(p.header),
                                align_cross: Align::Center,
                                ..Style::default()
                            },
                            |ui| {
                                text(ui, "icon", icons::SCENE, ICON, p.text_dim);
                                text(ui, "label", "Open", TEXT, p.text_dim);
                            },
                        );
                        if ui.sense(id, false).clicked {
                            out.open_prefab = true;
                        }
                    }
                },
            );
            out.inspector_filter = filter(ui, model.inspector_filter, "Filter properties", p);
            ui.scroll_column("sections", |ui| {
                for (s, section) in model.inspector.iter().enumerate() {
                    ui.push_id(s as u64);
                    ui.row(
                        "header",
                        Style {
                            size: [Size::Grow(1.0), Size::Fixed(24.0)],
                            padding: [8.0, 0.0, 8.0, 0.0],
                            gap: 6.0,
                            align_cross: Align::Center,
                            background: Some(p.header),
                            ..Style::default()
                        },
                        |ui| {
                            text(ui, "arrow", icons::EXPANDED, ICON, p.text_dim);
                            text(ui, "title", section.title, TEXT, p.text);
                        },
                    );
                    for (f, field) in section.fields.iter().enumerate() {
                        ui.push_id(f as u64);
                        let mut action = None;
                        let height = if matches!(field.widget, Widget::Asset { .. }) {
                            ASSET_ROW
                        } else {
                            26.0
                        };
                        let row = ui.row(
                            "field",
                            Style {
                                size: [Size::Grow(1.0), Size::Fixed(height)],
                                padding: [12.0, 0.0, 8.0, 0.0],
                                gap: 8.0,
                                align_cross: Align::Center,
                                ..Style::default()
                            },
                            |ui| {
                                ui.container(
                                    "name",
                                    Style {
                                        size: [Size::Fixed(96.0), Size::Fit],
                                        ..Style::default()
                                    },
                                    |ui| {
                                        text(ui, "text", field.name, TEXT, p.text_dim);
                                    },
                                );
                                match field.widget {
                                    Widget::Asset {
                                        preview,
                                        icon,
                                        name,
                                    } => {
                                        if asset_box(ui, preview, icon, name, p) {
                                            action = Some(FieldAction::Pick);
                                        }
                                    }
                                    Widget::Text(value) => {
                                        ui.row(
                                            "value",
                                            Style {
                                                size: [Size::Grow(1.0), Size::Fixed(20.0)],
                                                padding: [6.0, 0.0, 6.0, 0.0],
                                                radius: 3.0,
                                                background: Some(p.header),
                                                align_cross: Align::Center,
                                                clip: true,
                                                ..Style::default()
                                            },
                                            |ui| {
                                                text(ui, "text", value, TEXT, p.text_dim);
                                            },
                                        );
                                    }
                                    Widget::Numbers {
                                        values,
                                        labels,
                                        typing,
                                    } => {
                                        numbers(ui, values, labels, typing, p, &mut action);
                                    }
                                    Widget::Edit(value) => {
                                        let style = Style {
                                            size: [Size::Grow(1.0), Size::Fixed(20.0)],
                                            padding: [6.0, 0.0, 6.0, 0.0],
                                            radius: 3.0,
                                            background: Some(p.field),
                                            align_cross: Align::Center,
                                            ..Style::default()
                                        };
                                        let r = ui.text_field(
                                            "edit",
                                            value,
                                            &field_look(p, style, "", false),
                                        );
                                        if r.committed || r.cancelled {
                                            action = Some(FieldAction::Text {
                                                commit: r.committed,
                                            });
                                        }
                                    }
                                    Widget::Check(on) => checkbox(ui, on, p, &mut action),
                                    Widget::Choice { current, open } => {
                                        choice(ui, current, open, p, &mut action, popups);
                                    }
                                }
                                // Every row keeps the reset button's place, so fields line
                                // up and none moves when its value starts to differ.
                                let hovered = field.changed && ui.is_hovered(ui.id("reset"));
                                let id = ui.container(
                                    "reset",
                                    Style {
                                        size: [Size::Fixed(18.0), Size::Fixed(18.0)],
                                        radius: 3.0,
                                        background: hovered.then_some(p.field_hover),
                                        align_main: Align::Center,
                                        align_cross: Align::Center,
                                        ..Style::default()
                                    },
                                    |ui| {
                                        if field.changed {
                                            text(ui, "icon", icons::RESET, 13.0, p.text_dim);
                                        }
                                    },
                                );
                                if field.changed && ui.sense(id, false).clicked {
                                    action = Some(FieldAction::Reset);
                                }
                            },
                        );
                        ui.mark_region(row);
                        if ui.in_region(row) {
                            out.hovered_field = Some((s, f));
                        }
                        if let Some(action) = action {
                            out.field = Some(FieldEvent {
                                section: s,
                                field: f,
                                action,
                            });
                        }
                        ui.pop_id();
                    }
                    ui.pop_id();
                }
            });
        },
    );
}

/// The Output panel (ADR 0107 E6): a header of level toggles with their counts, the
/// filter, Follow and Clear; then the rows, the newest last, errors and warnings marked by
/// a sign and a color, a folded row's count at its end.
fn output_panel(ui: &mut Ui, view: &OutputView<'_>, p: &Palette, out: &mut ShellResponse) {
    let panel = Style {
        size: [Size::Grow(1.0), Size::Fixed(240.0)],
        direction: Direction::Column,
        background: Some(p.dock),
        ..Style::default()
    };
    ui.container("output", panel, |ui| {
        ui.row("output header", bar(p, 28.0), |ui| {
            const NAMES: [&str; 4] = ["Errors", "Warnings", "Info", "Debug"];
            for (i, name) in NAMES.iter().enumerate() {
                ui.push_id(i as u64);
                let on = view.levels[i];
                let id = ui.container(
                    "level",
                    Style {
                        padding: [8.0, 3.0, 8.0, 3.0],
                        radius: 3.0,
                        background: on.then_some(p.selection_dim),
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        let label = format!("{name} {}", view.counts[i]);
                        text(
                            ui,
                            "label",
                            &label,
                            SMALL,
                            if on { p.text } else { p.text_dim },
                        );
                    },
                );
                if ui.sense(id, false).clicked {
                    out.output_level = Some(i);
                }
                ui.pop_id();
            }
            out.output_filter = filter(ui, view.filter, "Filter the log", p);
            for (key, label, on) in [("follow", "Follow", view.follow), ("clear", "Clear", false)] {
                let id = ui.container(
                    key,
                    Style {
                        padding: [8.0, 3.0, 8.0, 3.0],
                        radius: 3.0,
                        background: on.then_some(p.selection_dim),
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        text(
                            ui,
                            "label",
                            label,
                            SMALL,
                            if on { p.text } else { p.text_dim },
                        );
                    },
                );
                if ui.sense(id, false).clicked {
                    if key == "follow" {
                        out.output_follow = true;
                    } else {
                        out.output_clear = true;
                    }
                }
            }
        });
        let mut last = None;
        ui.scroll_column("output rows", |ui| {
            for (i, row) in view.rows.iter().enumerate() {
                ui.push_id(i as u64);
                let line = Style {
                    size: [Size::Grow(1.0), Size::Fixed(20.0)],
                    padding: [8.0, 0.0, 8.0, 0.0],
                    gap: 8.0,
                    align_cross: Align::Center,
                    background: row.level.is_none().then_some(p.header),
                    ..Style::default()
                };
                let id = ui.container("row", line, |ui| {
                    let (mark, color) = match row.level {
                        Some(0) => (icons::ALERT, p.error),
                        Some(1) => (icons::ALERT, p.warning),
                        _ => ("", p.text),
                    };
                    text(ui, "time", row.time, SMALL, p.text_dim);
                    text(ui, "mark", mark, SMALL, color);
                    if !row.target.is_empty() {
                        text(ui, "target", row.target, SMALL, p.text_dim);
                    }
                    let shown = if row.level.is_none() {
                        p.text_dim
                    } else {
                        color
                    };
                    text(ui, "message", row.message, SMALL, shown);
                    if row.link {
                        text(ui, "link", icons::LINK, SMALL, p.accent);
                    }
                    if row.count > 1 {
                        ui.spacer("fill");
                        text(ui, "count", &format!("×{}", row.count), SMALL, p.text_dim);
                    }
                });
                if row.link && ui.sense(id, false).clicked {
                    out.output_row = Some(i);
                }
                last = Some(id);
                ui.pop_id();
            }
        });
        if view.follow
            && let Some(id) = last
        {
            ui.reveal(id);
        }
    });
}

/// The thin bar of bottom panel tabs, with the status at its right end.
fn bottom_bar(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    ui.row("bottom bar", bar(p, 28.0), |ui| {
        out.bottom_tab = tabs(ui, model.bottom_tabs, model.active_bottom, p);
        ui.spacer("fill");
        text(ui, "status", model.status, SMALL, p.text_dim);
    });
}
