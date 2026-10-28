//! The profiler's first panel (ADR 0107 E6-5; CLAUDE.md §14): the last frames' CPU and GPU
//! times, the latest frame's CPU stages (`engine_app::FrameReport`), its GPU passes (the
//! frame graph's timestamps, `RenderGraph::gpu_pass_times`) and the game's scripts (ADR
//! 0114 §11: each component's and system's hooks, their time and memory).

use engine_app::FrameReport;
use engine_app::scripts::{ScriptProfile, Scripts};
use engine_render::graph::PassTime;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The frames kept for the graph.
pub(crate) const FRAMES: usize = 120;
/// The stages and passes listed, the longest first (what the panel's height holds).
pub(crate) const LISTED: usize = 9;
/// How often the scripts' costs are taken: a script's are averaged over that time.
const SCRIPTS_EVERY: Duration = Duration::from_millis(500);

/// The panel's text, made with the frame while it is open: the header, the bars (each
/// frame's CPU and GPU shares of the graph's height, and whether it was over a 60 Hz
/// frame), the stages' and the passes' rows.
#[derive(Default)]
pub(crate) struct ProfilerText {
    pub(crate) header: String,
    pub(crate) bars: Vec<(f32, f32, bool)>,
    pub(crate) stages: Vec<(&'static str, String)>,
    pub(crate) passes: Vec<(&'static str, String)>,
    /// The scripts' rows: the component or system, and its costs a frame.
    pub(crate) scripts: Vec<(String, String)>,
}

/// The frames as the panel shows them.
#[derive(Default)]
pub(crate) struct Profile {
    /// The last frames' CPU milliseconds and the GPU's (once measured), oldest first.
    pub(crate) frames: VecDeque<(f32, Option<f32>)>,
    /// The latest frame's CPU stages: name and microseconds, the longest first.
    pub(crate) stages: Vec<(&'static str, f64)>,
    /// The latest measured GPU frame's passes: name, microseconds and how many of that
    /// name (a bloom's levels are one pass each), the longest first.
    pub(crate) passes: Vec<(&'static str, f32, u32)>,
    /// The scripts' costs taken last, the longest first, and the frames they were over
    /// (0: none yet).
    pub(crate) scripts: Vec<ScriptProfile>,
    pub(crate) script_frames: u32,
    /// The frames watched since the scripts' costs were taken, and when they were (`None`
    /// while they are not watched).
    counting: u32,
    taken: Option<Instant>,
}

impl Profile {
    /// Frame `report`, with the GPU's last measured frame: its passes and its time from
    /// the first pass to the last (`None` without timestamps).
    pub(crate) fn note(&mut self, report: &FrameReport, passes: &[PassTime], gpu_us: Option<f32>) {
        self.frames
            .push_back(((report.cpu / 1e3) as f32, gpu_us.map(|us| us / 1e3)));
        if self.frames.len() > FRAMES {
            self.frames.pop_front();
        }
        self.stages.clear();
        self.stages.extend([
            ("wait for the GPU", report.wait),
            ("acquire", report.acquire),
            ("simulate", report.simulate),
            ("propagate", report.propagate),
            ("extract", report.extract),
            ("present hook", report.interpolate),
            ("animate", report.animate),
            ("probes", report.probes),
            ("sound", report.sound),
            ("streaming", report.streaming),
            ("pools", report.pools),
            ("upload", report.upload),
            ("compose", report.compose),
            ("record", report.execute),
            ("submit", report.submit),
            ("present", report.present),
        ]);
        self.stages
            .sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal));
        self.stages.truncate(LISTED);
        // The GPU's are read when their frame slot comes around: kept until new ones come.
        if !passes.is_empty() {
            self.passes.clear();
            for pass in passes {
                match self
                    .passes
                    .iter_mut()
                    .find(|(name, _, _)| *name == pass.name)
                {
                    Some(same) => {
                        same.1 += pass.gpu_us;
                        same.2 += 1;
                    }
                    None => self.passes.push((pass.name, pass.gpu_us, 1)),
                }
            }
            self.passes.sort_unstable_by(|a, b| {
                b.1.partial_cmp(&a.1).unwrap_or(core::cmp::Ordering::Equal)
            });
            self.passes.truncate(LISTED);
        }
    }

    /// The game's scripts after a frame, timed while `watched` (the panel open while the
    /// game plays; ADR 0114 §11): their costs taken every half second, over the frames
    /// since.
    pub(crate) fn watch_scripts(
        &mut self,
        mut scripts: Option<&mut Scripts>,
        watched: bool,
        now: Instant,
    ) {
        if let Some(scripts) = scripts.as_deref_mut() {
            scripts.set_profiling(watched);
        }
        let (Some(scripts), true) = (scripts, watched) else {
            self.scripts.clear();
            self.taken = None;
            return;
        };
        self.counting += 1;
        match self.taken {
            Some(t) if now.duration_since(t) < SCRIPTS_EVERY => {}
            taken => {
                scripts.take_profile(&mut self.scripts);
                self.scripts.sort_by(|a, b| b.nanos.cmp(&a.nanos));
                // The first frames watched were not timed.
                let frames = core::mem::take(&mut self.counting);
                self.script_frames = if taken.is_some() { frames } else { 0 };
                self.taken = Some(now);
            }
        }
    }

    /// The latest frame's CPU and GPU milliseconds.
    pub(crate) fn latest(&self) -> Option<(f32, Option<f32>)> {
        self.frames.back().copied()
    }

    /// The longest CPU or GPU time among the frames kept, at least 16.7 ms (a 60 Hz
    /// frame): what the graph's bars are measured against.
    pub(crate) fn scale(&self) -> f32 {
        self.frames
            .iter()
            .map(|&(cpu, gpu)| cpu.max(gpu.unwrap_or(0.0)))
            .fold(1000.0 / 60.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_frames_are_kept_and_their_costs_listed_longest_first() {
        let mut profile = Profile::default();
        let pass = |name: &'static str, gpu_us: f32| PassTime {
            name,
            scope: None,
            queue: engine_render::graph::QueueKind::Graphics,
            gpu_us,
            span_us: gpu_us,
        };
        for i in 0..(FRAMES + 5) {
            let report = FrameReport {
                cpu: 2000.0 + i as f64,
                compose: 300.0,
                execute: 900.0,
                simulate: 50.0,
                ..FrameReport::default()
            };
            let passes = if i % 2 == 0 {
                vec![
                    pass("bloom", 200.0),
                    pass("forward", 4000.0),
                    pass("shadow", 1500.0),
                    pass("bloom", 100.0),
                ]
            } else {
                Vec::new()
            };
            profile.note(&report, &passes, Some(6000.0));
        }
        assert_eq!(profile.frames.len(), FRAMES);
        let (cpu, gpu) = profile.latest().unwrap();
        assert!((cpu - 2.124).abs() < 1e-3, "{cpu}");
        assert_eq!(gpu, Some(6.0));
        assert_eq!(profile.stages[0], ("record", 900.0));
        assert_eq!(profile.stages[1], ("compose", 300.0));
        assert_eq!(profile.stages.len(), LISTED);
        // A frame without new GPU times keeps the last ones.
        assert_eq!(
            profile.passes,
            [
                ("forward", 4000.0, 1),
                ("shadow", 1500.0, 1),
                ("bloom", 300.0, 2)
            ],
            "passes of one name summed"
        );
        assert!(
            (profile.scale() - 1000.0 / 60.0).abs() < 1e-3,
            "a 60 Hz frame at least"
        );
    }
}
