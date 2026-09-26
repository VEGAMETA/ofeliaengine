//! Windows devices: raw keyboard and mouse events from `engine_platform::raw_input`, and
//! XInput gamepads.

use crate::device::{
    EventKind, GamepadAxis, GamepadButton, InputEvent, Key, MAX_GAMEPADS, MouseButton,
};
use engine_platform::raw_input::{RawInputEvent, RawInputKind};
use engine_platform::window;
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK, XINPUT_GAMEPAD_DPAD_DOWN,
    XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT, XINPUT_GAMEPAD_DPAD_UP,
    XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB, XINPUT_GAMEPAD_RIGHT_SHOULDER,
    XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START, XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y,
    XINPUT_STATE, XInputGetState,
};

const VK_PAUSE: u16 = 0x13;
const VK_NUMLOCK: u16 = 0x90;

/// Converts a raw input event. `None` for keys without a [`Key`] and absolute mouse
/// positions (tablets, remote desktop), which are not relative motion.
pub fn from_raw(event: &RawInputEvent) -> Option<InputEvent> {
    let kind = match event.kind {
        RawInputKind::Key {
            scancode,
            extended,
            virtual_key,
            pressed,
        } => {
            // Pause and Num Lock share scan codes with other keys; the virtual key
            // tells them apart.
            let key = match virtual_key {
                VK_PAUSE => Key::Pause,
                VK_NUMLOCK => Key::NumLock,
                _ => Key::from_scancode(scancode, extended)?,
            };
            EventKind::Key { key, pressed }
        }
        RawInputKind::MouseButton { button, pressed } => EventKind::MouseButton {
            button: match button {
                window::MouseButton::Left => MouseButton::Left,
                window::MouseButton::Right => MouseButton::Right,
                window::MouseButton::Middle => MouseButton::Middle,
                window::MouseButton::X1 => MouseButton::Back,
                window::MouseButton::X2 => MouseButton::Forward,
            },
            pressed,
        },
        RawInputKind::MouseMove { absolute: true, .. } => return None,
        RawInputKind::MouseMove { dx, dy, .. } => EventKind::MouseMotion {
            dx: dx as f32,
            dy: dy as f32,
        },
        RawInputKind::Wheel { x, y } => EventKind::Wheel { x, y },
    };
    Some(InputEvent {
        time: event.time,
        kind,
    })
}

const BUTTONS: [(u16, GamepadButton); 14] = [
    (XINPUT_GAMEPAD_A, GamepadButton::South),
    (XINPUT_GAMEPAD_B, GamepadButton::East),
    (XINPUT_GAMEPAD_X, GamepadButton::West),
    (XINPUT_GAMEPAD_Y, GamepadButton::North),
    (XINPUT_GAMEPAD_LEFT_SHOULDER, GamepadButton::LeftShoulder),
    (XINPUT_GAMEPAD_RIGHT_SHOULDER, GamepadButton::RightShoulder),
    (XINPUT_GAMEPAD_LEFT_THUMB, GamepadButton::LeftStick),
    (XINPUT_GAMEPAD_RIGHT_THUMB, GamepadButton::RightStick),
    (XINPUT_GAMEPAD_START, GamepadButton::Start),
    (XINPUT_GAMEPAD_BACK, GamepadButton::Select),
    (XINPUT_GAMEPAD_DPAD_UP, GamepadButton::DPadUp),
    (XINPUT_GAMEPAD_DPAD_DOWN, GamepadButton::DPadDown),
    (XINPUT_GAMEPAD_DPAD_LEFT, GamepadButton::DPadLeft),
    (XINPUT_GAMEPAD_DPAD_RIGHT, GamepadButton::DPadRight),
];

/// How often an empty XInput slot is checked for a new controller. `XInputGetState` on
/// an empty slot can take hundreds of microseconds, so empty slots are not polled every
/// frame; one slot is checked per interval, in turn.
const RESCAN: Duration = Duration::from_millis(500);

fn stick(v: i16) -> f32 {
    (f32::from(v) / 32767.0).max(-1.0)
}

/// XInput gamepads (Xbox-compatible, up to [`MAX_GAMEPADS`]). Poll once per frame or
/// tick; changes come out as events with the poll's time.
#[derive(Debug)]
pub struct XInput {
    connected: [bool; MAX_GAMEPADS],
    buttons: [u16; MAX_GAMEPADS],
    axes: [[f32; 6]; MAX_GAMEPADS],
    next_scan: Instant,
    scan_slot: usize,
}

impl Default for XInput {
    fn default() -> Self {
        Self::new()
    }
}

impl XInput {
    /// Starts with every slot checked on the first poll.
    pub fn new() -> Self {
        Self {
            connected: [false; MAX_GAMEPADS],
            buttons: [0; MAX_GAMEPADS],
            axes: [[0.0; 6]; MAX_GAMEPADS],
            next_scan: Instant::now(),
            scan_slot: 0,
        }
    }

    /// Whether gamepad `pad` is connected.
    pub fn connected(&self, pad: usize) -> bool {
        self.connected.get(pad).copied().unwrap_or(false)
    }

    /// Reads connected pads (and, from time to time, one empty slot) and reports changes.
    pub fn poll(&mut self, now: Instant, mut emit: impl FnMut(InputEvent)) {
        let scan = if now >= self.next_scan {
            self.next_scan = now + RESCAN;
            self.scan_slot = (self.scan_slot + 1) % MAX_GAMEPADS;
            Some(self.scan_slot)
        } else {
            None
        };
        for pad in 0..MAX_GAMEPADS {
            if !self.connected[pad] && scan != Some(pad) {
                continue;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: `state` is a valid out-pointer; `pad` is below XUSER_MAX_COUNT.
            let result = unsafe { XInputGetState(pad as u32, &mut state) };
            let index = pad as u8;
            let connected = result == 0;
            if connected != self.connected[pad] {
                self.connected[pad] = connected;
                emit(InputEvent {
                    time: now,
                    kind: EventKind::GamepadConnection {
                        pad: index,
                        connected,
                    },
                });
                if !connected {
                    self.buttons[pad] = 0;
                    self.axes[pad] = [0.0; 6];
                    continue;
                }
            }
            if !connected {
                continue;
            }
            let g = state.Gamepad;
            let changed = g.wButtons ^ self.buttons[pad];
            for &(mask, button) in &BUTTONS {
                if changed & mask != 0 {
                    emit(InputEvent {
                        time: now,
                        kind: EventKind::GamepadButton {
                            pad: index,
                            button,
                            pressed: g.wButtons & mask != 0,
                        },
                    });
                }
            }
            self.buttons[pad] = g.wButtons;
            let axes = [
                (GamepadAxis::LeftX, stick(g.sThumbLX)),
                (GamepadAxis::LeftY, stick(g.sThumbLY)),
                (GamepadAxis::RightX, stick(g.sThumbRX)),
                (GamepadAxis::RightY, stick(g.sThumbRY)),
                (GamepadAxis::LeftTrigger, f32::from(g.bLeftTrigger) / 255.0),
                (
                    GamepadAxis::RightTrigger,
                    f32::from(g.bRightTrigger) / 255.0,
                ),
            ];
            for (axis, value) in axes {
                if self.axes[pad][axis as usize] != value {
                    self.axes[pad][axis as usize] = value;
                    emit(InputEvent {
                        time: now,
                        kind: EventKind::GamepadAxis {
                            pad: index,
                            axis,
                            value,
                        },
                    });
                }
            }
        }
    }
}
