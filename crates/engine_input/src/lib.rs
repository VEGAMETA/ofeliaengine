//! Input actions in the spirit of Godot's input map (research note `docs/research/input.md`).
//!
//! - **Actions** have names ("jump", "move") and kinds: buttons, 1D axes, 2D axes.
//! - **Bindings** tie physical inputs to them:
//!   - keys (by position, layout-independent), mouse buttons, wheel and motion;
//!   - gamepad buttons, sticks and triggers;
//!   - composites (WASD) and chords (Ctrl+S).
//! - **Contexts with priorities** (gameplay, UI, vehicle) decide which bindings see an
//!   input.
//! - **Two views of the state:**
//!   - [`Input::sample_tick`] for the fixed simulation tick: presses between ticks are
//!     latched, so a tap shorter than a tick is never lost;
//!   - [`Input::sample_frame`] for rendering and UI.
//! - **A tick's [`ActionState`] is the network user command** ([`ActionState::encode`]):
//!   values are quantized when sampled, and presses carry their sub-tick time.
//!
//! On Windows, `platform` converts raw input events and polls XInput gamepads.

mod device;
mod map;
mod net;
#[cfg(windows)]
pub mod platform;
mod state;

pub use device::{
    EventKind, GAMEPAD_AXIS_COUNT, GAMEPAD_BUTTON_COUNT, GamepadAxis, GamepadButton, InputEvent,
    KEY_COUNT, Key, MAX_GAMEPADS, MouseButton, Stick,
};
pub use map::{
    ActionId, ActionKind, Binding, ButtonSource, ContextId, DEFAULT_DEADZONE, InputMap, MapError,
    Modifiers,
};
pub use net::DecodeError;
pub use state::{ActionRef, ActionState, Actions, DeviceFilter, Input, VALUE_ONE};

#[cfg(test)]
mod tests;
