//! Snapping of dragged values (the owner's rules, 2026-10-17), shared by the inspector's
//! numbers now and the viewport's gizmos later (ADR 0107 phase E2):
//!
//! - **Free by default.** The default can be switched to snapping (the tool bar's grid
//!   button, `Y`).
//! - **Shift while dragging** takes the other mode for as long as it is held: snapping
//!   moves the value in whole steps from where the drag started (0.37 → 0.47, 0.57).
//! - **Ctrl while dragging** rounds the value itself to the step (0.37 → 0.4, 0.5): the
//!   object's center lands on the grid.
//! - **Steps:** position 0.1, scale 0.1, rotation 45°, other numbers 0.1 (or their
//!   field's own `step`), whole numbers 1.
//!
//! ```
//! use engine_editor::{Quantity, Snapping};
//!
//! let snap = Snapping::default();
//! // 37 pixels to the right of a position of 0.37: free, then Shift, then Ctrl.
//! let free = snap.drag(Quantity::Position, 0.37, 37.0, false, false);
//! assert!((free - 0.74).abs() < 1e-9);
//! let stepped = snap.drag(Quantity::Position, 0.37, 37.0, true, false);
//! assert!((stepped - 0.77).abs() < 1e-9);
//! let rounded = snap.drag(Quantity::Position, 0.37, 37.0, false, true);
//! assert!((rounded - 0.7).abs() < 1e-9);
//! ```

/// What a dragged number is: its speed and its step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quantity {
    /// A position (`translation`).
    Position,
    /// A scale.
    Scale,
    /// An angle in degrees.
    Angle,
    /// Any other number; `step` is its field's own (`#[reflect(step = …)]`).
    Number {
        /// The field's step, if it has one.
        step: Option<f64>,
    },
    /// A whole number.
    Integer,
}

/// The snapping settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapping {
    /// Snap by default (Shift then drags freely).
    pub on: bool,
    /// Step of positions.
    pub position: f64,
    /// Step of scales.
    pub scale: f64,
    /// Step of angles, in degrees.
    pub angle: f64,
    /// Step of other numbers without a step of their own.
    pub number: f64,
}

impl Default for Snapping {
    fn default() -> Self {
        Self {
            on: false,
            position: 0.1,
            scale: 0.1,
            angle: 45.0,
            number: 0.1,
        }
    }
}

impl Snapping {
    /// The step of a quantity.
    pub fn step(&self, quantity: Quantity) -> f64 {
        match quantity {
            Quantity::Position => self.position,
            Quantity::Scale => self.scale,
            Quantity::Angle => self.angle,
            Quantity::Number { step } => step.filter(|s| *s > 0.0).unwrap_or(self.number),
            Quantity::Integer => 1.0,
        }
    }

    /// The value a drag gives: `start` moved by the pointer's `travel` in logical pixels
    /// since the press, with Shift (`invert`) and Ctrl (`round`) as held this frame.
    pub fn drag(
        &self,
        quantity: Quantity,
        start: f64,
        travel: f32,
        invert: bool,
        round: bool,
    ) -> f64 {
        let free = start + f64::from(travel) * speed(quantity, start);
        let step = self.step(quantity);
        let value = if round {
            (free / step).round() * step
        } else if self.on != invert {
            start + ((free - start) / step).round() * step
        } else {
            free
        };
        let value = if quantity == Quantity::Integer {
            value.round()
        } else {
            value
        };
        // Rounding leaves -0 and float dust (0.30000000000000004): keep them out of files.
        clean(value)
    }
}

/// Change per logical pixel of travel. Positions and scales move 1 per 100 pixels (a
/// step of 0.1 every 10), angles 1° per 2 pixels (45° per 90); other numbers faster the
/// larger they are (Unity's rule: √|v| × 0.03, at least 0.03).
fn speed(quantity: Quantity, start: f64) -> f64 {
    match quantity {
        Quantity::Position | Quantity::Scale => 0.01,
        Quantity::Angle => 0.5,
        Quantity::Number { .. } => start.abs().sqrt().max(1.0) * 0.03,
        Quantity::Integer => start.abs().sqrt().max(1.0) * 0.1,
    }
}

/// `value` without float dust from stepping: rounded to 9 significant decimals.
fn clean(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return if value.is_finite() { 0.0 } else { value };
    }
    let scale = 10f64.powi(9 - value.abs().log10().ceil() as i32);
    let rounded = (value * scale).round() / scale;
    if rounded == 0.0 { 0.0 } else { rounded }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_inverts_the_default_and_ctrl_rounds_to_the_grid() {
        let mut snap = Snapping::default();
        let drag = |s: &Snapping, start, travel, shift, ctrl| {
            s.drag(Quantity::Position, start, travel, shift, ctrl)
        };
        assert_eq!(drag(&snap, 0.37, 14.0, false, false), 0.51);
        assert_eq!(
            drag(&snap, 0.37, 14.0, true, false),
            0.47,
            "steps from the start"
        );
        assert_eq!(
            drag(&snap, 0.37, 14.0, false, true),
            0.5,
            "rounded to the grid"
        );
        assert_eq!(drag(&snap, 0.37, 14.0, true, true), 0.5, "Ctrl wins");
        snap.on = true;
        assert_eq!(drag(&snap, 0.37, 14.0, false, false), 0.47);
        assert_eq!(
            drag(&snap, 0.37, 14.0, true, false),
            0.51,
            "Shift drags freely"
        );
        assert_eq!(drag(&snap, -0.37, -14.0, false, true), -0.5);
    }

    #[test]
    fn angles_scales_numbers_and_integers_have_their_steps() {
        let snap = Snapping::default();
        assert_eq!(snap.drag(Quantity::Angle, 10.0, 50.0, true, false), 55.0);
        assert_eq!(snap.drag(Quantity::Angle, 10.0, 50.0, false, true), 45.0);
        assert_eq!(snap.drag(Quantity::Angle, 10.0, 50.0, false, false), 35.0);
        assert_eq!(snap.drag(Quantity::Scale, 1.0, 26.0, false, true), 1.3);
        let own = Quantity::Number { step: Some(0.25) };
        assert_eq!(snap.drag(own, 1.0, 10.0, false, true), 1.25);
        assert_eq!(snap.drag(Quantity::Integer, 3.0, 10.0, false, false), 5.0);
        assert_eq!(snap.drag(Quantity::Integer, 3.0, 0.0, false, false), 3.0);
        // No float dust and no -0 in what files get.
        assert_eq!(snap.drag(Quantity::Position, 0.1, 20.0, true, false), 0.3);
        assert_eq!(
            snap.drag(Quantity::Position, 0.1, -10.0, false, true)
                .to_bits(),
            0f64.to_bits()
        );
    }
}
