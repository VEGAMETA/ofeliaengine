//! Bindings as text (ADR 0127's S1): how `settings/input.settings` and the editor write
//! them, read back by [`Binding::parse`].
//!
//! | Binding | Text |
//! |---|---|
//! | a key, a mouse button, a pad button | `KeyW`, `Mouse.Left`, `Pad.South` |
//! | with modifier keys | `ControlLeft+ShiftLeft+KeyS` |
//! | a pad axis pushed one way, a wheel notch | `Pad.LeftTrigger+`, `Pad.LeftX-`, `Wheel.Up` |
//! | two buttons for a 1D axis (negative, positive) | `KeyA / KeyD` |
//! | an analog pad axis | `Axis.LeftX`, `Axis.LeftY*-1` |
//! | four buttons for a 2D axis (up, down, left, right) | `KeyW, KeyS, KeyA, KeyD` |
//! | a stick | `Stick.Left`, `Stick.Right*0.5` |
//! | mouse motion | `MouseMotion*0.1` |

use crate::device::{GamepadAxis, GamepadButton, Key, MouseButton, Stick};
use crate::map::{Binding, ButtonSource, Modifiers};

const MOUSE: [(MouseButton, &str); 5] = [
    (MouseButton::Left, "Left"),
    (MouseButton::Right, "Right"),
    (MouseButton::Middle, "Middle"),
    (MouseButton::Back, "Back"),
    (MouseButton::Forward, "Forward"),
];

const PAD: [(GamepadButton, &str); 15] = [
    (GamepadButton::South, "South"),
    (GamepadButton::East, "East"),
    (GamepadButton::West, "West"),
    (GamepadButton::North, "North"),
    (GamepadButton::LeftShoulder, "LeftShoulder"),
    (GamepadButton::RightShoulder, "RightShoulder"),
    (GamepadButton::LeftStick, "LeftStick"),
    (GamepadButton::RightStick, "RightStick"),
    (GamepadButton::Start, "Start"),
    (GamepadButton::Select, "Select"),
    (GamepadButton::DPadUp, "DPadUp"),
    (GamepadButton::DPadDown, "DPadDown"),
    (GamepadButton::DPadLeft, "DPadLeft"),
    (GamepadButton::DPadRight, "DPadRight"),
    (GamepadButton::Guide, "Guide"),
];

const AXES: [(GamepadAxis, &str); 6] = [
    (GamepadAxis::LeftX, "LeftX"),
    (GamepadAxis::LeftY, "LeftY"),
    (GamepadAxis::RightX, "RightX"),
    (GamepadAxis::RightY, "RightY"),
    (GamepadAxis::LeftTrigger, "LeftTrigger"),
    (GamepadAxis::RightTrigger, "RightTrigger"),
];

