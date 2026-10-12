//! DLSS frame generation through Streamline (ADR 0091 §7), from the user's DLLs
//! (`sl.dlss_g.dll`, `nvngx_dlssg.dll`, `sl.reflex.dll`, `sl.pcl.dll`).
//!
//! Streamline presents: with frame generation loaded ([`crate::StreamlineDesc::frame_generation`]),
//! the surface and the swapchain are created through its hooks
//! ([`Streamline::vulkan_hooks`]), and its present call shows the generated frame before
//! each real one. The engine gives it, for every real frame:
//!
//! - the frame's inputs, tagged from a pass of the frame graph ([`DlssFrameGeneration::add_pass`]):
//!   depth and motion vectors at the render size, the frame without its UI ("hudless")
//!   and the UI alone (premultiplied color and coverage) at the output size, and the
//!   camera;
//! - Reflex's sleep and the PC latency markers ([`StreamlineLatency`], the
//!   `engine_latency::ExternalLatency` that DLSS frame generation requires in place of
//!   the driver's Reflex), with the same frame id.
//!
//! The back buffer the game presents is the hudless frame with the UI over it; DLSS frame
//! generation composites the UI over the frames it generates itself.

use crate::{DlssError, FrameGenerationFunctions, NEXT_VIEWPORT, Streamline, sl, transition};
use core::ffi::c_void;
use core::sync::atomic::Ordering;
use engine_core::math::{Mat4, Vec3};
use engine_latency::{ExternalLatency, LatencySettings, LowLatency, Marker};
use engine_render::graph::{Access, FrameGraph, ShaderStages, Texture, TextureDesc};
use engine_rhi::ash::vk;

/// The camera of a real frame, as DLSS frame generation reads it (no jitter anywhere).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenerationCamera {
    /// Clip from view: the projection without jitter (the engine's: right-handed view
    /// space looking down −Z, Vulkan's clip space).
    pub clip_from_view: Mat4,
    /// The last real frame's clip space from this frame's (identity after a cut).
    pub previous_clip_from_clip: Mat4,
    /// Position in world space.
    pub position: Vec3,
    /// The view's up, right and forward (looking) directions in world space.
    pub up: Vec3,
    /// See `up`.
    pub right: Vec3,
    /// See `up`.
    pub forward: Vec3,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance.
    pub far: f32,
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Width over height.
    pub aspect: f32,
}

/// One real frame's inputs to DLSS frame generation.
#[derive(Debug, Clone, Copy)]
pub struct GenerationInput {
    /// The scene's depth at the render size: 0 at the near plane, 1 at the far plane.
    pub depth: Texture,
    /// Motion vectors at the render size: the change of texture coordinates from the
    /// last frame to this one (the engine's), with camera motion, without jitter.
    pub velocity: Texture,
    /// The frame without its UI at the output size, encoded as the back buffer.
    pub hudless: Texture,
    /// The UI alone at the output size, as the back buffer: premultiplied color and
    /// coverage, zero where there is no UI. `None` without a UI.
    pub ui: Option<Texture>,
    /// The camera.
    pub camera: GenerationCamera,
    /// This frame's jitter, as the upscalers get it (`MotionOutput::jitter`).
    pub jitter: [f32; 2],
    /// No frame is generated between the last real frame and this one: a cut, the first
    /// frame, a change of size.
    pub reset: bool,
}

/// What DLSS frame generation reports ([`DlssFrameGeneration::state`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationState {
    /// `sl::DLSSGStatus` bits: 0 when it runs (see [`GenerationState::status_text`]).
    pub status: u32,
    /// Frames presented since the last call: two per real frame while frames are
    /// generated, one when they are not.
    pub presented: u32,
    /// The most frames it can generate per real frame on this system (1 on RTX 40).
    pub max_generated: u32,
    /// The smallest output width or height it runs at.
    pub min_size: u32,
}

