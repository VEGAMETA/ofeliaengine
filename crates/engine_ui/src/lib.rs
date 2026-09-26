//! UI core for the editor and games (ADR 0030, research `docs/research/ui-framework.md`).
//!
//! - **Immediate-mode building:** UI code runs whenever a frame is built and reads the
//!   model (ECS, reflection, settings) directly. There is no widget tree to keep in sync,
//!   so undo, scripts and multi-editing never leave stale UI behind.
//! - **Retained state:** per-widget state (scroll offsets, focus, the widget under the
//!   mouse) lives in [`Ui`] between frames, keyed by hashed ids.
//! - **Full-tree layout:** a frame's nodes are laid out after the whole tree is built, so
//!   sizes flow both ways (fit to children, grow into parents). See [`Size`].
//! - **Output** is a [`DrawCmd`] list (rectangles, text, clip changes) that a renderer turns
//!   into `engine_render2d` quads and glyphs. Hit testing takes this frame's mouse
//!   position against the previous frame's layout (rectangles are one frame old).
//! - Steady-state frames do not allocate: nodes, text and commands live in buffers that
//!   are cleared, not freed.
//!
//! ```
//! use engine_ui::*;
//!
//! let mut ui = Ui::new(Theme::default());
//! let mut measure = MonospaceMeasure::default();
//! let input = UiInput::default();
//! ui.begin_frame(&input, [800.0, 600.0]);
//! ui.column("root", Style::fill().padding(8.0).gap(4.0), |ui| {
//!     ui.label("Inspector");
//!     if ui.button("Add component").clicked {
//!         // change the model here; the next frame shows it
//!     }
//! });
//! let frame = ui.end_frame(&mut measure);
//! assert!(frame.commands.len() >= 3);
//! ```

mod layout;
mod widgets;

pub use layout::{Align, Direction};
pub use widgets::Response;

use core::hash::{BuildHasherDefault, Hasher};
use core::ops::Range;
use std::collections::HashMap;

pub(crate) const NONE: u32 = u32::MAX;

/// Linear RGBA color.
pub type Color = [f32; 4];

/// An axis-aligned rectangle in pixels (y down).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Top-left corner.
    pub pos: [f32; 2],
    /// Width and height.
    pub size: [f32; 2],
}

impl Rect {
    /// A rectangle from its corner and size.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            pos: [x, y],
            size: [w, h],
        }
    }

    /// Whether the point is inside.
    pub fn contains(&self, p: [f32; 2]) -> bool {
        p[0] >= self.pos[0]
            && p[1] >= self.pos[1]
            && p[0] < self.pos[0] + self.size[0]
            && p[1] < self.pos[1] + self.size[1]
    }

    /// The overlap of two rectangles (empty if they do not overlap).
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.pos[0].max(o.pos[0]);
        let y0 = self.pos[1].max(o.pos[1]);
        let x1 = (self.pos[0] + self.size[0]).min(o.pos[0] + o.size[0]);
        let y1 = (self.pos[1] + self.size[1]).min(o.pos[1] + o.size[1]);
        Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }
}

/// How a node is sized along one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// Exactly this many pixels.
    Fixed(f32),
    /// As large as its content (children, text) plus padding.
    Fit,
    /// A share of the space the parent has left, by weight (along the parent's main
    /// axis), or the parent's full inner size (across it).
    Grow(f32),
    /// A fraction of the parent's inner size.
    Percent(f32),
}

/// Layout and look of a node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// Width and height.
    pub size: [Size; 2],
    /// Lower bounds (pixels).
    pub min: [f32; 2],
    /// Upper bounds (pixels).
    pub max: [f32; 2],
    /// Children flow along this axis.
    pub direction: Direction,
    /// Left, top, right, bottom.
    pub padding: [f32; 4],
    /// Space between children.
    pub gap: f32,
    /// Children's alignment along the main axis.
    pub align_main: Align,
    /// Children's alignment across the main axis.
    pub align_cross: Align,
    /// Fill color.
    pub background: Option<Color>,
    /// Outline color and width.
    pub border: Option<(Color, f32)>,
    /// Corner radius.
    pub radius: f32,
    /// Clip children to this node.
    pub clip: bool,
    /// Scroll children along the main axis with the mouse wheel (implies `clip`).
    pub scroll: bool,
    /// Taken out of the flow and placed at this offset from the parent's corner, drawn
    /// above everything else (popups, tooltips, menus).
    pub floating: Option<[f32; 2]>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            size: [Size::Fit, Size::Fit],
            min: [0.0; 2],
            max: [f32::INFINITY; 2],
            direction: Direction::Row,
            padding: [0.0; 4],
            gap: 0.0,
            align_main: Align::Start,
            align_cross: Align::Start,
            background: None,
            border: None,
            radius: 0.0,
            clip: false,
            scroll: false,
            floating: None,
        }
    }
}

