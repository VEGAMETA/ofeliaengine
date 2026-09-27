//! Window input for the editor: platform events gathered over a frame into the UI's
//! [`UiInput`] (in logical pixels) plus what the 3D viewport's camera needs (drags and the
//! wheel, in physical pixels and notches).
//!
//! The collector knows nothing about windows: the application forwards
//! `engine_platform` events to it, which keeps it testable anywhere.

use engine_ui::{UiInput, UiKey};

/// Wheel movement in logical pixels per notch, for scrolling panels.
pub const WHEEL_PIXELS: f32 = 48.0;

/// Mouse buttons the editor tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// Primary: the UI's button.
    Left = 0,
    /// Secondary: camera look in the viewport.
    Right = 1,
    /// Wheel click: camera pan in the viewport.
    Middle = 2,
}

/// Win32 virtual-key codes the UI reacts to.
mod vk {
    pub(super) const TAB: u16 = 0x09;
    pub(super) const RETURN: u16 = 0x0D;
    pub(super) const SHIFT: u16 = 0x10;
    pub(super) const ESCAPE: u16 = 0x1B;
    pub(super) const SPACE: u16 = 0x20;
    pub(super) const LSHIFT: u16 = 0xA0;
    pub(super) const RSHIFT: u16 = 0xA1;
}

/// A frame's input, gathered from window events.
///
/// Forward events as they arrive, then call [`InputCollector::ui_input`] once per frame
/// for the UI, read the camera state, and finish with [`InputCollector::next_frame`].
#[derive(Debug, Default)]
pub struct InputCollector {
    ui: UiInput,
    /// Cursor position in physical pixels.
    cursor: Option<[f32; 2]>,
    held: [bool; 3],
    pressed: [bool; 3],
    released: [bool; 3],
    /// Cursor movement while the right or middle button was held, physical pixels.
    drag: [f32; 2],
    /// Wheel notches (positive: away from the user).
    wheel: f32,
    shift: bool,
}

