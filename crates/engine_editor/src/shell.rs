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
use engine_ui::{Align, Color, Direction, Size, Style, Ui};

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
    /// Selected rows and the active tool.
    pub selection: Color,
    /// Main text.
    pub text: Color,
    /// Secondary text: labels, inactive tabs, hints.
    pub text_dim: Color,
    /// Accents: the active tab's edge, focus.
    pub accent: Color,
    /// The edge around the viewport while the game plays.
    pub playing: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            window: srgb(18, 18, 22),
            dock: srgb(33, 34, 40),
            header: srgb(26, 27, 32),
            field: srgb(45, 47, 55),
            selection: srgb(52, 78, 120),
            text: srgb(222, 224, 230),
            text_dim: srgb(140, 144, 156),
            accent: srgb(96, 150, 240),
            playing: srgb(90, 200, 120),
        }
    }
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
}

/// One property in the inspector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field<'a> {
    /// Name.
    pub name: &'a str,
    /// Value as text.
    pub value: &'a str,
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
    /// Inspector sections.
    pub inspector: &'a [Section<'a>],
    /// The current folder's path.
    pub path: &'a str,
    /// Folder tree rows.
    pub folders: &'a [TreeItem<'a>],
    /// Files in the current folder.
    pub files: &'a [FileItem<'a>],
    /// Bottom panel tabs, and the open one (`None`: all closed).
    pub bottom_tabs: &'a [&'a str],
    /// The open bottom panel.
    pub active_bottom: Option<usize>,
    /// The game is running.
    pub playing: bool,
    /// Right end of the bottom bar.
    pub status: &'a str,
}

/// What happened in the shell this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellResponse {
    /// Id of the viewport area; its rectangle after layout comes from [`Ui::rect_of`].
    pub viewport: u64,
    /// A scene tab was clicked.
    pub scene_tab: Option<usize>,
    /// A tool was clicked.
    pub tool: Option<usize>,
    /// A scene tree row was clicked.
    pub tree_row: Option<usize>,
    /// A folder was clicked.
    pub folder: Option<usize>,
    /// A file was clicked.
    pub file: Option<usize>,
    /// A bottom panel tab was clicked.
    pub bottom_tab: Option<usize>,
    /// Play (F5).
    pub play: bool,
    /// Pause (F7).
    pub pause: bool,
    /// Stop (F8).
    pub stop: bool,
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

fn text(ui: &mut Ui, key: &str, s: &str, size: f32, color: Color) -> u64 {
    ui.text_node(key, Style::default(), s, size, color, false)
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
    ui.container("shell", root, |ui| {
        ui.row(
            "main",
            Style {
                gap: 1.0,
                ..Style::fill()
            },
            |ui| {
                center(ui, model, p, &mut out);
                scene_dock(ui, model, p, &mut out);
                inspector_dock(ui, model, p);
            },
        );
        bottom_bar(ui, model, p, &mut out);
    });
    out
}

/// Scene tabs with the play controls, tool bar, viewport and file browser.
fn center(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    let column = Style {
        direction: Direction::Column,
        gap: 1.0,
        ..Style::fill()
    };
    ui.container("center", column, |ui| {
        ui.row("scene tabs", bar(p, 30.0), |ui| {
            out.scene_tab = tabs(ui, model.scenes, Some(model.active_scene), p);
            ui.spacer("fill");
            play_controls(ui, model.playing, p, out);
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

/// Small monochrome play, pause and stop at the end of the tab row (not a colored group in
/// the middle of the window).
fn play_controls(ui: &mut Ui, playing: bool, p: &Palette, out: &mut ShellResponse) {
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
    let id = ui.container("pause", button, |ui| {
        text(ui, "icon", icons::PAUSE, ICON, p.text_dim);
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
                out.folder = tree_rows(ui, model.folders, p);
            });
        });
        dock_column(ui, "listing", Size::Grow(1.0), p, |ui| {
            dock_title(ui, model.path, p);
            ui.scroll_column("list", |ui| {
                for (i, file) in model.files.iter().enumerate() {
                    ui.push_id(i as u64);
                    let id = ui.row(
                        "file",
                        Style {
                            size: [Size::Grow(1.0), Size::Fit],
                            padding: [8.0, 3.0, 8.0, 3.0],
                            gap: 6.0,
                            align_cross: Align::Center,
                            ..Style::default()
                        },
                        |ui| {
                            let (glyph, color) = file_icon(file.kind, p);
                            text(ui, "icon", glyph, ICON, color);
                            text(ui, "name", file.name, TEXT, p.text);
                        },
                    );
                    if ui.sense(id, false).clicked {
                        out.file = Some(i);
                    }
                    ui.pop_id();
                }
            });
        });
    });
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

/// A filter box with a hint (text entry comes with the text field widget).
fn filter(ui: &mut Ui, hint: &str, p: &Palette) {
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
                    padding: [8.0, 0.0, 8.0, 0.0],
                    gap: 6.0,
                    radius: 3.0,
                    background: Some(p.field),
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", icons::SEARCH, ICON, p.text_dim);
                    text(ui, "hint", hint, TEXT, p.text_dim);
                },
            );
        },
    );
}