fn name_of<T: PartialEq + Copy>(table: &[(T, &'static str)], value: T) -> &'static str {
    table
        .iter()
        .find(|(v, _)| *v == value)
        .map_or("?", |(_, n)| n)
}

fn value_of<T: Copy>(table: &[(T, &str)], name: &str) -> Option<T> {
    table.iter().find(|(_, n)| *n == name).map(|(v, _)| *v)
}

impl ButtonSource {
    /// This input as text (`KeyW`, `Mouse.Left`, `Pad.South`, `Pad.LeftX+`, `Wheel.Up`).
    pub fn to_text(self) -> String {
        match self {
            Self::Key(k) => k.name().to_owned(),
            Self::Mouse(b) => format!("Mouse.{}", name_of(&MOUSE, b)),
            Self::Gamepad(b) => format!("Pad.{}", name_of(&PAD, b)),
            Self::AxisPositive(a) => format!("Pad.{}+", name_of(&AXES, a)),
            Self::AxisNegative(a) => format!("Pad.{}-", name_of(&AXES, a)),
            Self::WheelUp => String::from("Wheel.Up"),
            Self::WheelDown => String::from("Wheel.Down"),
        }
    }

    /// An input read from its text ([`Self::to_text`]'s).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some(name) = text.strip_prefix("Mouse.") {
            return value_of(&MOUSE, name).map(Self::Mouse);
        }
        if let Some(name) = text.strip_prefix("Pad.") {
            if let Some(axis) = name.strip_suffix('+') {
                return value_of(&AXES, axis).map(Self::AxisPositive);
            }
            if let Some(axis) = name.strip_suffix('-') {
                return value_of(&AXES, axis).map(Self::AxisNegative);
            }
            return value_of(&PAD, name).map(Self::Gamepad);
        }
        match text {
            "Wheel.Up" => Some(Self::WheelUp),
            "Wheel.Down" => Some(Self::WheelDown),
            _ => Key::from_name(text).map(Self::Key),
        }
    }
}

/// `value` with its multiplier (`*0.5`) when it is not 1.
fn scaled(value: String, scale: f32) -> String {
    if scale == 1.0 {
        value
    } else {
        format!("{value}*{scale}")
    }
}

/// `text` split from its multiplier (`Stick.Left*0.5` → `Stick.Left`, 0.5).
fn split_scale(text: &str) -> Result<(&str, f32), String> {
    match text.split_once('*') {
        Some((value, scale)) => scale
            .trim()
            .parse::<f32>()
            .map(|s| (value.trim(), s))
            .map_err(|_| format!("`{scale}` is not a number")),
        None => Ok((text.trim(), 1.0)),
    }
}

impl Binding {
    /// This binding as text (the module's table).
    pub fn to_text(&self) -> String {
        match *self {
            Binding::Button { source, modifiers } => {
                let mut out = String::new();
                for k in modifiers.keys() {
                    out.push_str(k.name());
                    out.push('+');
                }
                out.push_str(&source.to_text());
                out
            }
            Binding::Buttons1d { negative, positive } => {
                format!("{} / {}", negative.to_text(), positive.to_text())
            }
            Binding::Axis { axis, scale } => {
                scaled(format!("Axis.{}", name_of(&AXES, axis)), scale)
            }
            Binding::Buttons2d {
                up,
                down,
                left,
                right,
            } => format!(
                "{}, {}, {}, {}",
                up.to_text(),
                down.to_text(),
                left.to_text(),
                right.to_text()
            ),
            Binding::Stick { stick, scale } => {
                let name = match stick {
                    Stick::Left => "Stick.Left",
                    Stick::Right => "Stick.Right",
                };
                scaled(name.to_owned(), scale)
            }
            Binding::MouseMotion { scale } => scaled(String::from("MouseMotion"), scale),
        }
    }

    /// A binding read from its text ([`Self::to_text`]'s).
    ///
    /// # Errors
    ///
    /// Text that names no input, with what it could not read.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let source = |s: &str| {
            ButtonSource::parse(s).ok_or_else(|| format!("`{}` names no input", s.trim()))
        };
        if text.contains(',') {
            let parts: Vec<&str> = text.split(',').collect();
            let [up, down, left, right] = parts.as_slice() else {
                return Err(format!("`{text}`: four inputs (up, down, left, right)"));
            };
            return Ok(Binding::Buttons2d {
                up: source(up)?,
                down: source(down)?,
                left: source(left)?,
                right: source(right)?,
            });
        }
        if let Some((negative, positive)) = text.split_once(" / ") {
            return Ok(Binding::Buttons1d {
                negative: source(negative)?,
                positive: source(positive)?,
            });
        }
        let (value, scale) = split_scale(text)?;
        if let Some(name) = value.strip_prefix("Axis.") {
            let axis = value_of(&AXES, name).ok_or_else(|| format!("`{name}` names no axis"))?;
            return Ok(Binding::Axis { axis, scale });
        }
        match value {
            "Stick.Left" => {
                return Ok(Binding::Stick {
                    stick: Stick::Left,
                    scale,
                });
            }
            "Stick.Right" => {
                return Ok(Binding::Stick {
                    stick: Stick::Right,
                    scale,
                });
            }
            "MouseMotion" => return Ok(Binding::MouseMotion { scale }),
            _ => {}
        }
        // A button with its modifier keys before it: the last `+`-separated part that is
        // not a pad axis's own sign.
        let (mods, last) = match text.strip_suffix('+') {
            Some(axis) => match axis.rsplit_once('+') {
                Some((mods, last)) => (Some(mods), format!("{last}+")),
                None => (None, text.to_owned()),
            },
            None => match text.rsplit_once('+') {
                Some((mods, last)) => (Some(mods), last.to_owned()),
                None => (None, text.to_owned()),
            },
        };
        let mut keys = Vec::new();
        for m in mods.into_iter().flat_map(|m| m.split('+')) {
            keys.push(Key::from_name(m.trim()).ok_or_else(|| format!("`{m}` is not a key"))?);
        }
        if keys.len() > 3 {
            return Err(format!("`{text}`: at most three modifier keys"));
        }
        Ok(Binding::Button {
            source: source(&last)?,
            modifiers: Modifiers::of(&keys),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of binding written and read back the same.
    #[test]
    fn bindings_round_trip_through_their_text() {
        let all = [
            Binding::button(Key::KeyW),
            Binding::chord(&[Key::ControlLeft, Key::ShiftLeft], Key::KeyS),
            Binding::button(MouseButton::Left),
            Binding::button(GamepadButton::South),
            Binding::button(ButtonSource::AxisPositive(GamepadAxis::RightTrigger)),
            Binding::button(ButtonSource::AxisNegative(GamepadAxis::LeftX)),
            Binding::chord(
                &[Key::ShiftLeft],
                ButtonSource::AxisPositive(GamepadAxis::LeftY),
            ),
            Binding::button(ButtonSource::WheelUp),
            Binding::Buttons1d {
                negative: Key::KeyA.into(),
                positive: Key::KeyD.into(),
            },
            Binding::Axis {
                axis: GamepadAxis::LeftY,
                scale: -1.0,
            },
            Binding::wasd(),
            Binding::dpad(),
            Binding::stick(Stick::Left),
            Binding::Stick {
                stick: Stick::Right,
                scale: 0.5,
            },
            Binding::mouse_motion(0.1),
        ];
        for b in all {
            let text = b.to_text();
            assert_eq!(Binding::parse(&text), Ok(b), "{text}");
        }
        assert_eq!(Binding::wasd().to_text(), "KeyW, KeyS, KeyA, KeyD");
        assert_eq!(
            Binding::chord(&[Key::ControlLeft], Key::KeyS).to_text(),
            "ControlLeft+KeyS"
        );
    }

    /// Text naming no input says what it could not read.
    #[test]
    fn bad_text_is_an_error() {
        assert!(Binding::parse("KeyQQ").unwrap_err().contains("KeyQQ"));
        assert!(Binding::parse("KeyW, KeyS").is_err());
        assert!(Binding::parse("Stick.Left*x").is_err());
        assert!(Binding::parse("Pad.Nope").is_err());
    }
}