impl GenerationState {
    /// Why it does not run, from [`Self::status`].
    pub fn status_text(&self) -> &'static str {
        match self.status {
            0 => "ok",
            s if s & 1 != 0 => "the output is too small",
            s if s & 2 != 0 => "Reflex is not on",
            s if s & 4 != 0 => "the HDR format is not supported",
            s if s & 8 != 0 => "the common constants are invalid",
            _ => "unknown",
        }
    }
}

/// Usage flags Streamline is told the tagged images have (it creates its own views and
/// copies of them).
const SAMPLED_COPY: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::SAMPLED.as_raw() | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
);

/// DLSS frame generation for the view whose frames are presented (see the module
/// documentation). One per swapchain: DLSS frame generation runs on one view.
pub struct DlssFrameGeneration {
    streamline: Streamline,
    functions: FrameGenerationFunctions,
    viewport: u32,
    on: bool,
    /// Tests: show only the generated frames, and the constants in Vulkan's clip space.
    only_generated: bool,
    vulkan_clip: bool,
}

impl core::fmt::Debug for DlssFrameGeneration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DlssFrameGeneration")
            .field("viewport", &self.viewport)
            .field("on", &self.on)
            .finish_non_exhaustive()
    }
}

impl DlssFrameGeneration {
    /// Frame generation on the device `streamline` is attached to, off until
    /// [`Self::set_enabled`]. Fails when it cannot run there
    /// ([`Streamline::frame_generation_supported`]).
    pub fn new(streamline: &Streamline) -> Result<Self, DlssError> {
        let functions = streamline
            .frame_generation_functions()
            .ok_or_else(|| DlssError::MissingRequirement(String::from("DLSS frame generation")))?;
        Ok(Self {
            streamline: streamline.clone(),
            functions,
            viewport: NEXT_VIEWPORT.fetch_add(1, Ordering::Relaxed),
            on: false,
            only_generated: false,
            vulkan_clip: false,
        })
    }