/// Tree rows; returns the clicked one.
fn tree_rows(ui: &mut Ui, rows: &[TreeItem<'_>], p: &Palette) -> Option<usize> {
    let mut clicked = None;
    for (i, item) in rows.iter().enumerate() {
        ui.push_id(i as u64);
        let id = ui.row(
            "row",
            Style {
                size: [Size::Grow(1.0), Size::Fixed(22.0)],
                padding: [6.0 + INDENT * f32::from(item.depth), 0.0, 6.0, 0.0],
                gap: 5.0,
                align_cross: Align::Center,
                background: item.selected.then_some(p.selection),
                ..Style::default()
            },
            |ui| {
                let arrow = match item.expanded {
                    Some(true) => icons::EXPANDED,
                    Some(false) => icons::COLLAPSED,
                    None => "",
                };
                ui.container(
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
                if !item.icon.is_empty() {
                    text(ui, "icon", item.icon, ICON, p.text_dim);
                }
                text(ui, "name", item.name, TEXT, p.text);
                if !item.kind.is_empty() {
                    text(ui, "kind", item.kind, SMALL, p.text_dim);
                }
            },
        );
        if ui.sense(id, false).clicked {
            clicked = Some(i);
        }
        ui.pop_id();
    }
    clicked
}

/// Scene tree dock: tabs, filter, tree.
fn scene_dock(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    dock_column(ui, "scene dock", Size::Fixed(SCENE_WIDTH), p, |ui| {
        ui.row("tabs", bar(p, 30.0), |ui| {
            tabs(ui, &["Scene", "History"], Some(0), p);
        });
        filter(ui, "Filter nodes", p);
        ui.scroll_column("tree", |ui| {
            out.tree_row = tree_rows(ui, model.tree, p);
        });
    });
}

/// Inspector dock: tabs, the selection, filter, sections of fields.
fn inspector_dock(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette) {
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
                },
            );
            filter(ui, "Filter properties", p);
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
                        ui.row(
                            "field",
                            Style {
                                size: [Size::Grow(1.0), Size::Fixed(26.0)],
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
                                ui.row(
                                    "value",
                                    Style {
                                        size: [Size::Grow(1.0), Size::Fixed(20.0)],
                                        padding: [6.0, 0.0, 6.0, 0.0],
                                        radius: 3.0,
                                        background: Some(p.field),
                                        align_cross: Align::Center,
                                        ..Style::default()
                                    },
                                    |ui| {
                                        text(ui, "text", field.value, TEXT, p.text);
                                    },
                                );
                            },
                        );
                        ui.pop_id();
                    }
                    ui.pop_id();
                }
            });
        },
    );
}

/// The thin bar of bottom panel tabs, with the status at its right end.
fn bottom_bar(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    ui.row("bottom bar", bar(p, 28.0), |ui| {
        out.bottom_tab = tabs(ui, model.bottom_tabs, model.active_bottom, p);
        ui.spacer("fill");
        text(ui, "status", model.status, SMALL, p.text_dim);
    });
}