impl InputCollector {
    /// An empty collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cursor moved to `(x, y)` in physical client pixels.
    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        if let Some([px, py]) = self.cursor
            && (self.held[Button::Right as usize] || self.held[Button::Middle as usize])
        {
            self.drag[0] += x - px;
            self.drag[1] += y - py;
        }
        self.cursor = Some([x, y]);
    }

    /// The cursor left the window.
    pub fn cursor_left(&mut self) {
        self.cursor = None;
    }

    /// A mouse button changed state.
    pub fn button(&mut self, button: Button, pressed: bool) {
        let i = button as usize;
        if pressed && !self.held[i] {
            self.pressed[i] = true;
        }
        if !pressed && self.held[i] {
            self.released[i] = true;
        }
        self.held[i] = pressed;
    }

    /// The wheel turned by `notches` (positive: away from the user).
    pub fn wheel(&mut self, notches: f32) {
        self.wheel += notches;
    }

    /// A key changed state (Win32 virtual-key code).
    pub fn key(&mut self, virtual_key: u16, pressed: bool) {
        match virtual_key {
            vk::SHIFT | vk::LSHIFT | vk::RSHIFT => self.shift = pressed,
            _ if !pressed => {}
            vk::TAB => self.ui.keys.push(UiKey::Tab),
            vk::RETURN => self.ui.keys.push(UiKey::Enter),
            vk::SPACE => self.ui.keys.push(UiKey::Space),
            vk::ESCAPE => self.ui.keys.push(UiKey::Escape),
            _ => {}
        }
    }

    /// The window lost keyboard focus: buttons and modifiers released elsewhere never
    /// report back, so drop them.
    pub fn focus_lost(&mut self) {
        for i in 0..3 {
            if self.held[i] {
                self.released[i] = true;
            }
            self.held[i] = false;
        }
        self.shift = false;
    }

    /// This frame's UI input, in logical pixels (`scale` physical pixels per logical one).
    pub fn ui_input(&mut self, scale: f32) -> &UiInput {
        let left = Button::Left as usize;
        self.ui.mouse = self.cursor.map(|[x, y]| [x / scale, y / scale]);
        self.ui.mouse_down = self.held[left];
        self.ui.mouse_pressed = self.pressed[left];
        self.ui.mouse_released = self.released[left];
        self.ui.wheel = self.wheel * WHEEL_PIXELS;
        self.ui.shift = self.shift;
        &self.ui
    }

    /// Cursor position in physical pixels.
    pub fn cursor(&self) -> Option<[f32; 2]> {
        self.cursor
    }

    /// Whether Shift is held.
    pub fn shift(&self) -> bool {
        self.shift
    }

    /// Whether `button` is held.
    pub fn held(&self, button: Button) -> bool {
        self.held[button as usize]
    }

    /// Whether `button` went down this frame.
    pub fn pressed(&self, button: Button) -> bool {
        self.pressed[button as usize]
    }

    /// Cursor movement this frame while the right or middle button was held (physical
    /// pixels).
    pub fn drag(&self) -> [f32; 2] {
        self.drag
    }

    /// Wheel notches this frame.
    pub fn wheel_notches(&self) -> f32 {
        self.wheel
    }

    /// Starts the next frame: clears this frame's edges, keys, drag and wheel. Held
    /// buttons, modifiers and the cursor carry over. The key list keeps its storage.
    pub fn next_frame(&mut self) {
        self.pressed = [false; 3];
        self.released = [false; 3];
        self.drag = [0.0; 2];
        self.wheel = 0.0;
        self.ui.keys.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_are_edges_and_positions_are_logical() {
        let mut input = InputCollector::new();
        input.cursor_moved(300.0, 150.0);
        input.button(Button::Left, true);
        let ui = input.ui_input(1.5);
        assert_eq!(ui.mouse, Some([200.0, 100.0]));
        assert!(ui.mouse_pressed && ui.mouse_down && !ui.mouse_released);
        input.next_frame();
        let ui = input.ui_input(1.5);
        assert!(!ui.mouse_pressed && ui.mouse_down, "held, no new edge");
        input.next_frame();
        input.button(Button::Left, false);
        let ui = input.ui_input(1.5);
        assert!(ui.mouse_released && !ui.mouse_down);
    }

    #[test]
    fn drags_count_only_with_camera_buttons_held() {
        let mut input = InputCollector::new();
        input.cursor_moved(10.0, 10.0);
        input.cursor_moved(20.0, 10.0);
        assert_eq!(input.drag(), [0.0, 0.0], "no button: a hover, not a drag");
        input.button(Button::Right, true);
        input.cursor_moved(25.0, 4.0);
        input.cursor_moved(30.0, 2.0);
        assert_eq!(input.drag(), [10.0, -8.0]);
        input.next_frame();
        assert_eq!(input.drag(), [0.0, 0.0]);
        assert!(input.held(Button::Right), "held across frames");
    }

    #[test]
    fn keys_wheel_and_focus_loss() {
        let mut input = InputCollector::new();
        input.key(0x10, true);
        input.key(0x09, true);
        input.key(0x41, true);
        input.key(0x1B, true);
        input.key(0x09, false);
        input.wheel(1.0);
        input.wheel(0.5);
        let ui = input.ui_input(1.0);
        assert_eq!(ui.keys, [UiKey::Tab, UiKey::Escape]);
        assert!(ui.shift);
        assert_eq!(ui.wheel, 1.5 * WHEEL_PIXELS);
        input.next_frame();
        input.button(Button::Left, true);
        input.next_frame();
        input.focus_lost();
        let ui = input.ui_input(1.0);
        assert!(ui.keys.is_empty() && !ui.shift && ui.wheel == 0.0);
        assert!(
            ui.mouse_released && !ui.mouse_down,
            "a held button is released"
        );
    }
}
