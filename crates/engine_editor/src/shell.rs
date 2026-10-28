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
    /// Behind code (a script's tab).
    pub code: Color,
    /// The line of code the caret is on.
    pub code_line: Color,
    /// Code's colors, by [`Token`].
    pub syntax: [Color; 8],
    /// What the find bar found in the code.
    pub find: Color,
    /// The match it is on.
    pub find_current: Color,
    /// The script debugger's marks in the gutter (shapes tell them apart, ADR 0119).
    pub breakpoint: Color,
    /// A logpoint's latest message after its line.
    pub logged: Color,
    /// The band behind the line a stopped script runs next (its arrow: `warning`).
    pub stopped: Color,
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
            code: srgb(27, 28, 33),
            code_line: srgb(37, 39, 47),
            syntax: [
                srgb(222, 224, 230),
                srgb(197, 134, 192),
                srgb(206, 145, 120),
                srgb(106, 153, 85),
                srgb(181, 206, 168),
                srgb(220, 220, 170),
                srgb(86, 156, 214),
                srgb(78, 201, 176),
            ],
            find: srgb(98, 64, 28),
            find_current: srgb(156, 102, 36),
            breakpoint: srgb(229, 72, 64),
            logged: srgb(126, 176, 214),
            stopped: srgb(74, 64, 30),
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

/// What the Debugger panel's header buttons ask (ADR 0119 E9-1b-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugGo {
    /// Continue (F5).
    Continue,
    /// Step over (F10).
    Over,
    /// Step into (F11).
    Into,
    /// Step out (Shift+F11).
    Out,
}

/// The Debugger panel (ADR 0119 E9-1b-3; `docs/research/ux/script-debugger.md` §3), while
/// its tab is open: the stop's header and buttons, its call stack, the shown frame's
/// variables, every breakpoint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebuggerView<'a> {
    /// What stopped and where (`Breakpoint · on_tick · scripts/door.luau:14`), or that no
    /// script is stopped.
    pub header: &'a str,
    /// A script is stopped: Continue works.
    pub stopped: bool,
    /// And it can be stepped from there: the steps work.
    pub steppable: bool,
    /// The stack's frames, the stopped one first: its function and its place.
    pub frames: &'a [(&'a str, &'a str)],
    /// The frame whose variables are shown.
    pub frame: usize,
    /// Its variables: a section's title (`LOCALS`, no value) or a name and its value.
    pub variables: &'a [(&'a str, &'a str)],
    /// Every breakpoint.
    pub breakpoints: &'a [BreakpointRow<'a>],
}

/// A row of the Debugger panel's Breakpoints list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreakpointRow<'a> {
    /// Its mark, as the gutter draws it.
    pub look: MarkLook,
    /// Its file and line (`door.luau:14`).
    pub place: &'a str,
    /// Its line's code, where the script is open.
    pub code: &'a str,
    /// What it does in words, and its hits (`if self.hp < 0 · ×12`).
    pub detail: &'a str,
}

/// A row of the History panel (ADR 0107 E6-4): a step, or the start before them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryRow<'a> {
    /// The step's name (`Move`), or `Start`.
    pub name: &'a str,
    /// The scene it was made in, when another tab's (empty for the one shown).
    pub scene: &'a str,
    /// Done: before or at the current state; else undone, to redo.
    pub done: bool,
    /// The current state: the last step done.
    pub current: bool,
}

/// The profiler's first panel (ADR 0107 E6-5): the last frames as bars, the latest
/// frame's CPU stages and GPU passes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfilerView<'a> {
    /// The latest frame's times (`CPU 2.1 ms · GPU 6.0 ms`).
    pub header: &'a str,
    /// The last frames, oldest first: each frame's CPU and GPU times as shares of the
    /// graph's height, and whether it took longer than a 60 Hz frame.
    pub bars: &'a [(f32, f32, bool)],
    /// The CPU stages and the GPU passes, the longest first: a name and its time.
    pub stages: &'a [(&'a str, &'a str)],
    /// The GPU passes.
    pub passes: &'a [(&'a str, &'a str)],
}

/// A row of the Tasks panel (ADR 0107 E6-3): background work under way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskRow<'a> {
    /// What runs (`Compressing textures`).
    pub name: &'a str,
    /// How far it is (`12 left`); empty when nothing is counted.
    pub detail: &'a str,
    /// How long it has run (`4 s`).
    pub elapsed: &'a str,
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
    /// It has a script component: the script's mark at its end, a click on which opens
    /// the script.
    pub script: bool,
}

/// A code tab's panel (`code-editor.md`, "The code's side panel"): the open files and the
/// shown file's outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeSide<'a> {
    /// The open files, in their tabs' order (those the filter keeps).
    pub files: &'a [OpenFile<'a>],
    /// The shown file's declarations.
    pub outline: &'a [OutlineRow<'a>],
    /// What the files are filtered by.
    pub filter: &'a str,
}

/// An open file of the code panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenFile<'a> {
    /// Its kind's icon.
    pub icon: &'a str,
    /// Its file's name.
    pub name: &'a str,
    /// Its folder in the project, dimmed after the name.
    pub folder: &'a str,
    /// Shown now.
    pub active: bool,
    /// Its changes not saved.
    pub unsaved: bool,
    /// Its errors and warnings found (Luau's, Slang's), as a short label (`2`), or empty.
    pub problems: &'a str,
    /// Of them an error.
    pub error: bool,
}

/// A declaration in the outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineRow<'a> {
    /// What it is: a function, a type, a component, a heading.
    pub icon: &'a str,
    /// Its name as written.
    pub name: &'a str,
    /// How deep it is: a method one more than its struct.
    pub depth: u16,
    /// The one the caret is in.
    pub current: bool,
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
        /// A lock beside the name, shut or open: a scale whose parts keep their
        /// proportions (Unity's, Unreal's). None for no lock.
        lock: Option<bool>,
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
    /// The field's lock was clicked: shut or opened.
    Lock,
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
    /// A script component's file (`scripts/door.luau`), shown under its title: a click
    /// opens it. Empty for the others.
    pub script: &'a str,
    /// Its header's × takes the component away.
    pub removable: bool,
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
    /// A Slang module.
    Shader,
    /// A text file that is no asset (settings, data, notes).
    Text,
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

/// Code's text size (logical pixels).
pub const CODE_TEXT: f32 = 13.0;
/// A line of code's height.
pub const CODE_LINE: f32 = 19.0;
/// The room between the gutter and the code.
pub const CODE_PAD: f32 = 10.0;
/// The text's top within its line.
const CODE_TEXT_TOP: f32 = 1.0;
/// Wider and taller than any view: the current line's band, the gutter.
const CODE_FAR: f32 = 1.0e5;

/// What a run of code is, for its color ([`Palette::syntax`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Plain code: names, operators.
    Text = 0,
    /// Luau's reserved words, its contextual ones where they are keywords, attributes.
    Keyword = 1,
    /// Strings, an interpolated one's text.
    String = 2,
    /// Comments.
    Comment = 3,
    /// Numbers.
    Number = 4,
    /// A function's name where it is declared or called.
    Function = 5,
    /// `self` and the standard library's names.
    Builtin = 6,
    /// A type's name after `:`, `::`, `->` or `type`.
    Type = 7,
}

/// A run of code in view: its line and column, its text and what it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeRun<'a> {
    /// Its line in the script (from 0).
    pub line: usize,
    /// Its first column.
    pub column: usize,
    /// Its text (tabs as spaces).
    pub text: &'a str,
    /// What it is.
    pub token: Token,
}

/// The completion list under the word being typed (ADR 0107 E8-3c), while open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompletionView<'a> {
    /// Where the word starts: its line and column.
    pub line: usize,
    /// Its column.
    pub column: usize,
    /// The rows in view: an icon, what is written, its type.
    pub rows: &'a [(&'a str, &'a str, &'a str)],
    /// The picked row among them.
    pub selected: usize,
}

/// The find bar over a script's code (ADR 0107 E8-2), while it is open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FindView<'a> {
    /// What is looked for.
    pub query: &'a str,
    /// What replaces it: the replace field shows while there is one (Ctrl+H).
    pub replace: Option<&'a str>,
    /// `3 of 17`, `No results`.
    pub count: &'a str,
    /// Upper and lower case apart.
    pub case: bool,
    /// Whole words only.
    pub word: bool,
    /// The query field takes the keys (the bar opened, Ctrl+F again).
    pub focus: bool,
    /// The replace field does (Ctrl+H).
    pub focus_replace: bool,
}

