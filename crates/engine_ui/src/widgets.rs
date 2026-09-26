//! Basic widgets built from nodes: labels, paragraphs, buttons, spacers, panels, scroll
//! areas. Editor widgets (inspector fields, trees, docking) build on these.

use crate::{Align, Direction, Size, Style, Ui};

/// How a widget was used this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Response {
    /// The widget's id.
    pub id: u64,
    /// Under the mouse (from the last layout).
    pub hovered: bool,
    /// The primary button went down on it this frame.
    pub pressed: bool,
    /// Held down.
    pub active: bool,
    /// Has keyboard focus.
    pub focused: bool,
    /// Clicked: released over it after pressing it, or Enter/Space while focused.
    pub clicked: bool,
}

impl Ui {
    /// A line of text in the theme's size and color.
    pub fn label(&mut self, text: &str) -> u64 {
        let t = *self.theme();
        self.text_node(text, Style::default(), text, t.text_size, t.text, false)
    }

    /// A line of text with an explicit key (when the text is not unique or changes).
    pub fn label_keyed(&mut self, key: &str, text: &str) -> u64 {
        let t = *self.theme();
        self.text_node(key, Style::default(), text, t.text_size, t.text, false)
    }

    /// Text wrapped to the available width.
    pub fn paragraph(&mut self, key: &str, text: &str) -> u64 {
        let t = *self.theme();
        let style = Style {
            size: [Size::Grow(1.0), Size::Fit],
            ..Style::default()
        };
        self.text_node(key, style, text, t.text_size, t.text, true)
    }

    /// A push button; `text` is also its key.
    pub fn button(&mut self, text: &str) -> Response {
        self.button_keyed(text, text)
    }

    /// A push button with an explicit key.
    pub fn button_keyed(&mut self, key: &str, text: &str) -> Response {
        let t = *self.theme();
        let id = self.id(key);
        let r = self.interact_preview(id);
        let fill = if r.0 {
            t.button_active
        } else if r.1 {
            t.button_hover
        } else {
            t.button
        };
        let style = Style {
            padding: [
                t.button_padding[0],
                t.button_padding[1],
                t.button_padding[0],
                t.button_padding[1],
            ],
            background: Some(fill),
            radius: t.radius,
            align_main: Align::Center,
            align_cross: Align::Center,
            ..Style::default()
        };
        self.text_node(key, style, text, t.text_size, t.text, false);
        self.interact(id, true)
    }

    /// (active, hovered) of a widget before it is built, for choosing its look.
    fn interact_preview(&self, id: u64) -> (bool, bool) {
        (self.is_active(id), self.is_hovered(id))
    }

    /// Whether the widget holds the mouse.
    pub fn is_active(&self, id: u64) -> bool {
        self.active == id && self.input.mouse_down
    }

    /// Whether the widget is under the mouse.
    pub fn is_hovered(&self, id: u64) -> bool {
        self.hovered == id && self.input.mouse.is_some()
    }

    /// Empty space that grows along the parent's main axis.
    pub fn spacer(&mut self, key: &str) {
        self.container(key, Style::fill(), |_| {});
    }

    /// A panel: a column with the theme's background and padding.
    pub fn panel(&mut self, key: &str, style: Style, f: impl FnOnce(&mut Ui)) -> u64 {
        let bg = self.theme().panel;
        self.column(
            key,
            Style {
                background: Some(bg),
                ..style
            },
            f,
        )
    }

    /// A vertically scrolling column that fills its parent.
    pub fn scroll_column(&mut self, key: &str, f: impl FnOnce(&mut Ui)) -> u64 {
        self.container(
            key,
            Style {
                size: [Size::Grow(1.0), Size::Grow(1.0)],
                direction: Direction::Column,
                scroll: true,
                ..Style::default()
            },
            f,
        )
    }
}