impl Style {
    /// Grows to fill the parent on both axes.
    pub fn fill() -> Self {
        Self {
            size: [Size::Grow(1.0), Size::Grow(1.0)],
            ..Self::default()
        }
    }

    /// Sets both sizes.
    pub fn size(mut self, w: Size, h: Size) -> Self {
        self.size = [w, h];
        self
    }

    /// Sets the same padding on all sides.
    pub fn padding(mut self, p: f32) -> Self {
        self.padding = [p; 4];
        self
    }

    /// Sets the gap between children.
    pub fn gap(mut self, g: f32) -> Self {
        self.gap = g;
        self
    }

    /// Sets the fill color.
    pub fn background(mut self, c: Color) -> Self {
        self.background = Some(c);
        self
    }

    /// Sets the alignment of children.
    pub fn align(mut self, main: Align, cross: Align) -> Self {
        self.align_main = main;
        self.align_cross = cross;
        self
    }
}

/// Text inside a node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextSpan {
    pub(crate) range: Range<usize>,
    pub(crate) size: f32,
    pub(crate) color: Color,
    pub(crate) wrap: bool,
}

/// One element of a frame's tree.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) id: u64,
    pub(crate) parent: u32,
    pub(crate) first_child: u32,
    pub(crate) last_child: u32,
    pub(crate) next_sibling: u32,
    /// One past the last node of this subtree (pre-order).
    pub(crate) end: u32,
    pub(crate) style: Style,
    pub(crate) text: Option<TextSpan>,
    pub(crate) interactive: bool,
    pub(crate) focusable: bool,
    /// Size of the text (or other content) itself.
    pub(crate) content: [f32; 2],
    /// Content size plus padding.
    pub(crate) fit: [f32; 2],
    pub(crate) rect: Rect,
}

/// Measures text for layout. The editor implements it with `engine_text`; tests use
/// [`MonospaceMeasure`].
pub trait TextMeasure {
    /// Size of `text` at `size` pixels, wrapped at `max_width` if given.
    fn measure(&mut self, text: &str, size: f32, max_width: Option<f32>) -> [f32; 2];
}

/// Every character is `0.5 × size` wide and lines are `1.25 × size` tall: for tests
/// and tools without fonts.
#[derive(Debug, Default, Clone, Copy)]
pub struct MonospaceMeasure;

impl TextMeasure for MonospaceMeasure {
    fn measure(&mut self, text: &str, size: f32, max_width: Option<f32>) -> [f32; 2] {
        let advance = size * 0.5;
        let chars = text.chars().count() as f32;
        let width = chars * advance;
        match max_width {
            Some(w) if w > 0.0 && width > w => {
                let per_line = (w / advance).floor().max(1.0);
                let lines = (chars / per_line).ceil();
                [per_line * advance, lines * size * 1.25]
            }
            _ => [width, size * 1.25],
        }
    }
}

/// Colors and metrics shared by all widgets (design tokens, CLAUDE.md §12).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Body text size in pixels.
    pub text_size: f32,
    /// Text color.
    pub text: Color,
    /// Panel background.
    pub panel: Color,
    /// Button fill.
    pub button: Color,
    /// Button fill under the mouse.
    pub button_hover: Color,
    /// Button fill while pressed.
    pub button_active: Color,
    /// Keyboard focus outline.
    pub focus: Color,
    /// Padding inside buttons.
    pub button_padding: [f32; 2],
    /// Corner radius of buttons.
    pub radius: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text_size: 14.0,
            text: [0.9, 0.9, 0.92, 1.0],
            panel: [0.12, 0.12, 0.14, 1.0],
            button: [0.22, 0.22, 0.26, 1.0],
            button_hover: [0.28, 0.28, 0.33, 1.0],
            button_active: [0.18, 0.35, 0.6, 1.0],
            focus: [0.35, 0.6, 1.0, 1.0],
            button_padding: [10.0, 5.0],
            radius: 4.0,
        }
    }
}