/// What a breakpoint does, as its mark's shape says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// It stops the script (●).
    Stop,
    /// A logpoint (◆).
    Log,
    /// A pause point (‖).
    Pause,
}

/// How a breakpoint's mark looks in the gutter (ADR 0119; `docs/research/ux/
/// script-debugger.md` §1): by shape first, so color is never the only sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkLook {
    /// Its shape.
    pub kind: MarkKind,
    /// On: the shape filled; off, hollow.
    pub on: bool,
    /// A condition or a hit rule limits it: a short bar inside.
    pub limited: bool,
    /// Something is wrong with it (its condition fails, no code from its line on): a
    /// warning's dot beside it, and why after its line.
    pub problem: bool,
}

/// A breakpoint's options, one line under its code (ADR 0119; the UX note's §8): what
/// it does when hit, a logpoint's text, its condition and its hit rule. Tab moves between
/// the fields, Enter keeps, Escape drops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreakOptionsView<'a> {
    /// The breakpoint's line.
    pub line: usize,
    /// What it does.
    pub kind: MarkKind,
    /// A logpoint's text.
    pub text: &'a str,
    /// Its condition.
    pub condition: &'a str,
    /// Its hit rule, as typed.
    pub hits: &'a str,
    /// The field given the keys at the next frame: 0 the text, 1 the condition, 2 the
    /// hits.
    pub focus: Option<usize>,
    /// Why Enter did not keep it (a hit rule that does not read); empty otherwise.
    pub error: &'a str,
}

/// A script in the center instead of the viewport (ADR 0107 E8): the lines in view and
/// the carets among them, by line and column; the shell puts them on the monospace grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeView<'a> {
    /// The script's file.
    pub path: &'a str,
    /// Where its caret is (`Ln 4, Col 12`).
    pub place: &'a str,
    /// The runs of the lines in view, highlighted.
    pub runs: &'a [CodeRun<'a>],
    /// The first line in view (from 0).
    pub first: usize,
    /// Their numbers, as shown.
    pub numbers: &'a [&'a str],
    /// How far the view is scrolled: logical pixels across and down.
    pub scroll: [f32; 2],
    /// A character's width.
    pub advance: f32,
    /// The gutter's width, the numbers in it.
    pub gutter: f32,
    /// The selections in view: a line, its first and last columns selected (characters),
    /// and whether the selection goes on past the line's end.
    pub selections: &'a [(usize, usize, usize, bool)],
    /// The carets in view: a line and a column.
    pub carets: &'a [(usize, usize)],
    /// The carets show this frame (they blink).
    pub blink: bool,
    /// The primary caret's line: lit.
    pub current: usize,
    /// The find bar, while open.
    pub find: Option<FindView<'a>>,
    /// What it found in view: a line, the first and last columns, and whether it is the
    /// match it is on.
    pub matches: &'a [(usize, usize, usize, bool)],
    /// What Luau's checker found in view, underlined: a line, the first and last columns,
    /// and whether it is an error (else a warning).
    pub problems: &'a [(usize, usize, usize, bool)],
    /// Each such line's first message, after its text: a line, a column, the message, and
    /// whether it is an error.
    pub notes: &'a [(usize, usize, &'a str, bool)],
    /// The code bar's count (`2 errors, 1 warning`); empty when nothing was found.
    pub problem_count: &'a str,
    /// The completion list, while open.
    pub completion: Option<CompletionView<'a>>,
    /// What the mouse rests on: its line, its column, and what it is.
    pub hover: Option<(usize, usize, &'a str)>,
    /// The signature of the call the caret is in, above the caret: its line and column,
    /// the text before the argument at the caret, that argument, and the rest.
    pub signature: Option<(usize, usize, [&'a str; 3])>,
    /// The breakpoints' column, the gutter's left part (ADR 0119): its width, 0 for a
    /// file that cannot have breakpoints.
    pub marks_width: f32,
    /// The breakpoints' marks in view: a line and how its mark looks.
    pub marks: &'a [(usize, MarkLook)],
    /// A faint mark where a click in the column would put one, while the mouse is there.
    pub mark_hint: Option<(usize, MarkLook)>,
    /// The line a stopped script runs next (ADR 0119): its band, and the arrow in the
    /// breakpoints' column.
    pub stopped: Option<usize>,
    /// What the breakpoints say after their lines: a line, a column, the text, and whether
    /// it is a problem (else a logpoint's latest message).
    pub break_notes: &'a [(usize, usize, &'a str, bool)],
    /// A breakpoint's options under its line, while open.
    pub break_options: Option<BreakOptionsView<'a>>,
}