    /// Turns frame generation on or off from the next present (`slDLSSGSetOptions`; call
    /// it on the presenting thread). Off keeps its resources for a quick return; for a
    /// long pause, turn it off, recreate the swapchain with frame generation unloaded
    /// ([`Streamline::set_frame_generation_loaded`]) and call [`Self::free`].
    pub fn set_enabled(&mut self, on: bool) -> Result<(), DlssError> {
        let mut options = sl::DlssgOptions::new();
        options.mode = if on {
            sl::DLSSG_MODE_ON
        } else {
            sl::DLSSG_MODE_OFF
        };
        options.num_frames_to_generate = 1;
        options.flags = sl::DLSSG_FLAG_RETAIN_RESOURCES_WHEN_OFF
            | if self.only_generated {
                sl::DLSSG_FLAG_SHOW_ONLY_INTERPOLATED
            } else {
                0
            };
        // The UI is interpolated apart from the scene and composited over it (DLSS-FG
        // guide §6.6): the HUD stays sharp, UI placed in the world moves with it.
        options.enable_user_interface_recomposition = sl::TRUE;
        let handle = sl::ViewportHandle::new(self.viewport);
        // SAFETY: Streamline is attached; both structures outlive the call.
        let result = unsafe { (self.functions.set_options)(&handle, &options) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slDLSSGSetOptions",
                result,
            });
        }
        self.on = on;
        Ok(())
    }

    #[doc(hidden)]
    /// Tests: show only the generated frames (from the next [`Self::set_enabled`]), and
    /// give the constants in the engine's (Vulkan's) clip space instead of Direct3D's.
    pub fn set_debug(&mut self, only_generated: bool, vulkan_clip: bool) {
        self.only_generated = only_generated;
        self.vulkan_clip = vulkan_clip;
    }

    /// Whether frame generation is on ([`Self::set_enabled`]).
    pub fn enabled(&self) -> bool {
        self.on
    }

    /// Its state (`slDLSSGGetState`): call it once per presented real frame for
    /// [`GenerationState::presented`].
    pub fn state(&self) -> Result<GenerationState, DlssError> {
        let handle = sl::ViewportHandle::new(self.viewport);
        let mut state = sl::DlssgState::new();
        // SAFETY: Streamline is attached; no options: no memory estimate (the guide's
        // per-frame use).
        let result = unsafe { (self.functions.get_state)(&handle, &mut state, core::ptr::null()) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slDLSSGGetState",
                result,
            });
        }
        Ok(GenerationState {
            status: state.status,
            presented: state.num_frames_actually_presented,
            max_generated: state.num_frames_to_generate_max,
            min_size: state.min_width_or_height,
        })
    }

    /// Declares the pass that hands frame `frame`'s inputs to DLSS frame generation: the
    /// common constants, and the images tagged as valid only now (Streamline copies them
    /// in the pass: the graph may reuse their memory before the present). Declare it
    /// after the passes that write the UI. `frame` is the id the latency markers use
    /// ([`StreamlineLatency`] through `engine_latency::Latency`).
    pub fn add_pass<'g>(&'g self, fg: &mut FrameGraph<'g>, input: GenerationInput, frame: u64) {
        engine_profiler::zone!("dlss::DlssFrameGeneration::add_pass");
        let depth_desc = fg.texture_desc(input.depth);
        let velocity_desc = fg.texture_desc(input.velocity);
        let hudless_desc = fg.texture_desc(input.hudless);
        let ui_desc = input.ui.map(|ui| fg.texture_desc(ui));
        let constants = constants(&input.camera, input.jitter, input.reset, self.vulkan_clip);
        let (get_new_frame_token, set_constants, set_tag_for_frame, _) =
            self.streamline.core_functions();
        let viewport = self.viewport;
        let stage = Access::Sampled(ShaderStages::COMPUTE);
        // Streamline copies the images outside the graph: nothing in the graph reads what
        // this pass does, which must not be culled.
        let mut pass = fg
            .add_pass("dlss frame generation inputs")
            .side_effects()
            .read(input.depth, stage)
            .read(input.velocity, stage)
            .read(input.hudless, stage);
        if let Some(ui) = input.ui {
            pass = pass.read(ui, stage);
        }
        pass.execute(move |ctx| {
            let resource = |texture: Texture, desc: TextureDesc| {
                let mut r = sl::Resource::empty();
                r.native = vk::Handle::as_raw(ctx.image(texture)) as *mut c_void;
                r.view = vk::Handle::as_raw(ctx.view(texture)) as *mut c_void;
                r.state = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw() as u32;
                r.width = desc.width;
                r.height = desc.height;
                r.native_format = desc.format.as_raw() as u32;
                r.mip_levels = 1;
                r.array_layers = 1;
                r.usage = SAMPLED_COPY.as_raw();
                r
            };
            let mut resources = [
                resource(input.depth, depth_desc),
                resource(input.velocity, velocity_desc),
                resource(input.hudless, hudless_desc),
                match (input.ui, ui_desc) {
                    (Some(ui), Some(desc)) => resource(ui, desc),
                    _ => sl::Resource::empty(),
                },
            ];
            let images = [
                (resources[0].native, vk::ImageAspectFlags::DEPTH),
                (resources[1].native, vk::ImageAspectFlags::COLOR),
                (resources[2].native, vk::ImageAspectFlags::COLOR),
                (resources[3].native, vk::ImageAspectFlags::COLOR),
            ];
            // As for DLSS (the crate's `Dlss`): the graph's READ_ONLY_OPTIMAL is a layout
            // Streamline does not know; it copies from SHADER_READ_ONLY_OPTIMAL.
            // SAFETY: recording outside a rendering scope; the images are the pass's
            // declared inputs, synchronized by the graph for sampling.
            unsafe {
                transition(
                    ctx,
                    &images,
                    (
                        vk::PipelineStageFlags2::ALL_COMMANDS,
                        vk::AccessFlags2::MEMORY_WRITE,
                    ),
                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
            }
            let kinds = [
                sl::BUFFER_DEPTH,
                sl::BUFFER_MOTION_VECTORS,
                sl::BUFFER_HUDLESS_COLOR,
                sl::BUFFER_UI_COLOR_AND_ALPHA,
            ];
            let mut tags = [sl::ResourceTag::empty(); 4];
            for ((tag, resource), kind) in tags.iter_mut().zip(&mut resources).zip(kinds) {
                tag.resource = resource;
                tag.kind = kind;
                tag.lifecycle = sl::LIFECYCLE_ONLY_VALID_NOW;
            }
            let count = if input.ui.is_some() { 4 } else { 3 };
            let handle = sl::ViewportHandle::new(viewport);
            let frame_index = frame as u32;
            let mut token = core::ptr::null_mut();
            // SAFETY: Streamline is attached; every structure outlives the calls; the
            // command buffer is the frame's, recording; the tagged images are in the
            // layout stated.
            let result = unsafe {
                let mut result = get_new_frame_token(&mut token, &frame_index);
                if result == sl::OK {
                    result = set_constants(&constants, token, &handle);
                }
                if result == sl::OK {
                    result = set_tag_for_frame(
                        token,
                        &handle,
                        tags.as_ptr(),
                        count,
                        vk::Handle::as_raw(ctx.cmd) as *mut c_void,
                    );
                }
                result
            };
            if result != sl::OK {
                crate::EVALUATE_FAILURES.fetch_add(1, Ordering::Relaxed);
                crate::LAST_EVALUATE_RESULT.store(result, Ordering::Relaxed);
            }
            // SAFETY: as above.
            unsafe {
                transition(
                    ctx,
                    &images,
                    (
                        vk::PipelineStageFlags2::ALL_COMMANDS,
                        vk::AccessFlags2::NONE,
                    ),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::READ_ONLY_OPTIMAL,
                );
            }
        });
    }

    /// Frees frame generation's resources (`slFreeResources`) after turning it off.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them: the device is idle (through Streamline's
    /// `vkDeviceWaitIdle`, [`engine_rhi::VulkanHooks::device_wait_idle`]).
    pub unsafe fn free(&mut self) -> Result<(), DlssError> {
        if self.on {
            self.set_enabled(false)?;
        }
        let (_, _, _, free) = self.streamline.core_functions();
        let handle = sl::ViewportHandle::new(self.viewport);
        // SAFETY: Streamline is attached; the caller guarantees the GPU is done.
        let result = unsafe { free(sl::FEATURE_DLSS_G, &handle) };
        if result != sl::OK {
            return Err(DlssError::Streamline {
                call: "slFreeResources",
                result,
            });
        }
        Ok(())
    }

    /// Streamline's Reflex and PC latency markers for `engine_latency::Latency::with_external`
    /// (DLSS frame generation runs with no other Reflex integration).
    pub fn latency(&self) -> StreamlineLatency {
        let (get_new_frame_token, _, _, _) = self.streamline.core_functions();
        StreamlineLatency {
            get_new_frame_token,
            functions: self.functions,
        }
    }
}