/// Keys the UI core reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKey {
    /// Move focus forward (backward with shift).
    Tab,
    /// Activate the focused widget.
    Enter,
    /// Activate the focused widget.
    Space,
    /// Drop focus.
    Escape,
}

/// Input for one frame, gathered from `engine_platform` events.
#[derive(Debug, Clone, Default)]
pub struct UiInput {
    /// Mouse position in pixels; `None` outside the window.
    pub mouse: Option<[f32; 2]>,
    /// Primary button held.
    pub mouse_down: bool,
    /// Primary button went down this frame.
    pub mouse_pressed: bool,
    /// Primary button went up this frame.
    pub mouse_released: bool,
    /// Wheel movement in pixels (positive scrolls content up).
    pub wheel: f32,
    /// Keys pressed this frame, in order.
    pub keys: Vec<UiKey>,
    /// Shift held.
    pub shift: bool,
}

/// A drawing command, in paint order.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawCmd {
    /// A filled and/or outlined rounded rectangle.
    Rect {
        /// Where.
        rect: Rect,
        /// Fill, if any.
        fill: Option<Color>,
        /// Outline color and width, if any.
        border: Option<(Color, f32)>,
        /// Corner radius.
        radius: f32,
    },
    /// Text laid out in `rect` (wrapped to its width if `wrap`).
    Text {
        /// Where.
        rect: Rect,
        /// Byte range in [`Frame::text`].
        range: Range<usize>,
        /// Pixel size.
        size: f32,
        /// Color.
        color: Color,
        /// Wrap to the rectangle's width.
        wrap: bool,
    },
    /// Clip following commands to this rectangle (`None`: no clip).
    Clip(Option<Rect>),
}

/// The output of a frame.
#[derive(Debug)]
pub struct Frame<'a> {
    /// Commands in paint order.
    pub commands: &'a [DrawCmd],
    /// The frame's strings (text commands point into it).
    pub text: &'a str,
}

#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        // Ids are already hashes.
        self.0 = v;
    }
}

type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;

#[derive(Debug, Clone, Copy, Default)]
struct WidgetState {
    scroll: f32,
    /// Largest scroll offset (content minus viewport), from the last layout.
    scroll_max: f32,
    last_frame: u64,
}

/// Hashes a key under a parent id (FNV-1a, then a final mix).
fn hash_id(parent: u64, key: &str) -> u64 {
    let mut h = parent ^ 0xcbf2_9ce4_8422_2325;
    for &b in key.as_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 32;
    h = h.wrapping_mul(0xd6e8_feb8_6659_fd93);
    h ^ (h >> 32)
}

/// The UI context: frame buffers and retained state.
pub struct Ui {
    theme: Theme,
    nodes: Vec<Node>,
    text: String,
    commands: Vec<DrawCmd>,
    stack: Vec<u32>,
    id_stack: Vec<u64>,
    state: IdMap<WidgetState>,
    input: UiInput,
    viewport: [f32; 2],
    frame: u64,
    /// Widget under the mouse after the previous frame's layout.
    hovered: u64,
    /// Widget holding the mouse button.
    active: u64,
    /// Widget with keyboard focus.
    focused: u64,
    /// Focusable widgets of the previous frame, in order (for Tab).
    focus_order: Vec<u64>,
    floating: Vec<u32>,
    /// Reused by `emit`: clip rectangles and (subtree end, clip depth) of open clips.
    clips: Vec<Rect>,
    open_clips: Vec<(u32, usize)>,
    /// Index of the node opened last.
    last: u32,
}

