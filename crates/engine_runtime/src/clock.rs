//! The fixed-step simulation clock.

/// How many ticks a frame runs, and where between the last two it renders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Steps {
    /// Ticks to run now.
    pub count: u32,
    /// Interpolation factor for rendering, in [0, 1): how far real time has gone from
    /// the last tick towards the next. Rendering shows `lerp(previous, current, alpha)`.
    pub alpha: f32,
    /// Seconds dropped because more than the maximum number of ticks were due (the game
    /// slows down instead of falling ever further behind).
    pub dropped: f64,
}

/// Fixed-step accumulator (ADR 0019): the simulation advances in ticks of `step`
/// seconds whatever the frame rate.
#[derive(Debug, Clone)]
pub struct FixedStep {
    step: f64,
    accumulator: f64,
    max_steps: u32,
    ticks: u64,
}

impl FixedStep {
    /// A clock of `hz` ticks per second (64 for ADR 0019's default), running at most
    /// `max_steps` ticks per frame.
    pub fn new(hz: f64, max_steps: u32) -> Self {
        Self {
            step: 1.0 / hz.max(1e-3),
            accumulator: 0.0,
            max_steps: max_steps.max(1),
            ticks: 0,
        }
    }

    /// Seconds per tick.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// Ticks run so far.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Adds `elapsed` seconds of real time and returns the ticks now due.
    pub fn advance(&mut self, elapsed: f64) -> Steps {
        self.accumulator += elapsed.max(0.0);
        let due = (self.accumulator / self.step).floor();
        let count = (due as u64).min(u64::from(self.max_steps)) as u32;
        self.accumulator -= f64::from(count) * self.step;
        let mut dropped = 0.0;
        if self.accumulator >= self.step {
            // Behind by more than the frame may catch up: forget the debt.
            dropped = self.accumulator;
            self.accumulator = 0.0;
        }
        self.ticks += u64::from(count);
        Steps {
            count,
            // Below 1 even where rounding lands the remainder on a whole step.
            alpha: ((self.accumulator / self.step) as f32).min(1.0 - f32::EPSILON),
            dropped,
        }
    }

    /// Forgets the time accumulated towards the next tick (after a load or a pause).
    pub fn reset(&mut self) {
        self.accumulator = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_follow_real_time_and_alpha_the_remainder() {
        let mut clock = FixedStep::new(64.0, 4);
        let frame = 1.0 / 240.0;
        let mut ticks = 0;
        for _ in 0..240 {
            let s = clock.advance(frame);
            ticks += s.count;
            assert!((0.0..1.0).contains(&s.alpha), "{s:?}");
            assert_eq!(s.dropped, 0.0);
        }
        // One second: 64 ticks (give or take rounding at the boundary).
        assert!((63..=64).contains(&ticks), "{ticks}");
        assert_eq!(clock.ticks(), u64::from(ticks));
    }

    #[test]
    fn a_long_frame_runs_at_most_the_maximum_and_drops_the_rest() {
        let mut clock = FixedStep::new(60.0, 3);
        let s = clock.advance(1.0);
        assert_eq!(s.count, 3);
        assert!(s.dropped > 0.9 && s.dropped <= 0.95 + 1e-9, "{s:?}");
        assert!(s.alpha < 1.0);
        let s = clock.advance(0.0);
        assert_eq!(s.count, 0);
    }
}
