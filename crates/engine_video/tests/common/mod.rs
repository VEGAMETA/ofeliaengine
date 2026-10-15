//! What the GPU tests share: the device, a frame graph that runs conversions and videos
//! and reads the texture back, and the conversion's CPU reference.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks
)]

use engine_render::graph::{FrameGraph, RenderGraph};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, ExtraQueues, Instance,
    InstanceDesc, MemoryLocation,
};
use engine_video::convert::{
    Conversion, Converter, PlaneSource, Planes, VideoTexture, wait_for_decode,
};
use engine_video::player::VideoPlayer;

pub(crate) fn gpu(video_decode: bool) -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_video conversion tests"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
        })?;
        let adapter = instance.select_adapter(
            &AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance),
        )?;
        let device = Device::with_extras(
            &instance,
            &adapter,
            &[],
            ExtraQueues {
                video_decode,
                ..ExtraQueues::default()
            },
        )?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

const READBACK_BYTES: u64 = 4 << 20;

/// Runs conversions through a frame graph and reads the texture back.
pub(crate) struct Bed {
    pub(crate) device: Device,
    pub(crate) allocator: Allocator,
    pub(crate) bindless: Bindless,
    pub(crate) graph: RenderGraph,
    pub(crate) converter: Converter,
    pub(crate) pool: vk::CommandPool,
    pub(crate) cmd: vk::CommandBuffer,
    pub(crate) readback: vk::Buffer,
    pub(crate) readback_memory: Option<Allocation>,
}

impl Bed {
    pub(crate) fn new(device: Device) -> Self {
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let converter = Converter::new(&device, &bindless).unwrap();
        let d = device.raw();
        let pool = unsafe {
            d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
        }
        .unwrap();
        let cmd = unsafe {
            d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
        }
        .unwrap()[0];
        let (readback, memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(READBACK_BYTES)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        Self {
            device,
            allocator,
            bindless,
            graph,
            converter,
            pool,
            cmd,
            readback,
            readback_memory: Some(memory),
        }
    }

    /// Converts `source` into `texture` (after `semaphore` reaches its value, for a GPU
    /// decoder's picture) and returns the texture's RGBA bytes.
    pub(crate) fn convert(
        &mut self,
        texture: &mut VideoTexture,
        source: &PlaneSource,
        conversion: &Conversion,
        wait: Option<(vk::Semaphore, u64)>,
    ) -> Vec<[u8; 4]> {
        let Self {
            device,
            allocator,
            bindless,
            graph,
            converter,
            cmd,
            readback,
            readback_memory,
            ..
        } = self;
        let cmd = *cmd;
        if let Some((semaphore, value)) = wait {
            wait_for_decode(device, semaphore, value).unwrap();
        }
        begin(device, cmd);
        {
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            converter.add_pass(&mut fg, bindless, texture, source, conversion);
            add_readback(&mut fg, texture.image(), texture.size(), *readback);
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
        }
        submit(device, cmd);
        let [w, h] = texture.size();
        texels(readback_memory, w * h)
    }

    /// Runs a frame of `video` (number `frame`): its conversion when a picture is due,
    /// and the texture read back when `read` and written.
    pub(crate) fn show(
        &mut self,
        video: &mut VideoPlayer,
        frame: u64,
        read: bool,
    ) -> Option<Vec<[u8; 4]>> {
        let Self {
            device,
            allocator,
            bindless,
            graph,
            converter,
            cmd,
            readback,
            readback_memory,
            ..
        } = self;
        let cmd = *cmd;
        begin(device, cmd);
        let mut size = None;
        {
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            video
                .add_pass(&mut fg, converter, bindless, device, frame, 0)
                .unwrap();
            if read && video.texture().is_written() {
                let texture = video.texture();
                add_readback(&mut fg, texture.image(), texture.size(), *readback);
                size = Some(texture.size());
            }
            fg.execute_with_bindless(allocator, Some(bindless)).unwrap();
        }
        submit(device, cmd);
        size.map(|[w, h]| texels(readback_memory, w * h))
    }

    /// Frees everything; returns the instance's validation errors.
    pub(crate) fn finish(mut self, instance: &Instance) -> u32 {
        unsafe {
            self.device.wait_idle().unwrap();
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.allocator
                .destroy_buffer(self.readback, self.readback_memory.take().unwrap());
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        drop(self.converter);
        drop(self.bindless);
        drop(self.allocator);
        drop(self.device);
        let report = instance.validation_report();
        println!(
            "validation: enabled={} errors={} warnings={}",
            report.enabled, report.errors, report.warnings
        );
        if report.errors > 0 {
            println!("{:#?}", report.first_errors);
        }
        report.errors
    }
}

/// Begins recording `cmd`.
fn begin(device: &Device, cmd: vk::CommandBuffer) {
    let d = device.raw();
    unsafe {
        d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
            .unwrap();
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
    }
}

/// Ends `cmd`, submits it to the graphics queue and waits for the device.
fn submit(device: &Device, cmd: vk::CommandBuffer) {
    let d = device.raw();
    unsafe {
        d.end_command_buffer(cmd).unwrap();
        let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        d.queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
            vk::Fence::null(),
        )
        .unwrap();
        device.wait_idle().unwrap();
    }
}

/// A pass copying the texture `image` (in `READ_ONLY_OPTIMAL`, put back after) into
/// `buffer` for the host.
fn add_readback(fg: &mut FrameGraph<'_>, image: vk::Image, [w, h]: [u32; 2], buffer: vk::Buffer) {
    assert!(u64::from(w * h * 4) <= READBACK_BYTES);
    fg.add_pass("read back")
        .side_effects()
        .execute(move |ctx| unsafe {
            let range = vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            };
            let to_copy = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .image(image)
                .subresource_range(range);
            ctx.device.cmd_pipeline_barrier2(
                ctx.cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[to_copy]),
            );
            ctx.device.cmd_copy_image_to_buffer(
                ctx.cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: w,
                        height: h,
                        depth: 1,
                    },
                    ..Default::default()
                }],
            );
            let back = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::READ_ONLY_OPTIMAL)
                .image(image)
                .subresource_range(range);
            let host = vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ);
            ctx.device.cmd_pipeline_barrier2(
                ctx.cmd,
                &vk::DependencyInfo::default()
                    .memory_barriers(&[host])
                    .image_memory_barriers(&[back]),
            );
        });
}

