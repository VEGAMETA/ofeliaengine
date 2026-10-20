//! Window input for the editor: platform events gathered over a frame into the UI's
//! [`UiInput`] (in logical pixels), the shortcuts' key chords, and what the 3D viewport's
//! camera needs (drags and the wheel, in physical pixels and notches).
//!
//! The collector knows nothing about windows: the application forwards
//! `engine_platform` events to it, which keeps it testable anywhere.

use crate::Chord;
use engine_input::Key;
use engine_ui::{UiInput, UiKey};

/// Wheel movement in logical pixels per notch, for scrolling panels.
pub const WHEEL_PIXELS: f32 = 48.0;

/// Where the editor's copied text goes and pasted text comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Clipboard {
    /// Windows' clipboard.
    #[default]
    System,
    /// Text of the editor's own, shared with nothing (tests leave the user's clipboard
    /// alone).
    Local(String),
}

impl Clipboard {
    /// The text on it, if any.
    pub fn text(&self) -> Option<String> {
        match self {
            #[cfg(windows)]
            Self::System => engine_platform::clipboard::text(),
            #[cfg(not(windows))]
            Self::System => None,
            Self::Local(text) => Some(text.clone()),
        }
    }

    /// Puts `text` on it; false when Windows' clipboard is busy.
    pub fn set_text(&mut self, text: &str) -> bool {
        match self {
            #[cfg(windows)]
            Self::System => engine_platform::clipboard::set_text(text),
            #[cfg(not(windows))]
            Self::System => false,
            Self::Local(own) => {
                own.clear();
                own.push_str(text);
                true
            }
        }
    }
}

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
    pub(super) const BACK: u16 = 0x08;
    pub(super) const TAB: u16 = 0x09;
    pub(super) const RETURN: u16 = 0x0D;
    pub(super) const SHIFT: u16 = 0x10;
    pub(super) const CONTROL: u16 = 0x11;
    pub(super) const MENU: u16 = 0x12;
    pub(super) const ESCAPE: u16 = 0x1B;
    pub(super) const SPACE: u16 = 0x20;
    pub(super) const END: u16 = 0x23;
    pub(super) const HOME: u16 = 0x24;
    pub(super) const LEFT: u16 = 0x25;
    pub(super) const UP: u16 = 0x26;
    pub(super) const RIGHT: u16 = 0x27;
    pub(super) const DOWN: u16 = 0x28;
    pub(super) const DELETE: u16 = 0x2E;
    pub(super) const A: u16 = 0x41;
    pub(super) const C: u16 = 0x43;
    pub(super) const V: u16 = 0x56;
    pub(super) const X: u16 = 0x58;
    pub(super) const LWIN: u16 = 0x5B;
    pub(super) const RWIN: u16 = 0x5C;
    pub(super) const LSHIFT: u16 = 0xA0;
    pub(super) const RSHIFT: u16 = 0xA1;
    pub(super) const LCONTROL: u16 = 0xA2;
    pub(super) const RCONTROL: u16 = 0xA3;
    pub(super) const LMENU: u16 = 0xA4;
    pub(super) const RMENU: u16 = 0xA5;
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
    ctrl: bool,
    alt: bool,
    /// Physical keys held now, a bit per [`Key`]: for keys that act while held (flying the
    /// camera).
    held_keys: [u64; 4],
    /// Keys pressed this frame with the modifiers held then, for shortcuts.
    chords: Vec<Chord>,
    /// The virtual keys pressed this frame: the letter on the key in Latin layouts (the
    /// US one in others), for keys that mean their letter (a keyboard transform's X, Y, Z).
    letters: Vec<u16>,
    /// What Ctrl+V pastes from (and the editor copies to).
    pub clipboard: Clipboard,
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

    /// A key changed state: its Win32 virtual-key code, and its scan code and extended
    /// flag (the physical key shortcuts name, ADR 0021). Held keys repeat.
    pub fn key(&mut self, virtual_key: u16, scancode: u16, extended: bool, pressed: bool) {
        let physical = Key::from_scancode(scancode, extended);
        if let Some(k) = physical {
            let bit = 1u64 << (k as usize % 64);
            if pressed {
                self.held_keys[k as usize / 64] |= bit;
            } else {
                self.held_keys[k as usize / 64] &= !bit;
            }
        }
        match virtual_key {
            vk::SHIFT | vk::LSHIFT | vk::RSHIFT => {
                self.shift = pressed;
                return;
            }
            vk::CONTROL | vk::LCONTROL | vk::RCONTROL => {
                self.ctrl = pressed;
                return;
            }
            vk::MENU | vk::LMENU | vk::RMENU => {
                self.alt = pressed;
                return;
            }
            vk::LWIN | vk::RWIN => return,
            _ if !pressed => return,
            _ => self.letters.push(virtual_key),
        }
        match virtual_key {
            vk::TAB => self.ui.keys.push(UiKey::Tab),
            vk::RETURN => self.ui.keys.push(UiKey::Enter),
            vk::SPACE => self.ui.keys.push(UiKey::Space),
            vk::ESCAPE => self.ui.keys.push(UiKey::Escape),
            vk::LEFT => self.ui.keys.push(UiKey::Left),
            vk::RIGHT => self.ui.keys.push(UiKey::Right),
            vk::UP => self.ui.keys.push(UiKey::Up),
            vk::DOWN => self.ui.keys.push(UiKey::Down),
            vk::HOME => self.ui.keys.push(UiKey::Home),
            vk::END => self.ui.keys.push(UiKey::End),
            vk::BACK => self.ui.keys.push(UiKey::Backspace),
            vk::DELETE => self.ui.keys.push(UiKey::Delete),
            vk::A if self.ctrl => self.ui.keys.push(UiKey::SelectAll),
            vk::C if self.ctrl => self.ui.keys.push(UiKey::Copy),
            vk::X if self.ctrl => self.ui.keys.push(UiKey::Cut),
            vk::V if self.ctrl => {
                // The text field being edited pastes it this frame.
                self.ui.paste = self.clipboard.text().unwrap_or_default();
                self.ui.keys.push(UiKey::Paste);
            }
            _ => {}
        }
        if let Some(key) = physical {
            self.chords.push(Chord {
                ctrl: self.ctrl,
                shift: self.shift,
                alt: self.alt,
                key,
            });
        }
    }

    /// A character was typed (after the keyboard layout and dead keys): for the text field
    /// being edited. Control characters (Enter, Tab, Backspace) come as keys instead.
    pub fn char(&mut self, c: char) {
        if !c.is_control() {
            self.ui.text.push(c);
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
        self.ctrl = false;
        self.alt = false;
        self.held_keys = [0; 4];
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
        self.ui.ctrl = self.ctrl;
        &self.ui
    }

    /// The keys pressed this frame, with their modifiers: for the keymap.
    pub fn chords(&self) -> &[Chord] {
        &self.chords
    }

    /// The virtual-key codes pressed this frame (Win32 `VK_*`): what the key says, where
    /// its letter matters more than its place.
    pub fn letters(&self) -> &[u16] {
        &self.letters
    }

    /// Whether the physical key `key` is held.
    pub fn key_held(&self, key: Key) -> bool {
        (self.held_keys[key as usize / 64] & (1 << (key as usize % 64))) != 0
    }

    /// Whether Alt is held.
    pub fn alt(&self) -> bool {
        self.alt
    }

    /// The characters typed this frame (after the keyboard layout).
    pub fn text(&self) -> &str {
        &self.ui.text
    }

    /// Takes this frame's keys, typed text, button presses and wheel: something had them
    /// all (a keyboard transform), so the panels, the shortcuts and the camera do not.
    pub fn consume(&mut self) {
        self.pressed = [false; 3];
        self.wheel = 0.0;
        self.ui.keys.clear();
        self.ui.text.clear();
        self.ui.paste.clear();
        self.chords.clear();
        self.letters.clear();
    }

    /// Whether Control is held.
    pub fn ctrl(&self) -> bool {
        self.ctrl
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
        self.ui.text.clear();
        self.ui.paste.clear();
        self.chords.clear();
        self.letters.clear();
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
        input.key(0x10, 0x2A, false, true);
        input.key(0x09, 0x0F, false, true);
        input.key(0x41, 0x1E, false, true);
        input.key(0x1B, 0x01, false, true);
        input.key(0x09, 0x0F, false, false);
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

    #[test]
    fn held_keys_follow_presses_and_releases() {
        let mut input = InputCollector::new();
        input.key(0x57, 0x11, false, true); // W
        input.next_frame();
        assert!(input.key_held(Key::KeyW), "held across frames");
        input.key(0x57, 0x11, false, false);
        assert!(!input.key_held(Key::KeyW));
        input.key(0x41, 0x1E, false, true); // A
        input.focus_lost();
        assert!(!input.key_held(Key::KeyA), "dropped with the focus");
    }

    #[test]
    fn shortcuts_are_physical_keys_with_their_modifiers() {
        let mut input = InputCollector::new();
        input.key(0x11, 0x1D, false, true); // Ctrl
        input.key(0x5A, 0x2C, false, true); // Z
        input.key(0x10, 0x2A, false, true); // Shift
        input.key(0x5A, 0x2C, false, true); // Z again (held keys repeat)
        input.key(0x11, 0x1D, false, false);
        input.key(0x10, 0x2A, false, false);
        input.key(0x2E, 0x53, true, true); // Delete
        let want = ["Ctrl+Z", "Ctrl+Shift+Z", "Delete"].map(|c| Chord::parse(c).unwrap());
        assert_eq!(input.chords(), want);
        assert!(!input.ui_input(1.0).ctrl);
        input.next_frame();
        assert!(input.chords().is_empty());
        input.key(0x11, 0x1D, false, true);
        assert!(input.ui_input(1.0).ctrl && input.ctrl());
        input.focus_lost();
        assert!(!input.ctrl(), "modifiers released elsewhere are dropped");
    }
}
