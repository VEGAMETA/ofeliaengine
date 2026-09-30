//! Depth pyramids (ADR 0035): level `k` holds the farthest depth of every `2^k × 2^k`
//! pixel block of a depth target, all levels one after the other in one buffer. The
//! occlusion test of GPU culling reads it; screen-space effects can too.
//!
//! `hizReduce` (`shaders/hiz.slang`) writes up to six levels per dispatch: every
//! workgroup reduces a 64 × 64 tile of the level below through group shared memory. The
//! first dispatch reads the depth target and writes only levels 1 and 2: it is bound by
//! memory bandwidth, and on a small GPU a longer tail of barriers inside its many
//! workgroups costs more than it saves. The next dispatches work on small data and write
//! six levels each. A 1080p pyramid takes three dispatches instead of eleven.

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{Access, Buffer, FrameGraph, ShaderStages, Texture};
use engine_rhi::ash::vk;
use engine_rhi::{Bindless, Device, Pipeline, RhiError};

const REDUCE: &[u8] = include_bytes!("../shaders/hiz.hizReduce.spv");

/// Levels a layout has room for: targets up to 65536 pixels on a side.
pub const MAX_LEVELS: usize = 16;
/// Levels the first dispatch writes (from the depth target).
const FIRST_DISPATCH_LEVELS: u32 = 2;
/// Levels every later dispatch writes (the most `hizReduce` can).
const LEVELS_PER_DISPATCH: u32 = 6;
/// Input texels per workgroup side of `hizReduce`.
const TILE: u32 = 64;

/// Size of level `level` of a `width` × `height` target; level 0 is the target itself.
pub fn level_size(width: u32, height: u32, level: u32) -> (u32, u32) {
    if level == 0 {
        (width, height)
    } else {
        (((width - 1) >> level) + 1, ((height - 1) >> level) + 1)
    }
}

/// Where each level of a pyramid lives in its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyramidLayout {
    /// Levels, from 1 up to the 1 × 1 level (0 for an empty target).
    pub levels: u32,
    /// The finest stored level: 2, or 1 when level 1 is the whole pyramid. Level 1 is
    /// computed but not stored otherwise (a fifth of the memory traffic at 1080p).
    pub finest: u32,
    /// Offset of level `k + 1`, in floats (0 for levels that are not stored).
    pub offsets: [u32; MAX_LEVELS],
    /// Floats in the stored levels.
    pub floats: u32,
}

impl PyramidLayout {
    /// The layout for a `width` × `height` depth target: level `k` is `⌈width / 2^k⌉ ×
    /// ⌈height / 2^k⌉`, one texel per `2^k × 2^k` pixels.
    pub fn new(width: u32, height: u32) -> Self {
        let mut layout = Self {
            levels: 0,
            finest: 1,
            offsets: [0; MAX_LEVELS],
            floats: 0,
        };
        if width == 0 || height == 0 {
            return layout;
        }
        for level in 1..=MAX_LEVELS as u32 {
            layout.levels = level;
            let (w, h) = level_size(width, height, level);
            if w == 1 && h == 1 {
                break;
            }
        }
        layout.finest = layout.levels.min(2);
        for level in layout.finest..=layout.levels {
            let (w, h) = level_size(width, height, level);
            layout.offsets[level as usize - 1] = layout.floats;
            layout.floats += w * h;
        }
        layout
    }

    /// Bytes of the pyramid buffer.
    pub fn bytes(&self) -> u64 {
        u64::from(self.floats) * 4
    }

    /// Offset of `level` in floats; 0 past the last level (never written).
    fn offset(&self, level: u32) -> u32 {
        self.offsets
            .get((level as usize).wrapping_sub(1))
            .copied()
            .unwrap_or(0)
    }
}

/// `HizPush` in `hiz.slang` (64 bytes).
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct Push {
    pyramid: u64,
    depth_size: [u32; 2],
    texture: u32,
    levels: u32,
    first: u32,
    input_offset: u32,
    out_a: [u32; 4],
    out_b: [u32; 2],
    count: u32,
    _pad: u32,
}

impl Push {
    /// Points the push constants at `count` levels from `first`, and the level below.
    /// The shader indexes these offsets with constants only: indexing a push constant
    /// array with a run-time value makes the Radeon's compiler copy the array per thread
    /// (the fused reduction ran 280-320 µs instead of 190-220).
    fn levels_from(&mut self, layout: &PyramidLayout, first: u32, count: u32) {
        self.first = first;
        self.count = count;
        self.input_offset = if first >= 2 {
            layout.offset(first - 1)
        } else {
            0
        };
        self.out_a = [
            layout.offset(first),
            layout.offset(first + 1),
            layout.offset(first + 2),
            layout.offset(first + 3),
        ];
        self.out_b = [layout.offset(first + 4), layout.offset(first + 5)];
    }
}

/// The dispatches that build a pyramid of `levels` levels: `(first level, level count)`.
fn dispatches(levels: u32) -> impl Iterator<Item = (u32, u32)> {
    let mut first = 1;
    core::iter::from_fn(move || {
        if first > levels {
            return None;
        }
        let count = if first == 1 {
            FIRST_DISPATCH_LEVELS
        } else {
            LEVELS_PER_DISPATCH
        };
        let dispatch = (first, count.min(levels - first + 1));
        first += count;
        Some(dispatch)
    })
}

