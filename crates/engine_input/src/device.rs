//! Physical inputs and the events that change them.

use std::time::Instant;

macro_rules! keys {
    ($($(#[$doc:meta])* $name:ident),+ $(,)?) => {
        /// A physical key, named after its position on a US keyboard (as W3C `KeyboardEvent.code`
        /// and Godot's physical keycodes do). Bindings to physical keys work on every layout:
        /// `W` is the key above `S` on AZERTY too.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum Key {
            $($(#[$doc])* #[doc = concat!("The `", stringify!($name), "` key.")] $name,)+
        }

        impl Key {
            /// Every key, in declaration order.
            pub const ALL: &'static [Key] = &[$(Key::$name),+];

            /// The key's name as written in input maps (`"KeyW"`, `"Space"`).
            pub const fn name(self) -> &'static str {
                match self {
                    $(Key::$name => stringify!($name),)+
                }
            }

            /// Parses a name produced by [`Key::name`].
            pub fn from_name(name: &str) -> Option<Key> {
                Key::ALL.iter().copied().find(|k| k.name() == name)
            }
        }
    };
}

keys!(
    Escape,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Digit0,
    Minus,
    Equal,
    Backspace,
    Tab,
    KeyQ,
    KeyW,
    KeyE,
    KeyR,
    KeyT,
    KeyY,
    KeyU,
    KeyI,
    KeyO,
    KeyP,
    BracketLeft,
    BracketRight,
    Enter,
    ControlLeft,
    KeyA,
    KeyS,
    KeyD,
    KeyF,
    KeyG,
    KeyH,
    KeyJ,
    KeyK,
    KeyL,
    Semicolon,
    Quote,
    Backquote,
    ShiftLeft,
    Backslash,
    KeyZ,
    KeyX,
    KeyC,
    KeyV,
    KeyB,
    KeyN,
    KeyM,
    Comma,
    Period,
    Slash,
    ShiftRight,
    NumpadMultiply,
    AltLeft,
    Space,
    CapsLock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    NumLock,
    ScrollLock,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadSubtract,
    Numpad4,
    Numpad5,
    Numpad6,
    NumpadAdd,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad0,
    NumpadDecimal,
    IntlBackslash,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,
    NumpadEnter,
    ControlRight,
    NumpadDivide,
    PrintScreen,
    AltRight,
    Home,
    ArrowUp,
    PageUp,
    ArrowLeft,
    ArrowRight,
    End,
    ArrowDown,
    PageDown,
    Insert,
    Delete,
    MetaLeft,
    MetaRight,
    ContextMenu,
    Pause,
);

/// Number of [`Key`] variants.
pub const KEY_COUNT: usize = Key::ALL.len();

impl Key {
    /// The key for a PC scan code (set 1 make code, as Windows raw input reports it) and
    /// its E0 prefix. `None` for codes without a key here.
    pub fn from_scancode(code: u16, extended: bool) -> Option<Key> {
        use Key::*;
        let key = if extended {
            match code {
                0x1C => NumpadEnter,
                0x1D => ControlRight,
                0x35 => NumpadDivide,
                0x37 => PrintScreen,
                0x38 => AltRight,
                0x45 => NumLock,
                0x47 => Home,
                0x48 => ArrowUp,
                0x49 => PageUp,
                0x4B => ArrowLeft,
                0x4D => ArrowRight,
                0x4F => End,
                0x50 => ArrowDown,
                0x51 => PageDown,
                0x52 => Insert,
                0x53 => Delete,
                0x5B => MetaLeft,
                0x5C => MetaRight,
                0x5D => ContextMenu,
                _ => return None,
            }
        } else {
            match code {
                // Escape .. CapsLock and F1 .. F10 follow declaration order.
                0x01..=0x3A => Key::ALL[code as usize - 1],
                0x3B..=0x44 => Key::ALL[F1 as usize + (code as usize - 0x3B)],
                0x45 => NumLock,
                0x46 => ScrollLock,
                0x47..=0x53 => Key::ALL[Numpad7 as usize + (code as usize - 0x47)],
                0x56 => IntlBackslash,
                0x57 => F11,
                0x58 => F12,
                0x64..=0x6E => Key::ALL[F13 as usize + (code as usize - 0x64)],
                0x76 => F24,
                // Windows reports Pause as E1 1D 45; raw input delivers it as 0x1D with
                // an E1 flag, which callers map before calling (see `platform`).
                _ => return None,
            }
        };
        Some(key)
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum MouseButton {
    /// Primary.
    Left,
    /// Secondary.
    Right,
    /// Wheel click.
    Middle,
    /// Back (X1).
    Back,
    /// Forward (X2).
    Forward,
}

/// A gamepad button, by position (Xbox names in parentheses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum GamepadButton {
    /// Bottom face button (A).
    South,
    /// Right face button (B).
    East,
    /// Left face button (X).
    West,
    /// Top face button (Y).
    North,
    /// Left shoulder (LB).
    LeftShoulder,
    /// Right shoulder (RB).
    RightShoulder,
    /// Left stick click.
    LeftStick,
    /// Right stick click.
    RightStick,
    /// Menu / start.
    Start,
    /// View / select / back.
    Select,
    /// D-pad up.
    DPadUp,
    /// D-pad down.
    DPadDown,
    /// D-pad left.
    DPadLeft,
    /// D-pad right.
    DPadRight,
    /// Guide / home.
    Guide,
}

/// Number of [`GamepadButton`] variants.
pub const GAMEPAD_BUTTON_COUNT: usize = 15;

/// A gamepad axis. Sticks are −1..1 with +Y up; triggers 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum GamepadAxis {
    /// Left stick, horizontal.
    LeftX,
    /// Left stick, vertical (up is positive).
    LeftY,
    /// Right stick, horizontal.
    RightX,
    /// Right stick, vertical (up is positive).
    RightY,
    /// Left trigger (LT).
    LeftTrigger,
    /// Right trigger (RT).
    RightTrigger,
}

/// Number of [`GamepadAxis`] variants.
pub const GAMEPAD_AXIS_COUNT: usize = 6;

/// Gamepads the input system tracks at once.
pub const MAX_GAMEPADS: usize = 4;

/// A gamepad stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stick {
    /// Left stick.
    Left,
    /// Right stick.
    Right,
}

impl Stick {
    pub(crate) fn axes(self) -> (GamepadAxis, GamepadAxis) {
        match self {
            Stick::Left => (GamepadAxis::LeftX, GamepadAxis::LeftY),
            Stick::Right => (GamepadAxis::RightX, GamepadAxis::RightY),
        }
    }
}

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventKind {
    /// A key went down or up.
    Key {
        /// Which key.
        key: Key,
        /// Down (`true`) or up.
        pressed: bool,
    },
    /// A mouse button went down or up.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Down (`true`) or up.
        pressed: bool,
    },
    /// Relative mouse motion in device counts (no acceleration).
    MouseMotion {
        /// Right is positive.
        dx: f32,
        /// Down is positive (screen convention).
        dy: f32,
    },
    /// Wheel motion in notches.
    Wheel {
        /// Right is positive.
        x: f32,
        /// Away from the user is positive.
        y: f32,
    },
    /// A gamepad button went down or up.
    GamepadButton {
        /// Pad index, below [`MAX_GAMEPADS`].
        pad: u8,
        /// Which button.
        button: GamepadButton,
        /// Down (`true`) or up.
        pressed: bool,
    },
    /// A gamepad axis moved.
    GamepadAxis {
        /// Pad index, below [`MAX_GAMEPADS`].
        pad: u8,
        /// Which axis.
        axis: GamepadAxis,
        /// New value.
        value: f32,
    },
    /// A gamepad was connected or disconnected. Disconnecting releases its inputs.
    GamepadConnection {
        /// Pad index, below [`MAX_GAMEPADS`].
        pad: u8,
        /// Connected (`true`) or gone.
        connected: bool,
    },
}

/// A timestamped input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    /// When the input arrived (raw input thread's estimate, see `engine_platform::raw_input`).
    pub time: Instant,
    /// What changed.
    pub kind: EventKind,
}
