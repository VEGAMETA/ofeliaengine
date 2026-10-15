//! What the player reports about its start-up and each frame.

use core::fmt;

/// Where the start-up spent its time, phase by phase. Phases on threads of their own
/// overlap the others: the sum is more than the wall time.
#[derive(Debug, Clone, Default)]
pub struct Startup {
    /// Each phase and its milliseconds.
    pub phases: Vec<(&'static str, f64)>,
    /// Pipelines the renderer made at start-up.
    pub pipelines: StartupPipelines,
    /// What loading the pipeline cache found (`None`: no cache kept).
    pub cache: Option<String>,
    /// What could not be done (power throttling, MMCSS, frame generation...), and why.
    pub notes: Vec<String>,
}

/// The pipelines the renderer made at start-up.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartupPipelines {
    /// Made.
    pub created: u64,
    /// Found in the pipeline cache.
    pub from_cache: u64,
    /// Compiled.
    pub compiled: u64,
    /// Milliseconds in the driver, summed over threads.
    pub driver_ms: f64,
}

/// Where one frame's CPU time went, in microseconds, and what else it did.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameReport {
    /// The frame's number (`Frames::begin`).
    pub number: u64,
    /// The latency provider's wait (Reflex, Anti-Lag 2, the limiter).
    pub latency_wait: f64,
    /// `Frames::begin`: waiting for the frame slot (the GPU frames in flight back).
    pub wait: f64,
    /// The swapchain image (or the paced presenter's images).
    pub acquire: f64,
    /// Fixed ticks run.
    pub ticks: u32,
    /// The game's tick hooks.
    pub simulate: f64,
    /// Transform propagation, over the ticks.
    pub propagate: f64,
    /// Extraction into the GPU scene, over the ticks.
    pub extract: f64,
    /// The present hook and the movers' interpolation.
    pub interpolate: f64,
    /// The animate hook.
    pub animate: f64,
    /// The reflection probe entities and the GI volume mirrored into the renderer.
    pub probes: f64,
    /// Streaming: textures and mesh levels requested, arrived, evicted.
    pub streaming: f64,
    /// Texture and mesh levels that arrived this frame.
    pub arrived: u64,
    /// The pools' frame starts: textures, materials, geometry, skinning.
    pub pools: f64,
    /// Of which textures, materials, geometry.
    pub pool_parts: [f64; 3],
    /// The GPU scene's uploads.
    pub upload: f64,
    /// Preparing the view, declaring the frame's passes, the UI.
    pub compose: f64,
    /// Compiling and recording the frame graph.
    pub execute: f64,
    /// Submitting.
    pub submit: f64,
    /// Presenting.
    pub present: f64,
    /// The frame's own CPU work: from the end of the slot wait to the end of presenting.
    pub cpu: f64,
    /// Pipelines created during the frame (first uses).
    pub pipelines: u64,
    /// A generated frame was shown before this one (frame generation).
    pub generated_shown: bool,
}

impl fmt::Display for FrameReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "frame {}: CPU {:.0} µs (ticks {}, simulate {:.0}, streaming {:.0}, compose {:.0}, execute {:.0}, present {:.0})",
            self.number,
            self.cpu,
            self.ticks,
            self.simulate,
            self.streaming,
            self.compose,
            self.execute,
            self.present
        )
    }
}