/// The common constants of a frame (`sl::Constants`): Streamline's matrices are row-major
/// for row vectors (`sl_matrix_helpers.h`), view space looks down +Z, clip space has y up
/// (Direct3D's conventions).
fn constants(
    camera: &GenerationCamera,
    jitter: [f32; 2],
    reset: bool,
    vulkan_clip: bool,
) -> sl::Constants {
    // The engine's view space looks down −Z, its clip space has y down (Vulkan).
    let flip_z = Mat4::from_diagonal(engine_core::math::Vec4::new(1.0, 1.0, -1.0, 1.0));
    let flip_y = if vulkan_clip {
        Mat4::IDENTITY
    } else {
        Mat4::from_diagonal(engine_core::math::Vec4::new(1.0, -1.0, 1.0, 1.0))
    };
    let clip_from_view = flip_y * camera.clip_from_view * flip_z;
    let previous_clip_from_clip = flip_y * camera.previous_clip_from_clip * flip_y;
    // Column-vector matrices: the columns of M are the rows of Mᵀ, its row-vector form.
    let rows = |m: Mat4| m.to_cols_array_2d();
    let mut c = sl::Constants::new();
    c.camera_view_to_clip = rows(clip_from_view);
    c.clip_to_camera_view = rows(clip_from_view.inverse());
    c.clip_to_prev_clip = rows(previous_clip_from_clip);
    c.prev_clip_to_clip = rows(previous_clip_from_clip.inverse());
    c.jitter_offset = jitter;
    // The engine's vectors run from the last frame to this one in texture coordinates;
    // Streamline's point back to the last frame, normalized (as for DLSS, `Dlss::add_pass`).
    c.mvec_scale = [-1.0, -1.0];
    c.camera_pos = camera.position.to_array();
    c.camera_up = camera.up.to_array();
    c.camera_right = camera.right.to_array();
    c.camera_fwd = camera.forward.to_array();
    c.camera_near = camera.near;
    c.camera_far = camera.far;
    c.camera_fov = camera.fov_y;
    c.camera_aspect_ratio = camera.aspect;
    c.depth_inverted = sl::FALSE;
    c.camera_motion_included = sl::TRUE;
    c.motion_vectors_3d = sl::FALSE;
    c.motion_vectors_dilated = sl::FALSE;
    c.motion_vectors_jittered = sl::FALSE;
    c.orthographic_projection = sl::FALSE;
    c.reset = if reset { sl::TRUE } else { sl::FALSE };
    c
}