/// Everything the shell shows.
#[derive(Debug, Clone, Copy)]
pub struct ShellModel<'a> {
    /// Open scenes (tabs); an unsaved one ends with `*`.
    pub scenes: &'a [&'a str],
    /// The current scene tab.
    pub active_scene: usize,
    /// Open scripts (tabs after the scenes'); an unsaved one ends with `*`.
    pub scripts: &'a [&'a str],
    /// The script tab shown, if one is: its code instead of the viewport.
    pub active_script: Option<usize>,
    /// The shown script's code.
    pub code: Option<CodeView<'a>>,
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
    /// The inspector shows entities: its Add Component button after the sections.
    pub add_component: bool,
    /// Where the add menu opens, in logical pixels of the window: at the pointer when
    /// Ctrl+A opened it, so it is not far away. None: under the scene dock's +.
    pub add_at: Option<[f32; 2]>,
    /// The viewport's view mode as its chip reads it (`Lit`, ADR 0116).
    pub view_mode: &'a str,
    /// The modes' menu below the chip, while it is open.
    pub view_modes: Option<&'a [MenuItem<'a>]>,
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
    /// The Tasks panel there instead, while its tab is open.
    pub tasks: Option<&'a [TaskRow<'a>]>,
    /// The profiler's panel there instead, while its tab is open.
    pub profiler: Option<ProfilerView<'a>>,
    /// The script debugger's panel there instead, while its tab is open.
    pub debugger: Option<DebuggerView<'a>>,
    /// The scene dock's History tab (E6-4) instead of the tree, while it is chosen: its
    /// rows, oldest first.
    pub history: Option<&'a [HistoryRow<'a>]>,
    /// The open bottom panel.
    pub active_bottom: Option<usize>,
    /// The game is running.
    pub playing: bool,
    /// The game is paused.
    pub paused: bool,
    /// A script is stopped at a breakpoint (ADR 0119).
    pub stopped: bool,
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
    /// The scene tree's row a file dragged from the FileSystem dock, or rows dragged in the
    /// tree, would drop onto (outlined).
    pub drop_row: Option<usize>,
    /// The scene tree's row dragged rows would go next to: a line above it, or below it
    /// with `true`.
    pub drop_line: Option<(usize, bool)>,
    /// A code tab's own panel on the left (its open files and outline); with it, the scene's
    /// tree and inspector only when `docks_beside_code`.
    pub code_side: Option<CodeSide<'a>>,
    /// The scene's tree and inspector beside a code tab too (View: Scene Docks Beside
    /// Code).
    pub docks_beside_code: bool,
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
    /// A script tab was clicked.
    pub script_tab: Option<usize>,
    /// A script tab's close mark was clicked.
    pub close_script: Option<usize>,
    /// Id of the code area while a script is shown; its rectangle from [`Ui::rect_of`].
    pub code: u64,
    /// The find bar's query field was used (`Ui::edited_text` while it changes).
    pub find_query: Option<TextFieldResponse>,
    /// Its replace field was.
    pub find_replace: Option<TextFieldResponse>,
    /// Its match case toggle was clicked.
    pub find_case: bool,
    /// Its whole words toggle was.
    pub find_word: bool,
    /// One of its arrows: the match before (`true`) or after.
    pub find_step: Option<bool>,
    /// Its close mark.
    pub find_close: bool,
    /// Its Replace (`false`) or Replace All (`true`).
    pub replace: Option<bool>,
    /// The code bar's count of problems: the next one.
    pub next_problem: bool,
    /// A row of the completion list was clicked: its place among the rows in view.
    pub completion_row: Option<usize>,
    /// The breakpoint options' fields were used: the logpoint's text, the condition, the
    /// hit rule (`Ui::edited_text` while one changes).
    pub break_fields: [Option<TextFieldResponse>; 3],
    /// Its switch of what it does was clicked: a logpoint becomes a pause point, or back.
    pub break_act: bool,
    /// A tool was clicked.
    pub tool: Option<usize>,
    /// A scene tree row was clicked.
    pub tree_row: Option<usize>,
    /// A scene tree row's arrow was clicked: open or close it.
    pub tree_toggle: Option<usize>,
    /// A scene tree row's eye was clicked: turn it off or on.
    pub tree_eye: Option<usize>,
    /// A scene tree row's script mark was clicked: its script opens.
    pub tree_script: Option<usize>,
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
    /// The inspector's Add Component button was clicked.
    pub add_component: bool,
    /// A section's script file was clicked (the section's place): the script opens.
    pub section_script: Option<usize>,
    /// A section's × was clicked: its component is taken away.
    pub remove_section: Option<usize>,
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
    /// That row's node: where the pointer is in it (its top, its middle, its bottom).
    pub tree_under_id: Option<u64>,
    /// A scene tree's row held and moved past [`DRAG_START`]: rows dragged.
    pub tree_drag: Option<usize>,
    /// The code panel's open file clicked: its place among the rows shown.
    pub open_file: Option<usize>,
    /// The close mark of an open file clicked.
    pub close_file: Option<usize>,
    /// An outline row clicked.
    pub outline_row: Option<usize>,
    /// The code panel's filter used (`Ui::edited_text` while it changes).
    pub code_filter: Option<TextFieldResponse>,
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
    /// A Debugger header button was clicked.
    pub debug_go: Option<DebugGo>,
    /// A frame of the Debugger's call stack was clicked.
    pub debug_frame: Option<usize>,
    /// A breakpoint's row of the Debugger was clicked: its line is shown.
    pub debug_breakpoint: Option<usize>,
    /// A breakpoint's mark there was clicked: it is turned off or on.
    pub debug_toggle: Option<usize>,
    /// The scene dock's tab was clicked (0 the tree, 1 History).
    pub scene_dock_tab: Option<usize>,
    /// A History row was clicked: its place.
    pub history_row: Option<usize>,
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
    /// The scene dock's Attach Script button.
    pub attach_script: bool,
    /// The viewport's view mode chip was clicked: its menu opens or closes.
    pub view_mode: bool,
    /// A mode of its menu was picked.
    pub view_mode_item: Option<usize>,
    /// The mouse is over the viewport's bar of chips: a click there picks nothing in the
    /// view.
    pub on_view_bar: bool,
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
                // A code tab: its own panel on the left, the scene's docks only when asked.
                if let Some(side) = &model.code_side {
                    code_side(ui, side, p, &mut out);
                }
                center(ui, model, p, &mut out, &mut popups);
                if model.code_side.is_none() || model.docks_beside_code {
                    scene_dock(ui, model, p, &mut out, &mut popups);
                    inspector_dock(ui, model, p, &mut out, &mut popups);
                }
            },
        );
        if let Some(view) = &model.output {
            output_panel(ui, view, p, &mut out);
        }
        if let Some(rows) = model.tasks {
            tasks_panel(ui, rows, p);
        }
        if let Some(view) = &model.profiler {
            profiler_panel(ui, view, p);
        }
        if let Some(view) = &model.debugger {
            debugger_panel(ui, view, p, &mut out);
        }
        bottom_bar(ui, model, p, &mut out);
        if let (Some((entries, open)), Some(at)) = (model.add_menu, model.add_at) {
            let at = menu_place(at, entries, ui.viewport());
            let (popup, beside) = group_menu(
                ui,
                entries,
                open,
                p,
                at,
                &mut out.add_item,
                &mut out.add_group,
            );
            popups.open = Some((popup, popup));
            popups.beside = beside;
        }
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

