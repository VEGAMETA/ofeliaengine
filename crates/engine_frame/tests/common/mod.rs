//! A small GPU test bed for the frame renderer: a device, the pools, one frame at a time
//! with an optional read back of the output.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout
)]

use engine_core::math::{Mat4, Quat, Vec3, look_at};
use engine_frame::{
    FrameContext, FrameRenderer, PrepareContext, Prepared, SceneDraws, SceneInput, SceneTargets,
    View, ViewCamera, ViewSettings, ViewTarget,
};
use engine_render::forward::{DirectionalLight, DrawItem, ShadowSettings};
use engine_render::graph::{Access, BufferState, ImportedBuffer, LoadOp, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocation, Allocator, Bindless, Device, Instance, InstanceDesc,
    MemoryLocation,
};

pub const OUTPUT: vk::Format = vk::Format::R8G8B8A8_UNORM;

pub struct Bed {
    pub frame: FrameRenderer,
    pub ring: UploadRing,
    pub graph: RenderGraph,
    pub geometry: GeometryPool,
    pub textures: TexturePool,
    pub materials: MaterialTable,
    pub cube: MeshId,
    pub size: (u32, u32),
    pub frame_number: u64,
    readback: vk::Buffer,
    readback_memory: Option<Allocation>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    pub bindless: Bindless,
    pub allocator: Allocator,
    pub device: Device,
    pub instance: Instance,
}