/// Streamline's Reflex and PC latency markers ([`DlssFrameGeneration::latency`]), as an
/// `engine_latency` backend: DLSS frame generation needs Reflex on, so the mode is at
/// least "on" while it is loaded.
#[derive(Clone, Copy)]
pub struct StreamlineLatency {
    get_new_frame_token: sl::PfnGetNewFrameToken,
    functions: FrameGenerationFunctions,
}

impl core::fmt::Debug for StreamlineLatency {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamlineLatency").finish_non_exhaustive()
    }
}

impl StreamlineLatency {
    fn token(&self, frame: u64) -> Option<*mut sl::FrameToken> {
        let index = frame as u32;
        let mut token = core::ptr::null_mut();
        // SAFETY: Streamline is initialized; the index outlives the call.
        let result = unsafe { (self.get_new_frame_token)(&mut token, &index) };
        (result == sl::OK && !token.is_null()).then_some(token)
    }
}

impl ExternalLatency for StreamlineLatency {
    fn apply(&mut self, settings: &LatencySettings) {
        let mut options = sl::ReflexOptions::new();
        options.mode = match settings.mode {
            LowLatency::Off | LowLatency::On => sl::REFLEX_MODE_LOW_LATENCY,
            LowLatency::Boost => sl::REFLEX_MODE_LOW_LATENCY_WITH_BOOST,
        };
        options.frame_limit_us = settings
            .fps_limit
            .map_or(0, |fps| (1e6 / fps.max(1.0)) as u32);
        // SAFETY: Streamline is attached; the options outlive the call.
        let _ = unsafe { (self.functions.reflex_set_options)(&options) };
    }

    fn sleep(&mut self, frame: u64) {
        if let Some(token) = self.token(frame) {
            // SAFETY: the token is Streamline's for this frame.
            let _ = unsafe { (self.functions.reflex_sleep)(token) };
        }
    }

