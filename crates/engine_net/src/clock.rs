//! Sub-tick timestamps (CS2's idea, ADR 0019): an action inside a game's input carries
//! where within its tick it happened, so lag compensation resolves it at its real
//! instant instead of the tick boundary.

use engine_ecs::bytemuck;

/// Where within a tick an action happened: 0 at the start of the tick's input interval,
/// 255 at its end (when the input was sampled). Put it in the game's input type next to
/// the action (`fire: u8` with 0 for "no shot" works as well).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct SubTick(pub u8);

impl SubTick {
    /// The end of the interval: the moment the input was sampled.
    pub const END: SubTick = SubTick(255);

    /// From a fraction in `0..=1`.
    pub fn from_fraction(f: f32) -> Self {
        Self((f.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    /// The fraction in `0..=1`.
    pub fn fraction(self) -> f32 {
        f32::from(self.0) / 255.0
    }
}