/// Builds depth pyramids.
#[derive(Debug)]
pub struct DepthPyramid {
    reduce: ComputeShader<Push>,
}

impl DepthPyramid {
    /// Creates the pipeline.
    pub fn new(device: &Device, bindless: &Bindless) -> Result<Self, RhiError> {
        let source = Self::source()?;
        let [reduce] = ComputeSource::create_all(device, bindless, [source])?;
        Ok(Self { reduce })
    }

    /// The reduction shader, to create with other pipelines (ADR 0055).
    pub(crate) fn source() -> Result<ComputeSource<'static, Push>, RhiError> {
        ComputeSource::new(REDUCE, c"hizReduce", &[])
    }

    /// The pyramid, given the pipeline created from [`Self::source`].
    pub(crate) fn from_source(source: &ComputeSource<'_, Push>, pipeline: Pipeline) -> Self {
        Self {
            reduce: source.finish(pipeline),
        }
    }

    /// Adds the pass that reduces `depth` (sampled) into `pyramid`, a buffer of at least
    /// [`PyramidLayout::bytes`] for the depth's size. Without a bindless index for the
    /// depth, the pyramid is filled with the far plane (1.0): it hides nothing.
    pub fn add_pass<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        depth: Texture,
        pyramid: Buffer,
    ) {
        let desc = fg.texture_desc(depth);
        let (width, height) = (desc.width, desc.height);
        let layout = PyramidLayout::new(width, height);
        let reduce = &self.reduce;
        fg.add_compute("hiz", bindless)
            .read(depth, Access::Sampled(ShaderStages::COMPUTE))
            .write(pyramid, Access::StorageReadWrite(ShaderStages::COMPUTE))
            .record(move |rec| {
                let ctx = rec.context();
                let Some(texture) = ctx.sampled_index(depth) else {
                    // SAFETY: recording outside a rendering scope.
                    unsafe {
                        ctx.device.cmd_fill_buffer(
                            ctx.cmd,
                            ctx.buffer(pyramid),
                            0,
                            vk::WHOLE_SIZE,
                            1.0f32.to_bits(),
                        );
                    }
                    return;
                };
                let mut push = Push {
                    pyramid: ctx.address(pyramid),
                    depth_size: [width, height],
                    texture,
                    levels: layout.levels,
                    first: 1,
                    input_offset: 0,
                    out_a: [0; 4],
                    out_b: [0; 2],
                    count: 0,
                    _pad: 0,
                };
                for (first, count) in dispatches(layout.levels) {
                    // Each dispatch reads the level the previous one wrote last.
                    if first > 1 {
                        rec.barrier();
                    }
                    push.levels_from(&layout, first, count);
                    // One workgroup per TILE × TILE of its input level.
                    let (w, h) = level_size(width, height, first - 1);
                    let groups = [w.div_ceil(TILE), h.div_ceil(TILE), 1];
                    rec.dispatch(reduce, Dispatch::Groups(groups), &push);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_halves_to_one_texel() {
        let layout = PyramidLayout::new(1920, 1080);
        assert_eq!(layout.levels, 11, "2^11 = 2048 >= 1920");
        let sizes: Vec<(u32, u32)> = (1..=layout.levels)
            .map(|k| level_size(1920, 1080, k))
            .collect();
        assert_eq!(sizes[0], (960, 540));
        assert_eq!(sizes[1], (480, 270));
        assert_eq!(sizes[4], (60, 34), "1080 / 32 = 33.75 rounds up");
        assert_eq!(sizes[10], (1, 1));
        // Level 1 is not stored: level 2 comes first.
        assert_eq!(layout.finest, 2);
        let mut expected = 0;
        for (k, (w, h)) in sizes.iter().enumerate().skip(1) {
            assert_eq!(layout.offsets[k], expected);
            expected += w * h;
        }
        assert_eq!(layout.floats, expected);
        // A target of 2 × 2 or less: level 1 is the whole pyramid, and stored.
        let tiny = PyramidLayout::new(2, 1);
        assert_eq!((tiny.levels, tiny.finest, tiny.floats), (1, 1, 1));
        assert_eq!(PyramidLayout::new(1, 1).levels, 1);
        assert_eq!(PyramidLayout::new(0, 1080).levels, 0);
        assert_eq!(PyramidLayout::new(65536, 8).levels, 16);
    }

    #[test]
    fn dispatches_cover_every_level_once() {
        assert_eq!(dispatches(11).collect::<Vec<_>>(), [(1, 2), (3, 6), (9, 3)]);
        assert_eq!(dispatches(1).collect::<Vec<_>>(), [(1, 1)]);
        assert_eq!(dispatches(8).collect::<Vec<_>>(), [(1, 2), (3, 6)]);
        assert_eq!(dispatches(0).count(), 0);
        for levels in 1..=16 {
            let covered: u32 = dispatches(levels).map(|(_, n)| n).sum();
            assert_eq!(covered, levels);
        }
    }
}