    fn mark(&mut self, frame: u64, marker: Marker) {
        let pcl = match marker {
            // The PC latency markers have no plain input sample (Streamline 2.x).
            Marker::InputSample => return,
            Marker::SimulationStart => sl::PCL_SIMULATION_START,
            Marker::SimulationEnd => sl::PCL_SIMULATION_END,
            Marker::RenderSubmitStart => sl::PCL_RENDER_SUBMIT_START,
            Marker::RenderSubmitEnd => sl::PCL_RENDER_SUBMIT_END,
            Marker::PresentStart => sl::PCL_PRESENT_START,
            Marker::PresentEnd => sl::PCL_PRESENT_END,
        };
        if let Some(token) = self.token(frame) {
            // SAFETY: as in `sleep`.
            let _ = unsafe { (self.functions.pcl_set_marker)(pcl, token) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Quat, Vec4};

    fn camera(rotation: Quat, position: Vec3) -> (Mat4, Mat4) {
        let projection = engine_core::math::vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 100.0);
        let view = Mat4::from_rotation_translation(rotation, position).inverse();
        (projection, view)
    }

    /// A point seen by both cameras lands where the previous camera saw it, through the
    /// constants in Streamline's conventions (row vectors, y up, +Z forward).
    #[test]
    fn constants_reproject_in_streamlines_conventions() {
        let (projection, view) = camera(Quat::from_rotation_y(0.1), Vec3::new(0.0, 1.0, 0.0));
        let (_, previous_view) = camera(Quat::from_rotation_x(0.05), Vec3::new(0.2, 1.0, 0.3));
        let clip_from_world = projection * view;
        let previous_clip_from_world = projection * previous_view;
        let camera = GenerationCamera {
            clip_from_view: projection,
            previous_clip_from_clip: previous_clip_from_world * clip_from_world.inverse(),
            position: Vec3::ZERO,
            up: Vec3::Y,
            right: Vec3::X,
            forward: Vec3::NEG_Z,
            near: 0.1,
            far: 100.0,
            fov_y: 1.0,
            aspect: 16.0 / 9.0,
        };
        let c = constants(&camera, [0.0; 2], false, false);
        let row_mul = |v: Vec4, m: &sl::Float4x4| {
            Vec4::new(
                v.x * m[0][0] + v.y * m[1][0] + v.z * m[2][0] + v.w * m[3][0],
                v.x * m[0][1] + v.y * m[1][1] + v.z * m[2][1] + v.w * m[3][1],
                v.x * m[0][2] + v.y * m[1][2] + v.z * m[2][2] + v.w * m[3][2],
                v.x * m[0][3] + v.y * m[1][3] + v.z * m[2][3] + v.w * m[3][3],
            )
        };
        let world = Vec4::new(0.5, 1.3, -6.0, 1.0);
        // The engine's clip spaces (y down), then Direct3D's (y up).
        let clip = clip_from_world * world;
        let previous = previous_clip_from_world * world;
        let d3d = |v: Vec4| Vec4::new(v.x, -v.y, v.z, v.w);
        let reprojected = row_mul(d3d(clip), &c.clip_to_prev_clip);
        let expected = d3d(previous);
        let ndc = |v: Vec4| v.truncate() / v.w;
        assert!((ndc(reprojected) - ndc(expected)).abs().max_element() < 1e-4);
        // And back.
        let back = row_mul(reprojected, &c.prev_clip_to_clip);
        assert!((ndc(back) - ndc(d3d(clip))).abs().max_element() < 1e-4);
        // A point straight ahead (+Z in Streamline's view space) is in front: w > 0, and
        // projects to the center.
        let ahead = row_mul(Vec4::new(0.0, 0.0, 5.0, 1.0), &c.camera_view_to_clip);
        assert!(ahead.w > 0.0);
        assert!(ndc(ahead).truncate().length() < 1e-5);
        // Up in view space is up in Direct3D's clip space.
        let up = row_mul(Vec4::new(0.0, 1.0, 5.0, 1.0), &c.camera_view_to_clip);
        assert!(ndc(up).y > 0.0);
        // Depth runs from 0 at the near plane.
        let near = row_mul(Vec4::new(0.0, 0.0, 0.1, 1.0), &c.camera_view_to_clip);
        assert!(ndc(near).z.abs() < 1e-4);
    }

    #[test]
    fn status_reads_as_text() {
        let state = |status| GenerationState {
            status,
            ..GenerationState::default()
        };
        assert_eq!(state(0).status_text(), "ok");
        assert_eq!(state(2).status_text(), "Reflex is not on");
    }
}