impl Bed {
    /// `None` without a GPU (unless `ENGINE_REQUIRE_GPU=1`).
    pub fn new(size: (u32, u32), validation: bool) -> Option<Self> {
        let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
        let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
            let instance = Instance::new(&InstanceDesc {
                app_name: String::from("engine_frame test"),
                validation,
                debug_utils: validation,
                surface: false,
                headless_surface: false,
                sync_validation: validation,
            })?;
            let preference =
                AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
            let adapter = instance.select_adapter(&preference)?;
            let device = Device::new(&instance, &adapter)?;
            Ok((instance, device))
        };
        let (instance, device) = match attempt() {
            Ok(pair) => pair,
            Err(e) if !required => {
                println!("skipping: {e}");
                return None;
            }
            Err(e) => panic!("{e}"),
        };
        let mut allocator = Allocator::new(&device);
        let bindless = Bindless::new(&device).unwrap();
        let frame = FrameRenderer::new(
            &device,
            &bindless,
            OUTPUT,
            vk::Format::D32_SFLOAT,
            ShadowSettings::default(),
            1,
        )
        .unwrap();
        let mut geometry =
            GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
        let cube = geometry.add_mesh(&cube().as_data()).unwrap();
        let ring = UploadRing::new(&device, &mut allocator, 1, 16 << 20).unwrap();
        let graph = RenderGraph::new(&device, 1);
        let (readback, memory) = allocator
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(size.0 * size.1 * 4))
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                MemoryLocation::Readback,
            )
            .unwrap();
        let d = device.raw();
        // SAFETY: plain object creation.
        let (pool, cmd) = unsafe {
            let pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(device.graphics.family),
                    None,
                )
                .unwrap();
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .command_buffer_count(1),
                )
                .unwrap()[0];
            (pool, cmd)
        };
        Some(Self {
            frame,
            ring,
            graph,
            geometry,
            textures: TexturePool::new(&device, 64 << 20, 1),
            materials: MaterialTable::new(1),
            cube,
            size,
            frame_number: 0,
            readback,
            readback_memory: Some(memory),
            pool,
            cmd,
            bindless,
            allocator,
            device,
            instance,
        })
    }

    /// Prepares `view`, renders one frame of `draws` from `camera`, waits, and returns
    /// the prepared state and the output (RGBA8, rows top to bottom) when `read` is set.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        view: &mut View,
        settings: &ViewSettings,
        draws: &[DrawItem],
        camera: &ViewCamera,
        read: bool,
        hook: impl FnOnce(&SceneTargets<'_>),
    ) -> (Prepared, Option<Vec<u8>>) {
        self.frame_number += 1;
        let frame_number = self.frame_number;
        let prepared = self
            .frame
            .prepare(
                view,
                settings,
                self.size,
                PrepareContext {
                    device: &self.device,
                    allocator: &mut self.allocator,
                    bindless: &self.bindless,
                    materials: &mut self.materials,
                    frame_number,
                },
            )
            .unwrap();
        let d = self.device.raw().clone();
        let cmd = self.cmd;
        // SAFETY: the previous frame finished (waited below).
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
        self.ring.begin_slot(0);
        let (width, height) = self.size;
        let readback = self.readback;
        let size = u64::from(width * height * 4);
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { self.graph.begin_slot(0, cmd) };
            self.textures
                .begin_frame(&mut fg, &mut self.allocator, &self.bindless, frame_number)
                .unwrap();
            let mats = self
                .materials
                .begin_frame(
                    &mut fg,
                    &self.device,
                    &mut self.allocator,
                    &self.textures,
                    frame_number,
                )
                .unwrap();
            let geo = self
                .geometry
                .begin_frame(&mut fg, &self.device, &mut self.allocator, frame_number)
                .unwrap();
            let out = fg.create_texture("out", TextureDesc::new_2d(OUTPUT, width, height));
            self.frame
                .add_view_with(
                    &mut fg,
                    &mut FrameContext {
                        device: &self.device,
                        allocator: &mut self.allocator,
                        bindless: &self.bindless,
                        ring: &mut self.ring,
                        frame_number,
                        dt: 1.0 / 60.0,
                    },
                    view,
                    &SceneInput {
                        geometry: &geo,
                        materials: &mats,
                        draws: SceneDraws::List(draws),
                        skinning: None,
                        light: DirectionalLight {
                            direction: Vec3::new(-0.3, -1.0, -0.2),
                            color: Vec3::splat(2.0),
                            ambient: Vec3::splat(0.3),
                        },
                        lights: &[],
                        environment: None,
                        reflection_probes: None,
                    },
                    camera,
                    ViewTarget {
                        texture: out,
                        rect: None,
                        load: LoadOp::DontCare,
                    },
                    |_, targets| hook(targets),
                )
                .unwrap();
            if read {
                let buffer = fg.import_buffer(
                    "readback",
                    ImportedBuffer {
                        buffer: readback,
                        size,
                        address: 0,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                fg.add_pass("read back")
                    .read(out, Access::TransferRead)
                    .write(buffer, Access::TransferWrite)
                    .execute(move |ctx| {
                        let region = vk::BufferImageCopy {
                            image_subresource: vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: 0,
                                base_array_layer: 0,
                                layer_count: 1,
                            },
                            image_extent: vk::Extent3D {
                                width,
                                height,
                                depth: 1,
                            },
                            ..Default::default()
                        };
                        // SAFETY: recording; layouts arranged by the graph.
                        unsafe {
                            ctx.device.cmd_copy_image_to_buffer(
                                ctx.cmd,
                                ctx.image(out),
                                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                                ctx.buffer(buffer),
                                &[region],
                            );
                        }
                    });
            } else {
                fg.add_pass("consume")
                    .read(out, Access::TransferRead)
                    .side_effects()
                    .execute(|_| {});
            }
            fg.execute_with_bindless(&mut self.allocator, Some(&self.bindless))
                .unwrap();
        }
        // SAFETY: as above.
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                self.device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            self.device.wait_idle().unwrap();
        }
        let pixels = read.then(|| {
            let ptr = self.readback_memory.as_ref().unwrap().mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping covers the image.
            unsafe { core::slice::from_raw_parts(ptr.as_ptr(), size as usize) }.to_vec()
        });
        (prepared, pixels)
    }

    /// Frees everything; checks the validation layer saw no error.
    pub fn finish(mut self, views: &mut [&mut View]) {
        self.device.wait_idle().unwrap();
        // SAFETY: the device is idle.
        unsafe {
            for view in views {
                view.destroy(&mut self.allocator, &self.bindless);
            }
            self.frame
                .destroy(&self.device, &mut self.allocator, &self.bindless);
            self.graph
                .destroy(&mut self.allocator, Some(&self.bindless));
            self.ring.destroy(&mut self.allocator);
            self.geometry.destroy(&mut self.allocator);
            self.materials.destroy(&mut self.allocator);
            self.textures.destroy(&mut self.allocator, &self.bindless);
            if let Some(memory) = self.readback_memory.take() {
                self.allocator.destroy_buffer(self.readback, memory);
            }
            self.device.raw().destroy_command_pool(self.pool, None);
        }
        let report = self.instance.validation_report();
        assert_eq!(
            report.errors, 0,
            "validation errors: {:#?}",
            report.first_errors
        );
    }

    /// A solid material.
    pub fn solid(&mut self, rgb: [f32; 3], emissive: [f32; 3]) -> MaterialId {
        self.materials.add(MaterialDesc {
            emissive,
            ..MaterialDesc::solid(rgb, 0.0, 0.8)
        })
    }

    /// A cube draw.
    pub fn cube_at(&self, material: MaterialId, scale: Vec3, at: Vec3) -> DrawItem {
        DrawItem {
            mesh: self.cube,
            material,
            world_from_object: Mat4::from_scale_rotation_translation(scale, Quat::IDENTITY, at),
        }
    }
}

/// A camera at `eye` looking at `target`.
pub fn camera(eye: Vec3, target: Vec3, cut: bool) -> ViewCamera {
    ViewCamera {
        view_from_world: look_at(eye, target, Vec3::Y),
        position: eye,
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        cut,
    }
}