/// Where a group menu opened at `at` goes so that it and the group opened beside it (to
/// its right) stay in a window of `size` (logical pixels): moved left near the right edge,
/// up near the bottom.
fn menu_place(at: [f32; 2], entries: &[MenuEntry<'_>], size: [f32; 2]) -> [f32; 2] {
    const ROW: f32 = 24.0;
    // The menu, and the tallest group beside its row.
    let rows = entries
        .iter()
        .enumerate()
        .map(|(i, e)| i + e.children.len().max(1))
        .max()
        .unwrap_or(0)
        .max(entries.len());
    let height = rows as f32 * ROW + 8.0;
    let x = at[0].min(size[0] - 2.0 * MENU_WIDTH - 4.0);
    let y = at[1].min(size[1] - height - 4.0);
    [x.max(4.0), y.max(4.0)]
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
            script_tabs(ui, model, p, out);
            ui.spacer("fill");
            play_controls(ui, model.playing, model.paused, p, out);
        });
        // A script's tab: its code where the tool bar and the viewport are.
        if let Some(view) = &model.code {
            ui.row("code bar", bar(p, 30.0).background(p.dock), |ui| {
                text(ui, "path", view.path, TEXT, p.text_dim);
                ui.spacer("fill");
                if !view.problem_count.is_empty() {
                    let errors = view.problems.iter().any(|&(_, _, _, e)| e)
                        || view.problem_count.contains("error");
                    let id = ui.container(
                        "problems",
                        Style {
                            padding: [8.0, 3.0, 8.0, 3.0],
                            gap: 4.0,
                            radius: 3.0,
                            align_cross: Align::Center,
                            ..Style::default()
                        },
                        |ui| {
                            let color = if errors { p.error } else { p.warning };
                            text(ui, "icon", icons::ALERT, ICON, color);
                            text(ui, "label", view.problem_count, TEXT, color);
                        },
                    );
                    out.next_problem = ui.sense(id, false).clicked;
                }
                text(ui, "place", view.place, TEXT, p.text_dim);
            });
            if let Some(find) = &view.find {
                find_bar(ui, find, p, out);
            }
            out.code = code_view(ui, view, p, out);
            file_browser(ui, model, p, out);
            return;
        }
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
        // Amber while a script is stopped (ADR 0119): the game waits in its tick.
        let edge = match (model.playing, model.stopped) {
            (true, true) => Some((p.warning, 2.0)),
            (true, false) => Some((p.playing, 2.0)),
            _ => None,
        };
        out.viewport = ui.container(
            "viewport",
            Style {
                border: edge,
                padding: [8.0; 4],
                ..Style::fill()
            },
            |ui| {
                let chip = |open: bool| Style {
                    padding: [8.0, 3.0, 8.0, 3.0],
                    gap: 4.0,
                    radius: 3.0,
                    background: Some(if open {
                        p.selection
                    } else {
                        [0.0, 0.0, 0.0, 0.45]
                    }),
                    align_cross: Align::Center,
                    ..Style::default()
                };
                let bar = ui.row(
                    "view bar",
                    Style {
                        gap: 4.0,
                        ..Style::default()
                    },
                    |ui| {
                        ui.row("view menu", chip(false), |ui| {
                            text(ui, "label", "Perspective", SMALL, p.text);
                        });
                        // The view mode (ADR 0116), its menu below it.
                        let opener = ui.row("view mode", chip(model.view_modes.is_some()), |ui| {
                            text(ui, "label", model.view_mode, SMALL, p.text);
                            text(ui, "more", icons::EXPANDED, SMALL, p.text_dim);
                            if let Some(items) = model.view_modes {
                                let items = items.iter().map(|m| (m.icon, m.label));
                                let popup =
                                    menu(ui, items, p, [0.0, 22.0], &mut out.view_mode_item);
                                popups.open = Some((popup, 0));
                            }
                        });
                        if model.view_modes.is_some()
                            && let Some((popup, _)) = popups.open
                        {
                            popups.open = Some((popup, opener));
                        }
                        if ui.sense(opener, false).clicked {
                            out.view_mode = true;
                        }
                    },
                );
                ui.mark_region(bar);
                out.on_view_bar = ui.in_region(bar);
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
        let on = model.active_scene == i && model.active_script.is_none();
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

/// The script tabs after the scenes': each its icon, its label and a close mark.
fn script_tabs(ui: &mut Ui, model: &ShellModel<'_>, p: &Palette, out: &mut ShellResponse) {
    for (i, label) in model.scripts.iter().enumerate() {
        ui.push_id(i as u64);
        let on = model.active_script == Some(i);
        let mut close = 0;
        let id = ui.container(
            "script tab",
            Style {
                padding: [8.0, 5.0, 8.0, 5.0],
                gap: 6.0,
                radius: 3.0,
                background: on.then_some(p.dock),
                align_cross: Align::Center,
                ..Style::default()
            },
            |ui| {
                text(ui, "icon", icons::SCRIPT, ICON, srgb(222, 196, 96));
                let color = if on { p.text } else { p.text_dim };
                text(ui, "label", label, TEXT, color);
                close = ui.container(
                    "close",
                    Style {
                        align_cross: Align::Center,
                        ..Style::default()
                    },
                    |ui| {
                        text(ui, "icon", icons::CLOSE, ICON, p.text_dim);
                    },
                );
            },
        );
        if ui.sense(close, false).clicked {
            out.close_script = Some(i);
        } else if ui.sense(id, false).clicked {
            out.script_tab = Some(i);
        }
        ui.pop_id();
    }
}

/// A script's code: the primary caret's line lit, the selections, the lines in view on
/// their grid in their runs' colors, the carets over them, and the gutter with the lines'
/// numbers over what is scrolled under it.
fn code_view(ui: &mut Ui, view: &CodeView<'_>, p: &Palette, out: &mut ShellResponse) -> u64 {
    let area = Style {
        background: Some(p.code),
        clip: true,
        ..Style::fill()
    };
    ui.container("code", area, |ui| {
        let y = |line: usize| line as f32 * CODE_LINE - view.scroll[1];
        let x = |column: f32| view.gutter + CODE_PAD + column * view.advance - view.scroll[0];
        let band = |at: [f32; 2], size: [f32; 2], color: Color| Style {
            size: [Size::Fixed(size[0]), Size::Fixed(size[1])],
            placed: Some(at),
            background: Some(color),
            ..Style::default()
        };
        let lit = band([0.0, y(view.current)], [CODE_FAR, CODE_LINE], p.code_line);
        ui.container("current", lit, |_| {});
        if let Some(line) = view.stopped {
            let at = band([0.0, y(line)], [CODE_FAR, CODE_LINE], p.stopped);
            ui.container("stopped", at, |_| {});
        }
        for (i, &(line, a, b, on)) in view.selections.iter().enumerate() {
            ui.push_id(i as u64);
            // A selection going on past a line's end shows a bit of its line break.
            let end = b as f32 + if on { 0.5 } else { 0.0 };
            let width = (end - a as f32) * view.advance;
            let at = [x(a as f32), y(line)];
            ui.container(
                "selection",
                band(at, [width, CODE_LINE], p.selection),
                |_| {},
            );
            ui.pop_id();
        }
        // What the find bar found, over the selections (the match it is on is one).
        for (i, &(line, a, b, on)) in view.matches.iter().enumerate() {
            ui.push_id(i as u64);
            let color = if on { p.find_current } else { p.find };
            let width = (b - a) as f32 * view.advance;
            let at = [x(a as f32), y(line)];
            ui.container("match", band(at, [width, CODE_LINE], color), |_| {});
            ui.pop_id();
        }
        for (k, run) in view.runs.iter().enumerate() {
            ui.push_id(k as u64);
            let style = Style {
                placed: Some([x(run.column as f32), y(run.line) + CODE_TEXT_TOP]),
                ..Style::default()
            };
            let color = p.syntax[run.token as usize];
            ui.mono_text_node("run", style, run.text, CODE_TEXT, color);
            ui.pop_id();
        }
        // What the checker found: underlined, each line's first message after its text.
        for (i, &(line, a, b, error)) in view.problems.iter().enumerate() {
            ui.push_id(i as u64);
            let color = if error { p.error } else { p.warning };
            let width = (b.max(a + 1) - a) as f32 * view.advance;
            let at = [x(a as f32), y(line) + CODE_LINE - 2.5];
            ui.container("problem", band(at, [width, 1.5], color), |_| {});
            ui.pop_id();
        }
        for (i, &(line, column, message, error)) in view.notes.iter().enumerate() {
            ui.push_id(i as u64);
            let style = Style {
                placed: Some([x(column as f32), y(line) + CODE_TEXT_TOP]),
                ..Style::default()
            };
            let color = if error { p.error } else { p.warning };
            ui.mono_text_node("note", style, message, CODE_TEXT, color);
            ui.pop_id();
        }
        // What the breakpoints say after their lines: a logpoint's latest message, a
        // problem.
        for (i, &(line, column, said, problem)) in view.break_notes.iter().enumerate() {
            ui.push_id(i as u64);
            let style = Style {
                placed: Some([x(column as f32), y(line) + CODE_TEXT_TOP]),
                ..Style::default()
            };
            let color = if problem { p.warning } else { p.logged };
            ui.mono_text_node("break note", style, said, CODE_TEXT, color);
            ui.pop_id();
        }
        if view.blink {
            for (i, &(line, column)) in view.carets.iter().enumerate() {
                ui.push_id(i as u64);
                let at = [x(column as f32) - 1.0, y(line)];
                ui.container("caret", band(at, [2.0, CODE_LINE], p.text), |_| {});
                ui.pop_id();
            }
        }
        ui.container(
            "gutter",
            band([0.0, 0.0], [view.gutter, CODE_FAR], p.code),
            |_| {},
        );
        // The breakpoints' column: their marks, and the faint one a click would put.
        if view.marks_width > 0.0 {
            for (i, &(line, look)) in view.marks.iter().enumerate() {
                ui.push_id(i as u64);
                gutter_mark(ui, view.marks_width, y(line), look, p.breakpoint, p);
                ui.pop_id();
            }
            if let Some((line, look)) = view.mark_hint {
                let [r, g, b, _] = p.breakpoint;
                let faint = [r, g, b, 0.45];
                ui.push_id(u64::MAX);
                gutter_mark(ui, view.marks_width, y(line), look, faint, p);
                ui.pop_id();
            }
            // Over the line's mark: the line a stopped script runs next.
            if let Some(line) = view.stopped {
                let cell = Style {
                    size: [Size::Fixed(view.marks_width), Size::Fixed(CODE_LINE)],
                    placed: Some([0.0, y(line)]),
                    align_main: Align::Center,
                    align_cross: Align::Center,
                    ..Style::default()
                };
                ui.container("stopped at", cell, |ui| {
                    text(ui, "arrow", icons::STOPPED_AT, ICON, p.warning);
                });
            }
        }
        if let Some(list) = &view.completion {
            let at = [
                x(list.column as f32) - ICON - 12.0,
                y(list.line) + CODE_LINE + 2.0,
            ];
            completion_list(ui, list, at, p, &mut out.completion_row);
        }
        if let Some((line, column, [before, now, after])) = view.signature {
            let above = if line > view.first {
                -26.0
            } else {
                CODE_LINE + 4.0
            };
            let style = Style {
                floating: Some([x(column as f32), y(line) + above]),
                padding: [8.0, 3.0, 8.0, 3.0],
                radius: 3.0,
                background: Some(p.header),
                border: Some((p.selection_dim, 1.0)),
                ..Style::default()
            };
            ui.row("signature", style, |ui| {
                ui.mono_text_node("before", Style::default(), before, CODE_TEXT, p.text_dim);
                ui.mono_text_node("now", Style::default(), now, CODE_TEXT, p.accent);
                ui.mono_text_node("after", Style::default(), after, CODE_TEXT, p.text_dim);
            });
        }
        if let Some((line, column, what)) = view.hover {
            // Above the word when its lines fit there, below it otherwise.
            let lines = what.lines().count().max(1);
            let above = if line >= view.first + lines {
                -(7.0 + lines as f32 * CODE_LINE)
            } else {
                CODE_LINE + 4.0
            };
            let style = Style {
                floating: Some([x(column as f32), y(line) + above]),
                padding: [8.0, 3.0, 8.0, 3.0],
                radius: 3.0,
                background: Some(p.header),
                border: Some((p.selection_dim, 1.0)),
                ..Style::default()
            };
            ui.container("hover", style, |ui| {
                ui.mono_text_node("text", Style::default(), what, CODE_TEXT, p.text);
            });
        }
        for (k, number) in view.numbers.iter().enumerate() {
            ui.push_id(k as u64);
            let line = view.first + k;
            let width = number.chars().count() as f32 * view.advance;
            let style = Style {
                placed: Some([
                    view.gutter - CODE_PAD * 0.5 - width,
                    y(line) + CODE_TEXT_TOP,
                ]),
                ..Style::default()
            };
            let color = if line == view.current {
                p.text
            } else {
                p.text_dim
            };
            ui.mono_text_node("number", style, number, CODE_TEXT, color);
            ui.pop_id();
        }
        if let Some(options) = &view.break_options {
            let at = [view.marks_width + 4.0, y(options.line) + CODE_LINE + 2.0];
            break_options(ui, options, at, p, out);
        }
    })
}

/// A breakpoint's mark in the gutter's column `width` wide, on the line whose top is at
/// `top`: its shape filled or hollow, a bar inside when a condition or a hit rule limits
/// it, a warning's dot beside it when something is wrong.
fn gutter_mark(ui: &mut Ui, width: f32, top: f32, look: MarkLook, color: Color, p: &Palette) {
    let glyph = mark_glyph(look);
    let cell = Style {
        size: [Size::Fixed(width), Size::Fixed(CODE_LINE)],
        placed: Some([0.0, top]),
        align_main: Align::Center,
        align_cross: Align::Center,
        ..Style::default()
    };
    ui.container("mark", cell, |ui| {
        text(ui, "glyph", glyph, ICON, color);
    });
    let center = [width * 0.5, top + CODE_LINE * 0.5];
    if look.limited {
        // Cut into a filled shape, drawn on a hollow one.
        let bar = Style {
            size: [Size::Fixed(6.0), Size::Fixed(2.0)],
            placed: Some([center[0] - 3.0, center[1] - 1.0]),
            background: Some(if look.on { p.code } else { color }),
            ..Style::default()
        };
        ui.container("limited", bar, |_| {});
    }
    if look.problem {
        let dot = Style {
            size: [Size::Fixed(5.0), Size::Fixed(5.0)],
            placed: Some([center[0] + 4.0, center[1] - 8.0]),
            radius: 2.5,
            background: Some(p.warning),
            ..Style::default()
        };
        ui.container("problem", dot, |_| {});
    }
}

/// A breakpoint's options under its line, at `at` in the code area (ADR 0119 §8): its
/// switch of what it does, a logpoint's text, its condition, its hit rule; why Enter did
/// not keep them, else how to.
fn break_options(
    ui: &mut Ui,
    view: &BreakOptionsView<'_>,
    at: [f32; 2],
    p: &Palette,
    out: &mut ShellResponse,
) {
    let style = Style {
        floating: Some(at),
        padding: [6.0, 4.0, 8.0, 4.0],
        gap: 6.0,
        radius: 3.0,
        background: Some(p.header),
        border: Some((p.selection_dim, 1.0)),
        align_cross: Align::Center,
        ..Style::default()
    };
    ui.row("break options", style, |ui| {
        let (glyph, what) = match view.kind {
            MarkKind::Stop => (icons::BREAKPOINT, "Stop"),
            MarkKind::Log => (icons::LOGPOINT, "Log"),
            MarkKind::Pause => (icons::PAUSE_POINT, "Pause"),
        };
        let switch = Style {
            padding: [6.0, 2.0, 8.0, 2.0],
            gap: 4.0,
            radius: 3.0,
            background: Some(p.field),
            align_cross: Align::Center,
            ..Style::default()
        };
        let id = ui.row("act", switch, |ui| {
            text(ui, "icon", glyph, ICON, p.breakpoint);
            text(ui, "label", what, SMALL, p.text);
        });
        out.break_act = ui.sense(id, false).clicked;
        let fields: [(&str, &str, &str, f32); 3] = [
            ("text", view.text, "speed = {self.speed}", 300.0),
            ("condition", view.condition, "Condition", 220.0),
            ("hits", view.hits, "Hits: 10, >= 10, % 64", 150.0),
        ];
        for (k, (key, now, hint, width)) in fields.into_iter().enumerate() {
            // Only a logpoint has a text.
            if k == 0 && view.kind != MarkKind::Log {
                continue;
            }
            out.break_fields[k] = option_field(ui, key, now, hint, width, view.focus == Some(k), p);
        }
        if view.error.is_empty() {
            text(ui, "how", "Enter keeps · Esc drops", SMALL, p.text_dim);
        } else {
            text(ui, "error", view.error, SMALL, p.error);
        }
    });
}

/// One of a breakpoint's options' fields, `width` wide, with its hint; `focus` gives it the
/// keys from the next frame.
fn option_field(
    ui: &mut Ui,
    key: &str,
    text_now: &str,
    hint: &str,
    width: f32,
    focus: bool,
    p: &Palette,
) -> Option<TextFieldResponse> {
    let mut response = None;
    let style = Style {
        size: [Size::Fixed(width), Size::Fixed(24.0)],
        padding: [8.0, 0.0, 4.0, 0.0],
        radius: 3.0,
        background: Some(p.field),
        align_cross: Align::Center,
        ..Style::default()
    };
    ui.row(key, style, |ui| {
        let style = Style {
            size: [Size::Grow(1.0), Size::Fixed(22.0)],
            align_cross: Align::Center,
            ..Style::default()
        };
        let r = ui.text_field("text", text_now, &field_look(p, style, hint, false));
        if focus && !r.editing {
            ui.edit_field(r.id);
        }
        response = used(r);
    });
    response
}

/// The completion list at `at` in the code area: a row each, its kind's icon, what is
/// written and its type; the picked one lit, a click on one into `out_row`.
fn completion_list(
    ui: &mut Ui,
    list: &CompletionView<'_>,
    at: [f32; 2],
    p: &Palette,
    out_row: &mut Option<usize>,
) {
    let style = Style {
        direction: Direction::Column,
        size: [Size::Fixed(460.0), Size::Fit],
        floating: Some(at),
        padding: [2.0; 4],
        radius: 3.0,
        background: Some(p.header),
        border: Some((p.selection_dim, 1.0)),
        ..Style::default()
    };
    ui.container("completion", style, |ui| {
        for (i, &(glyph, label, detail)) in list.rows.iter().enumerate() {
            ui.push_id(i as u64);
            let on = i == list.selected;
            let id = ui.row(
                "row",
                Style {
                    size: [Size::Grow(1.0), Size::Fixed(22.0)],
                    padding: [6.0, 0.0, 8.0, 0.0],
                    gap: 6.0,
                    radius: 2.0,
                    background: on.then_some(p.selection),
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", glyph, ICON, p.accent);
                    ui.mono_text_node("label", Style::default(), label, CODE_TEXT, p.text);
                    ui.spacer("fill");
                    text(ui, "detail", detail, SMALL, p.text_dim);
                },
            );
            if ui.sense(id, false).clicked {
                *out_row = Some(i);
            }
            ui.pop_id();
        }
    });
}

/// The find bar under the code bar: the query field, match case and whole words, the
/// count, the arrows and the close mark; while replacing, the replace field and its two
/// buttons under them.
fn find_bar(ui: &mut Ui, view: &FindView<'_>, p: &Palette, out: &mut ShellResponse) {
    let bar = Style {
        direction: Direction::Column,
        size: [Size::Grow(1.0), Size::Fit],
        padding: [6.0, 4.0, 6.0, 4.0],
        gap: 4.0,
        background: Some(p.dock),
        ..Style::default()
    };
    let row = Style {
        size: [Size::Grow(1.0), Size::Fit],
        gap: 2.0,
        align_cross: Align::Center,
        ..Style::default()
    };
    ui.container("find bar", bar, |ui| {
        ui.row("find", row, |ui| {
            out.find_query = find_field(ui, "query", view.query, "Find", view.focus, p);
            out.find_case = find_button(ui, "case", icons::MATCH_CASE, view.case, p);
            out.find_word = find_button(ui, "word", icons::WHOLE_WORD, view.word, p);
            let count = Style {
                size: [Size::Fixed(96.0), Size::Fit],
                padding: [8.0, 0.0, 4.0, 0.0],
                ..Style::default()
            };
            ui.container("count", count, |ui| {
                text(ui, "label", view.count, SMALL, p.text_dim);
            });
            if find_button(ui, "previous", icons::PREVIOUS, false, p) {
                out.find_step = Some(true);
            }
            if find_button(ui, "next", icons::NEXT, false, p) {
                out.find_step = Some(false);
            }
            out.find_close = find_button(ui, "close", icons::CLOSE, false, p);
        });
        if let Some(replace) = view.replace {
            ui.row("replace", row, |ui| {
                out.find_replace =
                    find_field(ui, "with", replace, "Replace", view.focus_replace, p);
                if find_button(ui, "one", icons::REPLACE, false, p) {
                    out.replace = Some(false);
                }
                if find_button(ui, "all", icons::REPLACE_ALL, false, p) {
                    out.replace = Some(true);
                }
            });
        }
    });
}

/// One of the find bar's fields, with its hint; `focus` gives it the keys from the next
/// frame, its text selected, even from the other field.
fn find_field(
    ui: &mut Ui,
    key: &str,
    text_now: &str,
    hint: &str,
    focus: bool,
    p: &Palette,
) -> Option<TextFieldResponse> {
    let mut response = None;
    let style = Style {
        size: [Size::Fixed(300.0), Size::Fixed(24.0)],
        padding: [8.0, 0.0, 4.0, 0.0],
        radius: 3.0,
        background: Some(p.field),
        align_cross: Align::Center,
        ..Style::default()
    };
    ui.row(key, style, |ui| {
        let style = Style {
            size: [Size::Grow(1.0), Size::Fixed(22.0)],
            align_cross: Align::Center,
            ..Style::default()
        };
        let r = ui.text_field("text", text_now, &field_look(p, style, hint, false));
        if focus && !r.editing {
            ui.edit_field(r.id);
        }
        response = used(r);
    });
    response
}

/// One of the find bar's buttons, lit while `on`: whether it was clicked.
fn find_button(ui: &mut Ui, key: &str, glyph: &str, on: bool, p: &Palette) -> bool {
    let id = ui.container(
        key,
        Style {
            size: [Size::Fixed(24.0), Size::Fixed(22.0)],
            radius: 3.0,
            background: on.then_some(p.selection),
            align_main: Align::Center,
            align_cross: Align::Center,
            ..Style::default()
        },
        |ui| {
            let color = if on { p.text } else { p.text_dim };
            text(ui, "icon", glyph, ICON, color);
        },
    );
    ui.sense(id, false).clicked
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
                let rows = tree_rows(ui, model.folders, p, None, None, None, false, true);
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
        FileKind::Shader => (icons::SHADER, srgb(96, 196, 222)),
        FileKind::Text => (icons::TEXT_FILE, p.text_dim),
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
    /// The row under the pointer (by the last layout, whatever the mouse holds), and its
    /// node.
    under: Option<usize>,
    under_id: Option<u64>,
    /// A row held and moved past [`DRAG_START`].
    dragged: Option<usize>,
    toggled: Option<usize>,
    eye: Option<usize>,
    script: Option<usize>,
    rename: Option<bool>,
}

/// Tree rows: a click on a row, on its arrow, on its eye (`eyes`: the scene tree's rows
/// end in one), a row dragged, and renaming in place; row `target` outlined as where a
/// drop lands, and a line above or below row `line` where dragged rows would go; with
/// `regions` each row a hover region (the row under a drag, [`RowsResponse::under`]).
#[allow(clippy::too_many_arguments)]
fn tree_rows(
    ui: &mut Ui,
    rows: &[TreeItem<'_>],
    p: &Palette,
    reveal: Option<usize>,
    target: Option<usize>,
    line: Option<(usize, bool)>,
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
                // Where dragged rows would go: a line at the row's top or bottom, from its
                // indentation on.
                if let Some((_, below)) = line.filter(|&(at, _)| at == i) {
                    let indent = 6.0 + INDENT * f32::from(item.depth);
                    ui.container(
                        "drop line",
                        Style {
                            size: [Size::Fixed(SCENE_WIDTH - indent - 6.0), Size::Fixed(2.0)],
                            placed: Some([indent, if below { 20.0 } else { 0.0 }]),
                            background: Some(p.accent),
                            ..Style::default()
                        },
                        |_| {},
                    );
                }
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
                // A script's mark (Godot's): a click opens the script.
                if item.script {
                    let mark = ui.container(
                        "script",
                        Style {
                            size: [Size::Fixed(ICON + 4.0), Size::Fit],
                            align_main: Align::Center,
                            ..Style::default()
                        },
                        |ui| {
                            text(ui, "glyph", icons::SCRIPT_MARK, ICON, p.text_dim);
                        },
                    );
                    if ui.sense(mark, false).clicked {
                        out.script = Some(i);
                    }
                }
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
        if ui.sense(id, false).clicked
            && out.toggled != Some(i)
            && out.eye != Some(i)
            && out.script != Some(i)
        {
            out.clicked = Some(i);
        }
        if ui
            .drag_delta(id)
            .is_some_and(|[x, y]| x * x + y * y > DRAG_START * DRAG_START)
        {
            out.dragged = Some(i);
        }
        if reveal == Some(i) {
            ui.reveal(id);
        }
        // Hover regions: where a dragged file or row would drop.
        if regions {
            ui.mark_region(id);
            if ui.in_region(id) {
                out.under = Some(i);
                out.under_id = Some(id);
            }
        }
        ui.pop_id();
    }
    out
}

/// The code tab's panel: the open files (filtered), then the shown file's outline.
fn code_side(ui: &mut Ui, side: &CodeSide<'_>, p: &Palette, out: &mut ShellResponse) {
    let row = |depth: u16, lit: bool| Style {
        size: [Size::Grow(1.0), Size::Fixed(22.0)],
        padding: [8.0 + INDENT * f32::from(depth), 0.0, 6.0, 0.0],
        gap: 5.0,
        align_cross: Align::Center,
        background: lit.then_some(p.selection),
        ..Style::default()
    };
    dock_column(ui, "code side", Size::Fixed(SCENE_WIDTH), p, |ui| {
        ui.row("tabs", bar(p, 30.0), |ui| {
            let _ = tabs(ui, &["Open Files"], Some(0), p);
        });
        out.code_filter = filter(ui, side.filter, "Filter files", p);
        ui.container(
            "files",
            Style {
                size: [Size::Grow(1.0), Size::Fit],
                direction: Direction::Column,
                ..Style::default()
            },
            |ui| {
                for (i, f) in side.files.iter().enumerate() {
                    ui.push_id(i as u64);
                    let mut close = false;
                    let id = ui.row("file", row(0, f.active), |ui| {
                        icon(ui, "icon", f.icon, p.text_dim);
                        text(ui, "name", f.name, TEXT, p.text);
                        if !f.folder.is_empty() {
                            text(ui, "folder", f.folder, SMALL, p.text_dim);
                        }
                        ui.spacer("fill");
                        if !f.problems.is_empty() {
                            let color = if f.error { p.error } else { p.warning };
                            text(ui, "problems", f.problems, SMALL, color);
                        }
                        // The close mark, or the unsaved dot until the row is hovered.
                        let mark = ui.container(
                            "close",
                            Style {
                                size: [Size::Fixed(ICON + 4.0), Size::Fit],
                                align_main: Align::Center,
                                ..Style::default()
                            },
                            |ui| {
                                let glyph = if f.unsaved {
                                    icons::UNSAVED
                                } else {
                                    icons::CLOSE
                                };
                                text(ui, "glyph", glyph, ICON, p.text_dim);
                            },
                        );
                        close = ui.sense(mark, false).clicked;
                    });
                    if close {
                        out.close_file = Some(i);
                    } else if ui.sense(id, false).clicked {
                        out.open_file = Some(i);
                    }
                    ui.pop_id();
                }
            },
        );
        dock_title(ui, "OUTLINE", p);
        ui.scroll_column("outline", |ui| {
            for (i, s) in side.outline.iter().enumerate() {
                ui.push_id(i as u64);
                let id = ui.row("symbol", row(s.depth, s.current), |ui| {
                    icon(ui, "icon", s.icon, p.text_dim);
                    text(ui, "name", s.name, TEXT, p.text);
                });
                if s.current {
                    ui.reveal(id);
                }
                if ui.sense(id, false).clicked {
                    out.outline_row = Some(i);
                }
                ui.pop_id();
            }
        });
    });
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
            let chosen = usize::from(model.history.is_some());
            out.scene_dock_tab = tabs(ui, &["Scene", "History"], Some(chosen), p);
            ui.spacer("fill");
            // Attach Script (Godot's place for it): the selection's script, new or made.
            let attach = ui.container(
                "attach script",
                Style {
                    size: [Size::Fixed(24.0), Size::Fixed(22.0)],
                    radius: 3.0,
                    align_main: Align::Center,
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", icons::SCRIPT_MARK, ICON, p.text);
                },
            );
            if ui.sense(attach, false).clicked {
                out.attach_script = true;
            }
            let opener = ui.container(
                "add",
                Style {
                    size: [Size::Fixed(24.0), Size::Fixed(22.0)],
                    radius: 3.0,
                    background: (model.add_menu.is_some() && model.add_at.is_none())
                        .then_some(p.selection),
                    align_main: Align::Center,
                    align_cross: Align::Center,
                    ..Style::default()
                },
                |ui| {
                    text(ui, "icon", icons::PLUS, ICON, p.text);
                    if let Some((entries, open)) = model.add_menu.filter(|_| model.add_at.is_none())
                    {
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
        if let Some(rows) = model.history {
            history_rows(ui, rows, p, out);
            return;
        }
        out.tree_filter = filter(ui, model.tree_filter, "Filter nodes", p);
        let tree = ui.scroll_column("tree", |ui| {
            let rows = tree_rows(
                ui,
                model.tree,
                p,
                model.reveal_row,
                model.drop_row,
                model.drop_line,
                true,
                true,
            );
            out.tree_row = rows.clicked;
            out.tree_under = rows.under;
            out.tree_under_id = rows.under_id;
            out.tree_drag = rows.dragged;
            out.tree_toggle = rows.toggled;
            out.tree_eye = rows.eye;
            out.tree_script = rows.script;
            out.rename = rows.rename;
        });
        ui.mark_region(tree);
        out.tree_hovered = ui.in_region(tree);
    });
}

/// The History panel's rows (E6-4): every step by name, oldest first; those undone dimmed
/// below the current one; another tab's steps with their scene's name. A click goes to
/// that state.
fn history_rows(ui: &mut Ui, rows: &[HistoryRow<'_>], p: &Palette, out: &mut ShellResponse) {
    let mut current = None;
    ui.scroll_column("history", |ui| {
        for (i, row) in rows.iter().enumerate() {
            ui.push_id(i as u64);
            let line = Style {
                size: [Size::Grow(1.0), Size::Fixed(22.0)],
                padding: [10.0, 0.0, 8.0, 0.0],
                gap: 8.0,
                align_cross: Align::Center,
                background: row.current.then_some(p.selection),
                ..Style::default()
            };
            let id = ui.container("step", line, |ui| {
                let color = if row.done { p.text } else { p.text_dim };
                text(ui, "name", row.name, TEXT, color);
                if !row.scene.is_empty() {
                    text(ui, "scene", row.scene, SMALL, p.text_dim);
                }
            });
            if row.current {
                current = Some(id);
            }
            if ui.sense(id, false).clicked {
                out.history_row = Some(i);
            }
            ui.pop_id();
        }
    });
    if let Some(id) = current {
        ui.reveal(id);
    }
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
                            if section.removable {
                                ui.spacer("fill");
                                let hovered = ui.is_hovered(ui.id("remove"));
                                let id = ui.container(
                                    "remove",
                                    Style {
                                        size: [Size::Fixed(18.0), Size::Fixed(18.0)],
                                        radius: 3.0,
                                        background: hovered.then_some(p.field_hover),
                                        align_main: Align::Center,
                                        align_cross: Align::Center,
                                        ..Style::default()
                                    },
                                    |ui| {
                                        text(ui, "icon", icons::CLOSE, 13.0, p.text_dim);
                                    },
                                );
                                if ui.sense(id, false).clicked {
                                    out.remove_section = Some(s);
                                }
                            }
                        },
                    );
                    // A script's component: its file, as Godot's Script property.
                    if !section.script.is_empty() {
                        let id = ui.row(
                            "script",
                            Style {
                                size: [Size::Grow(1.0), Size::Fixed(26.0)],
                                padding: [12.0, 0.0, 8.0, 0.0],
                                gap: 6.0,
                                align_cross: Align::Center,
                                ..Style::default()
                            },
                            |ui| {
                                text(ui, "icon", icons::SCRIPT, ICON, p.text_dim);
                                text(ui, "path", section.script, TEXT, p.accent);
                            },
                        );
                        if ui.sense(id, false).clicked {
                            out.section_script = Some(s);
                        }
                    }
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
                                ui.row(
                                    "name",
                                    Style {
                                        size: [Size::Fixed(96.0), Size::Fit],
                                        align_cross: Align::Center,
                                        ..Style::default()
                                    },
                                    |ui| {
                                        text(ui, "text", field.name, TEXT, p.text_dim);
                                        if let Widget::Numbers {
                                            lock: Some(shut), ..
                                        } = field.widget
                                        {
                                            ui.spacer("fill");
                                            let glyph =
                                                if shut { icons::LOCK } else { icons::UNLOCK };
                                            let color = if shut { p.text } else { p.text_dim };
                                            let hovered = ui.is_hovered(ui.id("lock"));
                                            let id = ui.container(
                                                "lock",
                                                Style {
                                                    size: [Size::Fixed(18.0), Size::Fixed(18.0)],
                                                    radius: 3.0,
                                                    background: hovered.then_some(p.field_hover),
                                                    align_main: Align::Center,
                                                    align_cross: Align::Center,
                                                    ..Style::default()
                                                },
                                                |ui| {
                                                    text(ui, "icon", glyph, 13.0, color);
                                                },
                                            );
                                            if ui.sense(id, false).clicked {
                                                action = Some(FieldAction::Lock);
                                            }
                                        }
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
                                        ..
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
                // Add Component, after the components (Unity's place).
                if model.add_component {
                    ui.row(
                        "add component row",
                        Style {
                            size: [Size::Grow(1.0), Size::Fixed(48.0)],
                            align_main: Align::Center,
                            align_cross: Align::Center,
                            ..Style::default()
                        },
                        |ui| {
                            let id = ui.row(
                                "add component",
                                Style {
                                    padding: [14.0, 4.0, 14.0, 4.0],
                                    gap: 6.0,
                                    radius: 3.0,
                                    background: Some(p.header),
                                    align_cross: Align::Center,
                                    ..Style::default()
                                },
                                |ui| {
                                    text(ui, "icon", icons::ADD_COMPONENT, ICON, p.text);
                                    text(ui, "label", "Add Component", TEXT, p.text);
                                },
                            );
                            if ui.sense(id, false).clicked {
                                out.add_component = true;
                            }
                        },
                    );
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

/// The profiler's panel (ADR 0107 E6-5): the latest frame's times, a bar per frame kept
/// (CPU, and the GPU behind it), then the longest CPU stages and GPU passes side by side.
fn profiler_panel(ui: &mut Ui, view: &ProfilerView<'_>, p: &Palette) {
    const GRAPH: f32 = 56.0;
    let panel = Style {
        size: [Size::Grow(1.0), Size::Fixed(240.0)],
        direction: Direction::Column,
        padding: [8.0, 4.0, 8.0, 4.0],
        gap: 4.0,
        background: Some(p.dock),
        ..Style::default()
    };
    ui.container("profiler", panel, |ui| {
        text(ui, "header", view.header, SMALL, p.text);
        ui.row(
            "graph",
            Style {
                size: [Size::Grow(1.0), Size::Fixed(GRAPH)],
                gap: 1.0,
                align_cross: Align::End,
                background: Some(p.header),
                ..Style::default()
            },
            |ui| {
                for (i, &(cpu, gpu, over)) in view.bars.iter().enumerate() {
                    ui.push_id(i as u64);
                    let height = cpu.max(gpu).clamp(0.02, 1.0) * GRAPH;
                    ui.container(
                        "bar",
                        Style {
                            size: [Size::Fixed(4.0), Size::Fixed(height)],
                            background: Some(if over { p.warning } else { p.accent }),
                            ..Style::default()
                        },
                        |_| {},
                    );
                    ui.pop_id();
                }
            },
        );
        ui.row(
            "tables",
            Style {
                size: [Size::Grow(1.0), Size::Grow(1.0)],
                gap: 24.0,
                ..Style::default()
            },
            |ui| {
                for (key, title, rows) in [("cpu", "CPU", view.stages), ("gpu", "GPU", view.passes)]
                {
                    ui.container(
                        key,
                        Style {
                            size: [Size::Fixed(300.0), Size::Grow(1.0)],
                            direction: Direction::Column,
                            ..Style::default()
                        },
                        |ui| {
                            text(ui, "title", title, SMALL, p.text_dim);
                            for (i, (name, time)) in rows.iter().enumerate() {
                                ui.push_id(i as u64);
                                ui.row(
                                    "row",
                                    Style {
                                        size: [Size::Grow(1.0), Size::Fixed(15.0)],
                                        align_cross: Align::Center,
                                        ..Style::default()
                                    },
                                    |ui| {
                                        text(ui, "name", name, SMALL, p.text);
                                        ui.spacer("fill");
                                        text(ui, "time", time, SMALL, p.text_dim);
                                    },
                                );
                                ui.pop_id();
                            }
                        },
                    );
                }
            },
        );
    });
}

/// The Debugger panel (ADR 0119 E9-1b-3): the stop's header with Continue and the steps,
/// then its call stack, the shown frame's variables and every breakpoint side by side.
fn debugger_panel(ui: &mut Ui, view: &DebuggerView<'_>, p: &Palette, out: &mut ShellResponse) {
    let panel = Style {
        size: [Size::Grow(1.0), Size::Fixed(240.0)],
        direction: Direction::Column,
        background: Some(p.dock),
        ..Style::default()
    };
    ui.container("debugger", panel, |ui| {
        ui.row("debug header", bar(p, 28.0), |ui| {
            let buttons = [
                ("continue", "Continue F5", DebugGo::Continue, view.stopped),
                ("over", "Over F10", DebugGo::Over, view.steppable),
                ("into", "Into F11", DebugGo::Into, view.steppable),
                ("out", "Out Shift+F11", DebugGo::Out, view.steppable),
            ];
            for (key, label, go, on) in buttons {
                let id = ui.container(
                    key,
                    Style {
                        padding: [8.0, 3.0, 8.0, 3.0],
                        radius: 3.0,
                        background: on.then_some(p.field),
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
                if on && ui.sense(id, false).clicked {
                    out.debug_go = Some(go);
                }
            }
            ui.container(
                "gap",
                Style {
                    size: [Size::Fixed(12.0), Size::Fixed(1.0)],
                    ..Style::default()
                },
                |_| {},
            );
            let color = if view.stopped { p.warning } else { p.text_dim };
            text(ui, "header", view.header, SMALL, color);
        });
        let columns = Style {
            size: [Size::Grow(1.0), Size::Grow(1.0)],
            gap: 1.0,
            background: Some(p.window),
            ..Style::default()
        };
        ui.row("debug columns", columns, |ui| {
            let column = |width: Size| Style {
                size: [width, Size::Grow(1.0)],
                direction: Direction::Column,
                padding: [8.0, 4.0, 8.0, 4.0],
                background: Some(p.dock),
                ..Style::default()
            };
            let row = Style {
                size: [Size::Grow(1.0), Size::Fixed(18.0)],
                gap: 8.0,
                align_cross: Align::Center,
                ..Style::default()
            };
            ui.container("stack", column(Size::Fixed(280.0)), |ui| {
                text(ui, "title", "CALL STACK", SMALL, p.text_dim);
                ui.scroll_column("frames", |ui| {
                    for (i, &(function, place)) in view.frames.iter().enumerate() {
                        ui.push_id(i as u64);
                        let shown = Style {
                            background: (i == view.frame).then_some(p.selection_dim),
                            ..row
                        };
                        let id = ui.container("frame", shown, |ui| {
                            let mark = if i == 0 { icons::STOPPED_AT } else { "" };
                            icon(ui, "mark", mark, p.warning);
                            text(ui, "function", function, SMALL, p.text);
                            ui.spacer("fill");
                            text(ui, "place", place, SMALL, p.text_dim);
                        });
                        if ui.sense(id, false).clicked {
                            out.debug_frame = Some(i);
                        }
                        ui.pop_id();
                    }
                });
            });
            ui.container("variables", column(Size::Grow(1.0)), |ui| {
                text(ui, "title", "VARIABLES", SMALL, p.text_dim);
                ui.scroll_column("values", |ui| {
                    for (i, &(name, value)) in view.variables.iter().enumerate() {
                        ui.push_id(i as u64);
                        ui.container("variable", row, |ui| {
                            if value.is_empty() {
                                text(ui, "section", name, SMALL, p.text_dim);
                            } else {
                                text(ui, "name", name, SMALL, p.text);
                                text(ui, "value", value, SMALL, p.logged);
                            }
                        });
                        ui.pop_id();
                    }
                });
            });
            ui.container("breakpoints", column(Size::Fixed(480.0)), |ui| {
                text(ui, "title", "BREAKPOINTS", SMALL, p.text_dim);
                ui.scroll_column("marks", |ui| {
                    if view.breakpoints.is_empty() {
                        text(
                            ui,
                            "none",
                            "None: F9 sets one in a script's tab",
                            SMALL,
                            p.text_dim,
                        );
                    }
                    for (i, b) in view.breakpoints.iter().enumerate() {
                        ui.push_id(i as u64);
                        let id = ui.container("breakpoint", row, |ui| {
                            let mark = ui.container(
                                "mark",
                                Style {
                                    size: [Size::Fixed(ICON + 6.0), Size::Fixed(18.0)],
                                    align_main: Align::Center,
                                    align_cross: Align::Center,
                                    ..Style::default()
                                },
                                |ui| {
                                    text(ui, "glyph", mark_glyph(b.look), ICON, p.breakpoint);
                                },
                            );
                            if ui.sense(mark, false).clicked {
                                out.debug_toggle = Some(i);
                            }
                            text(ui, "place", b.place, SMALL, p.text);
                            text(ui, "code", b.code, SMALL, p.text_dim);
                            ui.spacer("fill");
                            text(ui, "detail", b.detail, SMALL, p.text_dim);
                        });
                        if out.debug_toggle != Some(i) && ui.sense(id, false).clicked {
                            out.debug_breakpoint = Some(i);
                        }
                        ui.pop_id();
                    }
                });
            });
        });
    });
}

/// The glyph of a breakpoint's mark: its shape for what it does, hollow when off.
fn mark_glyph(look: MarkLook) -> &'static str {
    match (look.kind, look.on) {
        (MarkKind::Stop, true) => icons::BREAKPOINT,
        (MarkKind::Stop, false) => icons::BREAKPOINT_OFF,
        (MarkKind::Log, true) => icons::LOGPOINT,
        (MarkKind::Log, false) => icons::LOGPOINT_OFF,
        (MarkKind::Pause, true) => icons::PAUSE_POINT,
        (MarkKind::Pause, false) => icons::PAUSE_POINT_OFF,
    }
}

/// The Tasks panel (ADR 0107 E6-3): what runs in the background, how far it is and for how
/// long; nothing blocks the editor meanwhile.
fn tasks_panel(ui: &mut Ui, rows: &[TaskRow<'_>], p: &Palette) {
    let panel = Style {
        size: [Size::Grow(1.0), Size::Fixed(160.0)],
        direction: Direction::Column,
        padding: [0.0, 4.0, 0.0, 4.0],
        background: Some(p.dock),
        ..Style::default()
    };
    ui.container("tasks", panel, |ui| {
        if rows.is_empty() {
            ui.row("idle", bar(p, 24.0).background(p.dock), |ui| {
                text(
                    ui,
                    "label",
                    "Nothing runs in the background",
                    SMALL,
                    p.text_dim,
                );
            });
        }
        for (i, row) in rows.iter().enumerate() {
            ui.push_id(i as u64);
            let line = Style {
                size: [Size::Grow(1.0), Size::Fixed(22.0)],
                padding: [8.0, 0.0, 8.0, 0.0],
                gap: 12.0,
                align_cross: Align::Center,
                ..Style::default()
            };
            ui.container("task", line, |ui| {
                text(ui, "name", row.name, SMALL, p.text);
                text(ui, "detail", row.detail, SMALL, p.text_dim);
                ui.spacer("fill");
                text(ui, "elapsed", row.elapsed, SMALL, p.text_dim);
            });
            ui.pop_id();
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