/// The first `count` RGBA texels read back.
fn texels(memory: &mut Option<Allocation>, count: u32) -> Vec<[u8; 4]> {
    let mapped = memory.as_mut().unwrap().mapped_slice_mut().unwrap();
    mapped[..count as usize * 4]
        .chunks_exact(4)
        .map(|p| [p[0], p[1], p[2], p[3]])
        .collect()
}

/// A picture's code values: luma, and Cb, Cr pairs (`None`: luma only).
pub(crate) struct Codes {
    pub(crate) size: [u32; 2],
    pub(crate) luma: Vec<u16>,
    pub(crate) chroma: Option<(Vec<[u16; 2]>, [u32; 2])>,
}

/// The pass's arithmetic on the CPU: the texture `target` converts `codes` into.
pub(crate) fn reference(codes: &Codes, conversion: &Conversion, target: [u32; 2]) -> Vec<[u8; 4]> {
    let [w, h] = codes.size;
    let luma = |x: i32, y: i32| {
        let (x, y) = (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1));
        let v = f32::from(codes.luma[(y as u32 * w + x as u32) as usize]);
        [v, v]
    };
    let (chroma, chroma_size) = match &codes.chroma {
        Some((samples, size)) => (samples.clone(), *size),
        None => (vec![[128, 128]], [1, 1]),
    };
    let chroma = |x: i32, y: i32| {
        let (cw, ch) = (chroma_size[0] as i32, chroma_size[1] as i32);
        let s = chroma[(y.clamp(0, ch - 1) * cw + x.clamp(0, cw - 1)) as usize];
        [f32::from(s[0]), f32::from(s[1])]
    };
    let lerp =
        |a: [f32; 2], b: [f32; 2], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let bilinear = |plane: &dyn Fn(i32, i32) -> [f32; 2], x: [f32; 2]| {
        let corner = [x[0].floor(), x[1].floor()];
        let f = [x[0] - corner[0], x[1] - corner[1]];
        let (px, py) = (corner[0] as i32, corner[1] as i32);
        let top = lerp(plane(px, py), plane(px + 1, py), f[0]);
        let bottom = lerp(plane(px, py + 1), plane(px + 1, py + 1), f[0]);
        lerp(top, bottom, f[1])
    };
    let step = [w as f32 / target[0] as f32, h as f32 / target[1] as f32];
    let mut out = Vec::with_capacity((target[0] * target[1]) as usize);
    for ty in 0..target[1] {
        for tx in 0..target[0] {
            let x = [
                (tx as f32 + 0.5) * step[0] - 0.5,
                (ty as f32 + 0.5) * step[1] - 0.5,
            ];
            let y = if step == [1.0, 1.0] {
                luma(tx as i32, ty as i32)[0]
            } else {
                bilinear(&luma, x)[0]
            };
            let c = bilinear(
                &chroma,
                [
                    x[0] * conversion.chroma_step[0] + conversion.chroma_bias[0],
                    x[1] * conversion.chroma_step[1] + conversion.chroma_bias[1],
                ],
            );
            let [r, g, b] = conversion.rgb([y, c[0], c[1]]);
            out.push([r, g, b, 255]);
        }
    }
    out
}

/// How the GPU's texels differ from the reference: `Err` past one in any channel.
pub(crate) fn compare(ours: &[[u8; 4]], theirs: &[[u8; 4]], width: u32) -> Result<usize, String> {
    assert_eq!(ours.len(), theirs.len());
    let mut off_by_one = 0;
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        let largest = a
            .iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        if largest > 1 {
            let (x, y) = (i as u32 % width, i as u32 / width);
            return Err(format!("texel {x},{y}: GPU {a:?}, CPU {b:?}"));
        }
        off_by_one += usize::from(largest == 1);
    }
    Ok(off_by_one)
}

/// The code values of a picture's planes in memory.
pub(crate) fn codes_of(planes: &Planes<'_>) -> Codes {
    let sample = |(rows, stride): (&[u8], usize), [w, h]: [u32; 2]| -> Vec<u16> {
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                out.push(if planes.bit_depth > 8 {
                    let at = y * stride + 2 * x;
                    u16::from_le_bytes([rows[at], rows[at + 1]])
                } else {
                    u16::from(rows[y * stride + x])
                });
            }
        }
        out
    };
    Codes {
        size: planes.size,
        luma: sample(planes.luma, planes.size),
        chroma: planes.chroma.map(|([cb, cr], size)| {
            let (cb, cr) = (sample(cb, size), sample(cr, size));
            (cb.into_iter().zip(cr).map(|(b, r)| [b, r]).collect(), size)
        }),
    }
}