impl Ui {
    /// A UI with the given theme.
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            nodes: Vec::new(),
            text: String::new(),
            commands: Vec::new(),
            stack: Vec::new(),
            id_stack: Vec::new(),
            state: IdMap::default(),
            input: UiInput::default(),
            viewport: [0.0; 2],
            frame: 0,
            hovered: 0,
            active: 0,
            focused: 0,
            focus_order: Vec::new(),
            floating: Vec::new(),
            clips: Vec::new(),
            open_clips: Vec::new(),
            last: NONE,
        }
    }

    /// The theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Starts a frame: takes the input and the viewport size, and opens the root node.
    pub fn begin_frame(&mut self, input: &UiInput, viewport: [f32; 2]) {
        self.frame += 1;
        self.input.clone_from(input);
        self.viewport = viewport;
        // This frame's mouse against the previous frame's layout: hover and clicks
        // follow the pointer without delay; only the rectangles are a frame old.
        self.hit_test();
        self.nodes.clear();
        self.text.clear();
        self.stack.clear();
        self.id_stack.clear();
        self.handle_keys();
        // `active` is cleared at the end of the frame, so this frame's widgets still see
        // which one the button was released on.
        self.id_stack.push(0);
        self.open(0, Style::default(), None);
    }

    fn handle_keys(&mut self) {
        for i in 0..self.input.keys.len() {
            match self.input.keys[i] {
                UiKey::Tab if !self.focus_order.is_empty() => {
                    let n = self.focus_order.len();
                    let pos = self.focus_order.iter().position(|&f| f == self.focused);
                    let next = match (pos, self.input.shift) {
                        (Some(p), false) => (p + 1) % n,
                        (Some(p), true) => (p + n - 1) % n,
                        (None, false) => 0,
                        (None, true) => n - 1,
                    };
                    self.focused = self.focus_order[next];
                }
                UiKey::Escape => self.focused = 0,
                _ => {}
            }
        }
    }

    /// The id a key gets under the current parent.
    pub fn id(&self, key: &str) -> u64 {
        hash_id(self.id_stack.last().copied().unwrap_or(0), key)
    }

    fn open(&mut self, id: u64, style: Style, text: Option<TextSpan>) -> u32 {
        let index = self.nodes.len() as u32;
        let parent = self.stack.last().copied().unwrap_or(NONE);
        let mut style = style;
        if style.scroll {
            style.clip = true;
        }
        self.nodes.push(Node {
            id,
            parent,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            end: index + 1,
            style,
            text,
            interactive: false,
            focusable: false,
            content: [0.0; 2],
            fit: [0.0; 2],
            rect: Rect::default(),
        });
        if parent != NONE {
            let p = &mut self.nodes[parent as usize];
            if p.first_child == NONE {
                p.first_child = index;
            } else {
                let last = p.last_child;
                p.last_child = index;
                self.nodes[last as usize].next_sibling = index;
            }
            self.nodes[parent as usize].last_child = index;
        }
        self.stack.push(index);
        self.last = index;
        index
    }

    fn close(&mut self) {
        if let Some(i) = self.stack.pop() {
            self.nodes[i as usize].end = self.nodes.len() as u32;
        }
    }

    /// Opens a container; call [`Ui::end`] after its children. `key` must be unique
    /// among its siblings.
    pub fn begin(&mut self, key: &str, style: Style) -> u64 {
        let id = self.id(key);
        self.open(id, style, None);
        self.id_stack.push(id);
        if style.scroll {
            self.state.entry(id).or_default().last_frame = self.frame;
        }
        id
    }

    /// Closes the container opened last.
    pub fn end(&mut self) {
        self.id_stack.pop();
        self.close();
    }

    /// A container holding what `f` builds.
    pub fn container(&mut self, key: &str, style: Style, f: impl FnOnce(&mut Ui)) -> u64 {
        let id = self.begin(key, style);
        f(self);
        self.end();
        id
    }

    /// A row (children left to right).
    pub fn row(&mut self, key: &str, style: Style, f: impl FnOnce(&mut Ui)) -> u64 {
        self.container(
            key,
            Style {
                direction: Direction::Row,
                ..style
            },
            f,
        )
    }

    /// A column (children top to bottom).
    pub fn column(&mut self, key: &str, style: Style, f: impl FnOnce(&mut Ui)) -> u64 {
        self.container(
            key,
            Style {
                direction: Direction::Column,
                ..style
            },
            f,
        )
    }

    /// A leaf node with text.
    pub fn text_node(
        &mut self,
        key: &str,
        style: Style,
        text: &str,
        size: f32,
        color: Color,
        wrap: bool,
    ) -> u64 {
        let id = self.id(key);
        let start = self.text.len();
        self.text.push_str(text);
        let span = TextSpan {
            range: start..self.text.len(),
            size,
            color,
            wrap,
        };
        self.open(id, style, Some(span));
        self.close();
        id
    }

    /// Marks the node built last as reacting to the mouse (and to the keyboard if
    /// `focusable`), and returns how it was used.
    pub(crate) fn interact(&mut self, id: u64, focusable: bool) -> Response {
        if let Some(n) = self.nodes.get_mut(self.last as usize)
            && n.id == id
        {
            n.interactive = true;
            n.focusable = focusable;
        }
        let hovered = self.hovered == id && self.input.mouse.is_some();
        let pressed = hovered && self.input.mouse_pressed;
        if pressed {
            self.active = id;
            if focusable {
                self.focused = id;
            }
        }
        let active = self.active == id;
        let focused = self.focused == id;
        let released_on = active && hovered && self.input.mouse_released;
        let key_activate = focused
            && self
                .input
                .keys
                .iter()
                .any(|k| matches!(k, UiKey::Enter | UiKey::Space));
        Response {
            id,
            hovered,
            pressed,
            active: active && self.input.mouse_down,
            focused,
            clicked: released_on || key_activate,
        }
    }

    /// Ends the frame: lays out the tree, resolves the widget under the mouse and scroll
    /// input, and returns the draw commands.
    pub fn end_frame(&mut self, measure: &mut dyn TextMeasure) -> Frame<'_> {
        engine_profiler::zone!("ui::end_frame");
        while !self.stack.is_empty() {
            self.close();
        }
        let root = Rect::new(0.0, 0.0, self.viewport[0], self.viewport[1]);
        let state = &self.state;
        layout::layout(&mut self.nodes, &self.text, root, measure, &|id| {
            state.get(&id).map_or(0.0, |s| s.scroll)
        });
        self.update_scroll_limits();
        self.focus_order.clear();
        self.focus_order
            .extend(self.nodes.iter().filter(|n| n.focusable).map(|n| n.id));
        if !self.focus_order.contains(&self.focused) {
            self.focused = 0;
        }
        if !self.input.mouse_down {
            self.active = 0;
        }
        let frame = self.frame;
        self.state.retain(|_, s| frame - s.last_frame < 120);
        self.emit();
        Frame {
            commands: &self.commands,
            text: &self.text,
        }
    }

    fn update_scroll_limits(&mut self) {
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if !n.style.scroll {
                continue;
            }
            let main = match n.style.direction {
                Direction::Row => 0,
                Direction::Column => 1,
            };
            let max = (n.fit[main] - n.rect.size[main]).max(0.0);
            if let Some(s) = self.state.get_mut(&n.id) {
                s.scroll_max = max;
                s.scroll = s.scroll.clamp(0.0, max);
            }
        }
    }

    /// Finds the topmost interactive node under the mouse (floating layers above the
    /// rest, later nodes above earlier ones, clipped by clipping ancestors), and applies
    /// the wheel to the innermost scroll container under the mouse.
    fn hit_test(&mut self) {
        self.hovered = 0;
        let Some(mouse) = self.input.mouse else {
            return;
        };
        let mut best: Option<(bool, usize)> = None;
        let mut scroll_target: Option<(bool, usize)> = None;
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if !(n.interactive || n.style.scroll) || !self.visible_at(i, mouse) {
                continue;
            }
            let floating = self.in_floating(i);
            let key = (floating, i);
            if n.interactive && best.is_none_or(|b| key > b) {
                best = Some(key);
            }
            if n.style.scroll && scroll_target.is_none_or(|b| key > b) {
                scroll_target = Some(key);
            }
        }
        if let Some((_, i)) = best {
            self.hovered = self.nodes[i].id;
        }
        if self.input.wheel != 0.0
            && let Some((_, i)) = scroll_target
            && let Some(s) = self.state.get_mut(&self.nodes[i].id)
        {
            s.scroll = (s.scroll - self.input.wheel).clamp(0.0, s.scroll_max);
        }
    }

    /// Whether the subtree of `i` contains a floating node (which escapes clipping).
    fn has_floating(&self, i: u32) -> bool {
        let n = &self.nodes[i as usize];
        self.nodes[(i + 1) as usize..n.end as usize]
            .iter()
            .any(|c| c.style.floating.is_some())
    }

    fn in_floating(&self, mut i: usize) -> bool {
        loop {
            if self.nodes[i].style.floating.is_some() {
                return true;
            }
            let p = self.nodes[i].parent;
            if p == NONE {
                return false;
            }
            i = p as usize;
        }
    }

    fn visible_at(&self, i: usize, p: [f32; 2]) -> bool {
        if !self.nodes[i].rect.contains(p) {
            return false;
        }
        let mut a = self.nodes[i].parent;
        while a != NONE {
            let n = &self.nodes[a as usize];
            if n.style.floating.is_some() {
                // A floating layer escapes its ancestors' clips.
                return true;
            }
            if n.style.clip && !n.rect.contains(p) {
                return false;
            }
            a = n.parent;
        }
        true
    }

    fn emit(&mut self) {
        self.commands.clear();
        self.floating.clear();
        self.clips.clear();
        self.emit_range(0, self.nodes.len() as u32, true);
        let mut k = 0;
        while k < self.floating.len() {
            let i = self.floating[k];
            let end = self.nodes[i as usize].end;
            self.clips.clear();
            self.commands.push(DrawCmd::Clip(None));
            self.emit_range(i, end, false);
            k += 1;
        }
    }

    /// Emits nodes `start..end` (a pre-order range). Floating subtrees met on the way
    /// are deferred when `defer` (except the range's own root).
    fn emit_range(&mut self, start: u32, end: u32, defer: bool) {
        // Clip rectangles to restore when leaving a clipping node: (end index, depth).
        let mut open = core::mem::take(&mut self.open_clips);
        let mut clips = core::mem::take(&mut self.clips);
        open.clear();
        let mut i = start;
        while i < end {
            while let Some(&(e, depth)) = open.last()
                && i >= e
            {
                open.pop();
                clips.truncate(depth);
                self.commands.push(DrawCmd::Clip(clips.last().copied()));
            }
            let n = &self.nodes[i as usize];
            if n.style.floating.is_some() && (defer || i != start) {
                self.floating.push(i);
                i = n.end;
                continue;
            }
            let (rect, style, focused) = (n.rect, n.style, n.focusable && n.id == self.focused);
            // Subtrees entirely outside the current clip draw nothing (children of a
            // node lie inside it unless they float, and floating ones are deferred).
            if let Some(clip) = clips.last()
                && clip.intersect(&rect).size.contains(&0.0)
                && !self.has_floating(i)
            {
                i = n.end;
                continue;
            }
            if style.background.is_some() || style.border.is_some() || focused {
                let border = if focused {
                    Some((self.theme.focus, 2.0))
                } else {
                    style.border
                };
                self.commands.push(DrawCmd::Rect {
                    rect,
                    fill: style.background,
                    border,
                    radius: style.radius,
                });
            }
            if let Some(t) = &n.text {
                let p = style.padding;
                self.commands.push(DrawCmd::Text {
                    rect: Rect::new(
                        rect.pos[0] + p[0],
                        rect.pos[1] + p[1],
                        (rect.size[0] - p[0] - p[2]).max(0.0),
                        (rect.size[1] - p[1] - p[3]).max(0.0),
                    ),
                    range: t.range.clone(),
                    size: t.size,
                    color: t.color,
                    wrap: t.wrap,
                });
            }
            if style.clip && n.end > i + 1 {
                let clip = clips.last().map_or(rect, |c| c.intersect(&rect));
                open.push((n.end, clips.len()));
                clips.push(clip);
                self.commands.push(DrawCmd::Clip(Some(clip)));
            }
            i += 1;
        }
        while let Some((_, depth)) = open.pop() {
            clips.truncate(depth);
            self.commands.push(DrawCmd::Clip(clips.last().copied()));
        }
        self.open_clips = open;
        self.clips = clips;
    }

    /// Scroll offset of a scroll container, by id.
    pub fn scroll_offset(&self, id: u64) -> f32 {
        self.state.get(&id).map_or(0.0, |s| s.scroll)
    }

    /// The laid-out rectangle of a node from the last frame, by id.
    pub fn rect_of(&self, id: u64) -> Option<Rect> {
        self.nodes.iter().find(|n| n.id == id).map(|n| n.rect)
    }

    /// Nodes in the last frame.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}
