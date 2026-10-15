//! The forward renderer's passes for a laid-out frame ([`ForwardRenderer::declare`]): culling,
//! the shadow cascades and local shadows, the depth prepass, ambient occlusion and contact
//! shadows, the opaque shading (single-sampled and multisampled), the sky or the
//! atmosphere's composite, the transparent surfaces, subsurface scattering and
//! reflections, in the order the frame runs them.

use super::*;
use crate::graph::PassContext;
use crate::shadow::Keep;
use crate::shadow_cache::{Held, MAX_PIECES, Piece, Plan as ShadowPlan};
use crate::vsm::{VIRTUAL_TEXELS, VirtualShadows, VsmFrame, VsmPrepared, VsmView};

impl ForwardRenderer {
    /// Declares the passes of a laid-out frame (both paths).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) fn declare<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        view: &ForwardView<'g>,
        setup: &Setup,
        layout: &Layout,
        imported: &Imported<'g>,
        capture_shadow: Option<&CaptureShadow>,
    ) -> Option<ForwardPasses> {
        let names = if setup.capture {
            &CAPTURE_NAMES
        } else {
            &VIEW_NAMES
        };
        let (color, depth) = (view.color, view.depth);
        let load = view.clear.map_or(LoadOp::Load, LoadOp::Clear);
        let depth_clear = LoadOp::Clear([1.0, 0.0, 0.0, 0.0]);
        let Setup {
            depth_desc,
            phases,
            cascade_count,
            shadow_draws,
            capture,
            vsm: vsm_on,
        } = *setup;
        let occlusion = view.occlusion.filter(|_| phases == 2);
        // MSAA (ADR 0075): multisampled targets draw with their sample count's pipelines,
        // always after a depth prepass, and what reads the scene by pixel (ambient
        // occlusion, contact shadows, culling, reflections, subsurface scattering) reads
        // single-sampled resolves.
        let samples = depth_desc.samples;
        let ms = if samples == vk::SampleCountFlags::TYPE_1 {
            None
        } else {
            Some(self.msaa_pipelines(samples)?)
        };
        let single = |format| TextureDesc::new_2d(format, depth_desc.width, depth_desc.height);
        let multi = |format| TextureDesc {
            samples,
            ..single(format)
        };
        let resolved_depth =
            ms.map(|_| fg.create_texture("depth resolved", single(self.depth_format)));
        let read_depth = resolved_depth.unwrap_or(depth);
        let [nearest, farthest] = self.depth_resolves;
        let Layout {
            instance_count,
            instances,
            source,
            batch_table,
            commands,
            command_count,
            drawn_commands,
            slot_count,
            batch_count,
            states,
            min,
            max,
            subsurface_draws,
            meshlets,
            vsm: vsm_layout,
        } = *layout;
        // The meshlet lists (ADR 0085): in the command buffer after every phase's command
        // list, one per phase; their draws' slots in the visible buffer after the phases'.
        let command_bytes = u64::from(command_count) * COMMAND_BYTES;
        let meshlet_draws = meshlets.map(|layout| MeshletDraws {
            layout,
            base: (u64::from(phases) * command_bytes).next_multiple_of(16),
            bytes: layout.list_bytes(),
        });
        let meshlet_shaders = self.meshlet_shaders().filter(|_| meshlet_draws.is_some());
        // And each shadow cascade's (the shadow passes draw depth only: any order will do),
        // after the cascades' command lists, their draws' slots after the cascades' casters.
        let shadow_meshlets = meshlets
            .filter(|_| cascade_count > 0 && meshlet_shaders.is_some())
            .map(|layout| MeshletDraws {
                layout,
                base: (u64::from(cascade_count) * command_bytes).next_multiple_of(16),
                bytes: layout.list_bytes(),
            });
        let shadow_shaders = meshlet_shaders.filter(|_| shadow_meshlets.is_some());

        let subsurface = self.subsurface_for(view);
        // The scene's instances, read by culling and by the vertex shaders.
        let scene_instances = match source {
            Source::Scene { current, .. } => Some(current),
            Source::Ring(_) => None,
        };
        let pyramid_layout = PyramidLayout::new(depth_desc.width, depth_desc.height);
        // Temporal state: this frame's jitter (the projection moved by a sub-pixel
        // offset), last frame's camera, whether last frame is related at all.
        let temporal = view.temporal;
        let (jitter, unjittered, clip_from_world) = jittered(view, &depth_desc);
        let jitter_ndc = [
            2.0 * jitter[0] / depth_desc.width.max(1) as f32,
            2.0 * jitter[1] / depth_desc.height.max(1) as f32,
        ];
        let reset = temporal.is_some_and(|t| t.reset.replace(false));
        let previous_clip_from_world = temporal
            .and_then(|t| t.previous_clip_from_world.get())
            .unwrap_or(unjittered);
        let previous_count = match source {
            Source::Ring(_) => temporal.map_or(0, |t| t.previous_count.get()),
            // Every slot holds last frame's transform (new and teleported instances their
            // current one), unless the view's history was reset.
            Source::Scene { .. } if reset => 0,
            Source::Scene { .. } => instance_count,
        };
        // The environment, imported (and rebuilt if needed) by the caller.
        let world_from_clip = clip_from_world.inverse();
        let environment = imported.environment;
        let atmosphere = environment.and_then(|e| e.atmosphere);
        // Local lights: culled, sorted and binned on the CPU (ADR 0045).
        let axis = ViewAxis::new(unjittered);
        let (light_layout, uploaded) = {
            let mut scratch = self.scratch.borrow_mut();
            let scratch = &mut scratch.lights;
            let layout = match axis {
                Some(axis) if !view.lights.is_empty() => cull_lights(
                    view.lights,
                    unjittered,
                    axis,
                    [depth_desc.width, depth_desc.height],
                    scratch,
                ),
                _ => LightLayout::EMPTY,
            };
            let uploaded = if layout.count > 0 {
                let n = layout.count as usize;
                let (out, lights) = ring.alloc::<LightData>(n)?;
                write_lights(view.lights, scratch, out);
                let (out, rects) = ring.alloc::<[u32; 2]>(n)?;
                write_rects(scratch, out);
                let (out, bins) = ring.alloc::<u32>(Z_BINS)?;
                write_bins(view.lights, &layout, scratch, out);
                Some((lights.address, rects.address, bins.address))
            } else {
                None
            };
            (layout, uploaded)
        };
        // Local lights' shadows (ADR 0071): each kept light's first view, the views, the
        // views drawn this frame, and the atlas (imported once per frame).
        let local = self
            .local_shadows
            .as_ref()
            .filter(|_| self.local_shadows_on && view.features.local_shadows && !capture);
        let mut local_frame = LocalFrame::default();
        if let Some(state) = local {
            let plan = state.plan.borrow();
            if light_layout.count > 0 && !plan.views.is_empty() {
                let scratch = self.scratch.borrow();
                let n = light_layout.count as usize;
                let (out, staged) = ring.alloc::<u32>(n)?;
                for (out, index) in out.iter_mut().zip(scratch.lights.kept()) {
                    let first = plan.first_view[index as usize];
                    local_frame.shadowed |= first != NO_SHADOW;
                    out.write(first);
                }
                local_frame.light_shadows = staged.address;
            }
            if local_frame.shadowed {
                local_frame.views = ring.push(&plan.views)?;
            }
            if !plan.drawn.is_empty() {
                let (out, staged) = ring.alloc::<ShadowViewData>(plan.drawn.len())?;
                for (out, view) in out.iter_mut().zip(plan.drawn.iter()) {
                    out.write(view.data);
                }
                local_frame.draws = staged.address;
            }
            if local_frame.shadowed || !plan.drawn.is_empty() {
                let serial = fg.serial();
                let atlas = match state.imported.get() {
                    Some((s, texture)) if s == serial => texture,
                    _ => {
                        let final_state =
                            crate::graph::TextureState::shader_read(ShaderStages::FRAGMENT);
                        let texture = fg.import_texture(
                            "local shadow atlas",
                            crate::graph::ImportedTexture {
                                image: state.atlas.image,
                                view: state.atlas.view,
                                desc: state.atlas.desc,
                                initial: state.atlas.state.get(),
                                final_state: Some(final_state),
                                sampled_index: Some(state.atlas.sampled),
                                storage_index: None,
                            },
                        );
                        state.atlas.state.set(final_state);
                        state.imported.set(Some((serial, texture)));
                        state.rects.borrow_mut().clear();
                        texture
                    }
                };
                local_frame.atlas = Some(atlas);
                let mut rects = state.rects.borrow_mut();
                // Room for every view of every light in pieces (more views of one frame
                // may add theirs), so later frames do not grow it.
                rects.reserve(
                    plan.views.len().max(plan.drawn.len()) * 2 * crate::local_shadows::PIECES,
                );
                let first = rects.len();
                rects.extend(plan.drawn.iter().map(|d| d.rect));
                local_frame.rects = first..rects.len();
            }
        }
        // Subsurface profiles, for transmission and the scatter pass (ADR 0059).
        let profiles = if subsurface_draws {
            ring.push(&self.subsurface_profiles)?
        } else {
            0
        };
        let settings = &self.shadow_settings;
        // Reflections (ADR 0073): with an environment (their last fallback), in perspective
        // views, never in a probe's capture. Their constants (and visible probes) go to the
        // ring before the shading.
        let reflection_passes = self.reflections.as_ref().filter(|_| {
            self.reflections_on && view.features.reflections && !capture && view.reflections.any()
        });
        let prepared = match (reflection_passes, environment) {
            (Some(r), Some(env)) => r.passes.prepare(
                ring,
                &ViewSetup {
                    environment: env,
                    clip_from_world,
                    camera: view.camera.position,
                    size: [depth_desc.width, depth_desc.height],
                    probes: imported.probes.map(|(p, _)| p),
                    ssr: view.reflections.ssr,
                },
            ),
            _ => None,
        };
        // Whether the view reflects anything: visible probes or screen-space reflections.
        let reflecting = prepared.is_some_and(|p| p.probes > 0 || p.ssr.is_some());
        // Decals (ADR 0082): culled and binned for the view, then drawn into the decal
        // buffer after the depth prepass. Not in probe captures or multisampled views.
        let decals = self
            .decals
            .as_deref()
            .filter(|_| {
                self.decals_on
                    && view.features.decals
                    && !capture
                    && ms.is_none()
                    && !view.decals.is_empty()
            })
            .and_then(|d| {
                let size = [depth_desc.width, depth_desc.height];
                let prepared = d.passes.prepare(ring, view.decals, clip_from_world, size)?;
                Some((d, prepared))
            });
        // GI (ADR 0088, ADR 0090): a pass after the depth prepass writes each pixel's
        // diffuse irradiance, from the probe volume or a screen GI, which the GI shading
        // reads. Not in multisampled views, nor before the volume's first update, nor a
        // screen GI in probe captures.
        let gi_view = view.gi.filter(|g| {
            self.gi_on
                && view.features.global_illumination
                && ms.is_none()
                && self.gi.is_some()
                && match g {
                    ViewGi::Probes { volume, .. } => volume.has_light(),
                    ViewGi::Screen(_) => !capture,
                }
        });
        // Ambient occlusion needs the whole depth before shading: the prepass draws it.
        // Probe captures draw without it.
        let ao = self
            .ao_settings
            .filter(|_| view.features.ambient_occlusion && !capture)
            .zip(view.camera.view_space())
            .zip(self.ao.as_ref());
        // The sun's contact shadows (ADR 0071) need it too.
        let contact = self
            .contact_settings
            .filter(|_| view.features.contact_shadows && !capture)
            .zip(view.camera.view_space())
            .zip(self.contact.as_ref());
        // As do multisampled targets, subsurface materials (their pass follows the forward
        // pass with the depth tested for equality), reflections (their inputs are written
        // by the shading after it), decals and GI.
        // The sun's virtual shadow pages (ADR 0093 step 3b) are marked from it too.
        let vsm_view = view
            .virtual_shadows
            .zip(self.vsm_draw())
            .zip(vsm_layout)
            .zip(ViewAxis::new(unjittered))
            .filter(|_| vsm_on);
        let prepass = ms.is_some()
            || (self.depth_prepass && view.features.depth_prepass)
            || ao.is_some()
            || subsurface_draws
            || contact.is_some()
            || reflecting
            || decals.is_some()
            || gi_view.is_some()
            || vsm_view.is_some();
        // Views draw by meshlet (ADR 0085) only after a depth prepass: without one, the
        // meshlet draws' order (the GPU's) would undo the front-to-back order by which the
        // depth test rejects hidden pixels (Sponza without a prepass: forward +15 % on the
        // Radeon, +35-50 % on the RTX).
        let meshlet_draws = meshlet_draws.filter(|_| prepass);
        let meshlet_shaders = meshlet_shaders.filter(|_| prepass);
        let meshlet_lists = meshlet_draws.map(|m| MeshletLists {
            offset: m.base,
            bytes: m.bytes,
            draws: m.layout.draws,
        });
        // The sky's irradiance harmonics outside the volume (the flat ambient as ones:
        // radiance `ambient` everywhere is irradiance π × ambient).
        let gi_sky = match (gi_view, environment) {
            (None | Some(ViewGi::Screen(_)), _) => None,
            (Some(ViewGi::Probes { .. }), Some(e)) => Some((e.sh_address, e.intensity)),
            (Some(ViewGi::Probes { .. }), None) => {
                let e = view.light.ambient * (core::f32::consts::PI / 0.282_095);
                let mut sh = [[0.0f32; 4]; 9];
                sh[0] = [e.x, e.y, e.z, 0.0];
                Some((ring.push(&sh)?, 1.0))
            }
        };
        // The sun's cascades (ADR 0093): a view's cache holds them still across frames
        // (their texel size while it covers the view, the scene's depth range) and draws
        // only what changed; without one (and in a probe's capture) each is drawn whole.
        let cache = view.shadow_cache.filter(|c| !capture && c.fits(settings));
        let fit = match capture_shadow {
            Some(c) => c.fit,
            None => shadow::fit_cascades_keeping(
                unjittered,
                view.camera.position,
                view.light.direction,
                min,
                max,
                settings,
                &cache.map_or_else(Keep::default, CascadeCache::keep),
            ),
        };
        let resolution = settings.resolution.max(1);
        // A cache's cascade drawn in several pieces draws them at once with the meshlet path,
        // where vertex shaders pick viewports (ADR 0093): the piece cull and pipelines, and
        // where the meshlet draws' pieces go in the caster lists (after the cascades'
        // meshlet slots, from a draw's slot).
        let pieces = match (cache, shadow_shaders, &self.shadow_pieces, shadow_meshlets) {
            (Some(_), Some(s), Some(p), Some(m)) => s.pieces.as_ref().and_then(|cull| {
                Some((
                    cull,
                    [ready(&p[0])?, ready(&p[1])?],
                    cascade_count * m.layout.draws,
                ))
            }),
            _ => None,
        };
        let (plans, held) = match cache {
            Some(c) => c.plan(&fit, pieces.is_some()),
            None => whole_plans(&fit, resolution),
        };
        // The pages' frame (ADR 0093 step 3b), written before the frame's data, which holds
        // its address: the camera, the light, the scene's extent along the light.
        let vsm = vsm_view.and_then(|(((pages, draw), lists), axis)| {
            let direction = view.light.direction.normalize_or(Vec3::NEG_Y);
            let along = (0..8).map(|i: u32| {
                Vec3::new(
                    if i & 1 == 0 { min.x } else { max.x },
                    if i & 2 == 0 { min.y } else { max.y },
                    if i & 4 == 0 { min.z } else { max.z },
                )
                .dot(direction)
            });
            let extent = along.fold([f32::MAX, f32::MIN], |[lo, hi], d| [lo.min(d), hi.max(d)]);
            let pixels = crate::subsurface::Footprint::new(
                unjittered,
                axis.forward,
                axis.near_center,
                depth_desc.height,
            )
            .pixels_per_unit;
            let prepared = pages.prepare(
                ring,
                [depth_desc.width, depth_desc.height],
                &VsmView {
                    world_from_clip,
                    eye: view.camera.position,
                    footprint: 1.0 / pixels.max(1e-9),
                    light_direction: direction,
                    depth_range: extent,
                },
            )?;
            Some(PagesFrame {
                pages,
                draw,
                lists,
                prepared,
            })
        });
        let frame = FrameData {
            clip_from_world: clip_from_world.to_cols_array(),
            light_from_world: fit.light_from_world.to_cols_array(),
            camera_position: view.camera.position.extend(1.0).to_array(),
            to_light: (-view.light.direction.normalize_or(Vec3::NEG_Y))
                .extend(0.0)
                .to_array(),
            light_color: environment
                .and_then(|e| e.sun_color)
                .unwrap_or(view.light.color)
                .extend(0.0)
                .to_array(),
            ambient: view.light.ambient.extend(0.0).to_array(),
            planes: Frustum::from_clip_from_world(view.camera.clip_from_world)
                .planes
                .map(|p| p.to_array()),
            shadow_sampler: Bindless::sampler(SamplerKind::ShadowCompare),
            cascade_sampler: Bindless::sampler(SamplerKind::ShadowCompareRepeat),
            cull: u32::from(self.frustum_culling),
            visibility_capacity: occlusion.map_or(0, Occlusion::capacity),
            hiz_levels: pyramid_layout.levels,
            depth_size: [depth_desc.width, depth_desc.height],
            // Orthographic views (no perspective divide) draw full meshes.
            lod_scale: crate::cull::lod_scale(&unjittered, depth_desc.height, self.lod_threshold),
            slot_count,
            hiz_offsets: pyramid_layout.offsets,
            previous_clip_from_world: previous_clip_from_world.to_cols_array(),
            previous_clip_from_clip: (previous_clip_from_world * unjittered.inverse())
                .to_cols_array(),
            jitter: [jitter_ndc[0], jitter_ndc[1], 0.0, 0.0],
            previous_count,
            scatter_budget: self.subsurface_settings.unwrap_or_default().budget(),
            decal_tiles: decals.map_or(0, |(_, p)| p.occupied()),
            cascades: cascade_data(
                &fit,
                &plans,
                &held,
                self.lod_threshold,
                cache.is_none().then(|| settings.atlas_grid()),
                pieces.map(|p| p.2),
            ),
            cascade_count: fit.count as u32,
            command_count,
            cascade_blend: settings.blend.clamp(0.0, 1.0),
            normal_bias: settings.normal_bias,
            fade_last: u32::from(fit.fade),
            scatter_footprint: {
                let (forward, near_center) =
                    axis.map_or((Vec3::NEG_Z, Vec3::ZERO), |a| (a.forward, a.near_center));
                let f = Footprint::new(unjittered, forward, near_center, depth_desc.height);
                [f.pixels_per_unit, f.w_scale, f.w_offset]
            },
            view_axis: axis.map_or([0.0; 4], |a| {
                a.forward.extend(-a.forward.dot(a.near_center)).to_array()
            }),
            lights: uploaded.map_or(0, |u| u.0),
            zbins: uploaded.map_or(0, |u| u.2),
            light_count: light_layout.count,
            light_words: light_layout.words,
            tiles_x: light_layout.tiles[0],
            bin_scale: light_layout.bin_scale,
            light_culling: u32::from(self.light_culling),
            // New noise every frame only when a temporal resolve averages it.
            scatter_frame: temporal.map_or(0, |t| t.frame.get() as u32),
            sky_turn: environment.map_or([0.0, 1.0], |e| e.background_turn),
            world_from_clip: world_from_clip.to_cols_array(),
            environment: u32::from(environment.is_some()),
            sky_cube: environment.map_or(0, |e| e.background_index),
            prefiltered: environment.map_or(0, |e| e.prefiltered_index),
            dfg_lut: environment.map_or(0, |e| e.dfg_index),
            env_levels: PREFILTERED_LEVELS,
            env_intensity: environment.map_or(1.0, |e| e.intensity),
            linear_sampler: Bindless::sampler(SamplerKind::LinearClamp),
            subsurface_count: if subsurface_draws {
                self.subsurface_profiles.len() as u32
            } else {
                0
            },
            sh: environment.map_or(0, |e| e.sh_address),
            subsurface: profiles,
            atmosphere: atmosphere.map_or(0, |a| a.constants),
            local_shadow_atlas: local.map_or(0, |s| s.atlas.sampled),
            sun_disk: if environment.is_some_and(|e| !e.sun_disk) {
                2.0
            } else {
                crate::environment::SUN_DISK_COS
            },
            light_shadows: local_frame.light_shadows,
            shadow_views: local_frame.views,
            shadow_draws: local_frame.draws,
            shadow_draw_count: shadow_draws,
            shadow_texel: local.map_or(0.0, |s| 1.0 / s.atlas.desc.width.max(1) as f32),
            gi_scale: match gi_view {
                Some(ViewGi::Probes { volume, .. }) => volume.resolve_scale() as f32,
                _ => 1.0,
            },
            gi_samples: match gi_view {
                Some(ViewGi::Probes { volume, .. }) => {
                    let scale = volume.resolve_scale();
                    [
                        depth_desc.width.div_ceil(scale),
                        depth_desc.height.div_ceil(scale),
                    ]
                }
                Some(ViewGi::Screen(_)) => [depth_desc.width, depth_desc.height],
                None => [0; 2],
            },
            material_data: materials.params().0,
            material_stride: materials.params().1,
            cascade_layers: if cache.is_some() { 1.0 } else { 0.0 },
            batch_meshlets: meshlets.map_or(0, |m| m.table),
            meshlets: if meshlets.is_some() || vsm.is_some() {
                geometry.index_address()
            } else {
                0
            },
            meshlet_offset: {
                let mut offsets = [0; 7];
                if let Some(m) = meshlet_draws {
                    // The late phase's commands start a list later.
                    offsets[0] = m.lists(0) as u32;
                    offsets[1] = (m.lists(1) - command_bytes) as u32;
                }
                if let Some(m) = shadow_meshlets {
                    for k in 0..cascade_count {
                        offsets[2 + k as usize] = m.lists(k) as u32;
                    }
                }
                // The pages' lists follow a command list in their buffer; their slots
                // start their own.
                if vsm.is_some() {
                    offsets[VSM_LIST] = command_bytes as u32;
                }
                offsets
            },
            meshlet_slot: {
                let mut slots = [0; 7];
                if let Some(m) = meshlet_draws {
                    let first = phases * slot_count;
                    slots[0] = first;
                    slots[1] = first + m.layout.draws;
                }
                if let Some(m) = shadow_meshlets {
                    for k in 0..cascade_count {
                        slots[2 + k as usize] = cascade_count * slot_count + k * m.layout.draws;
                    }
                }
                slots
            },
            vsm: vsm.map_or(0, |v| v.prepared.frame),
            vsm_table: vsm.map_or(0, |v| v.pages.table_address()),
            vsm_hierarchy: vsm.map_or(0, |v| v.pages.hierarchy_address()),
            vsm_batches: vsm.map_or(0, |v| v.lists.table),
            vsm_pool: vsm.map_or(0, |v| v.pages.pool_index()),
            vsm_lod_threshold: self.lod_threshold,
        };
        let frame = ring.push(&[frame])?;
        let base = Push {
            frame,
            instances,
            batches: batch_table,
            visible: 0,
            commands: 0,
            visibility: occlusion.map_or(0, |o| o.address),
            pyramid: 0,
            positions: geometry.position_address(),
            attributes: geometry.attribute_address(),
            materials: materials.address(),
            previous_instances: match source {
                Source::Ring(_) => temporal.map_or(0, |t| t.address),
                Source::Scene {
                    previous_address, ..
                } => previous_address,
            },
            light_tiles: 0,
            instance_count,
            texture: 0,
            phase: PHASE_ALL,
            ao: NO_AO,
            decal_albedo: 0,
            decal_surface: 0,
            gi: 0,
            _pad: 0,
        };

        let culled = fg.create_buffer(
            "draw commands",
            BufferDesc {
                size: meshlet_draws.map_or(u64::from(phases) * command_bytes, |m| m.lists(phases)),
            },
        );
        let visible_bytes = u64::from(slot_count) * 4;
        let visible = fg.create_buffer(
            "visible instances",
            BufferDesc {
                size: u64::from(phases)
                    * (visible_bytes + meshlet_draws.map_or(0, |m| u64::from(m.layout.draws) * 4)),
            },
        );
        let shadow_commands = fg.create_buffer(
            "shadow commands",
            BufferDesc {
                size: shadow_meshlets.map_or(u64::from(cascade_count) * command_bytes, |m| {
                    m.lists(cascade_count)
                }),
            },
        );
        let casters = fg.create_buffer(
            "shadow casters",
            BufferDesc {
                size: u64::from(cascade_count)
                    * (visible_bytes
                        + shadow_meshlets.map_or(0, |m| {
                            // Their pieces after the slots, when drawn at once.
                            u64::from(m.layout.draws) * if pieces.is_some() { 8 } else { 4 }
                        })),
            },
        );
        // Local lights' shadow views drawn this frame: their command and caster lists,
        // sized for a power of two of views, at least the budget's, so the graph reuses its
        // buffers from frame to frame (the count changes every frame, pieces included).
        let local_lists = (shadow_draws > 0).then(|| {
            let budget = local.map_or(0, |s| s.cache.borrow().settings().updates_per_frame);
            let room = u64::from(shadow_draws.max(budget).next_power_of_two().max(4));
            (
                fg.create_buffer(
                    "local shadow commands",
                    BufferDesc {
                        size: room * command_bytes,
                    },
                ),
                fg.create_buffer(
                    "local shadow casters",
                    BufferDesc {
                        size: room * visible_bytes,
                    },
                ),
            )
        });
        // The pages' casters (ADR 0093 step 3b): a command list (the batches' levels, which
        // their whole draws copy), then their meshlet lists; and their draws' slots.
        let vsm_lists = vsm.map(|v| {
            (
                fg.create_buffer(
                    "vsm commands",
                    BufferDesc {
                        size: command_bytes + v.lists.list_bytes(),
                    },
                ),
                fg.create_buffer(
                    "vsm casters",
                    BufferDesc {
                        size: u64::from(v.lists.draws.max(1)) * 4,
                    },
                ),
            )
        });
        let flags = occlusion.map(|o| {
            // Flags written by the previous frame's late phase, or to be cleared now.
            let stale = o.stale.replace(false);
            let written = BufferState {
                stages: vk::PipelineStageFlags2::COMPUTE_SHADER,
                access: vk::AccessFlags2::SHADER_STORAGE_READ
                    | vk::AccessFlags2::SHADER_STORAGE_WRITE,
            };
            let buffer = fg.import_buffer(
                "visibility flags",
                ImportedBuffer {
                    buffer: o.buffer,
                    size: u64::from(o.capacity.max(1)) * 4,
                    address: o.address,
                    initial: if stale { BufferState::idle() } else { written },
                    final_state: Some(written),
                },
            );
            (buffer, stale)
        });
        let mut setup = fg
            .add_pass(names.cull_setup)
            .write(culled, Access::TransferWrite)
            .write(shadow_commands, Access::TransferWrite);
        if let Some((flags, true)) = flags {
            setup = setup.write(flags, Access::TransferWrite);
        }
        if let Some((local_commands, _)) = local_lists {
            setup = setup.write(local_commands, Access::TransferWrite);
        }
        if let Some((vsm_commands, _)) = vsm_lists {
            setup = setup.write(vsm_commands, Access::TransferWrite);
        }
        setup.execute(move |ctx| {
            let region = vk::BufferCopy {
                src_offset: commands.offset,
                dst_offset: 0,
                size: u64::from(phases) * command_bytes,
            };
            let shadow_region = vk::BufferCopy {
                src_offset: commands.offset + u64::from(phases) * command_bytes,
                dst_offset: 0,
                size: u64::from(cascade_count) * command_bytes,
            };
            // SAFETY: recording outside a rendering scope; the ranges are in bounds.
            unsafe {
                ctx.device
                    .cmd_copy_buffer(ctx.cmd, commands.buffer, ctx.buffer(culled), &[region]);
                if let Some(m) = meshlet_draws {
                    let header = |list: u32| vk::BufferCopy {
                        src_offset: m.layout.header.offset,
                        dst_offset: m.lists(list),
                        size: MESHLET_HEADER_BYTES,
                    };
                    let regions = [header(0), header(1)];
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        m.layout.header.buffer,
                        ctx.buffer(culled),
                        &regions[..phases as usize],
                    );
                }
                ctx.device.cmd_copy_buffer(
                    ctx.cmd,
                    commands.buffer,
                    ctx.buffer(shadow_commands),
                    &[shadow_region],
                );
                if let Some(m) = shadow_meshlets {
                    let header = |list: u32| vk::BufferCopy {
                        src_offset: m.layout.header.offset,
                        dst_offset: m.lists(list),
                        size: MESHLET_HEADER_BYTES,
                    };
                    let regions = [header(0), header(1), header(2), header(3)];
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        m.layout.header.buffer,
                        ctx.buffer(shadow_commands),
                        &regions[..cascade_count as usize],
                    );
                }
                if let Some((flags, true)) = flags {
                    ctx.device
                        .cmd_fill_buffer(ctx.cmd, ctx.buffer(flags), 0, vk::WHOLE_SIZE, 0);
                }
                if let Some((local_commands, _)) = local_lists {
                    let region = vk::BufferCopy {
                        src_offset: commands.offset
                            + u64::from(phases + cascade_count) * command_bytes,
                        dst_offset: 0,
                        size: u64::from(shadow_draws) * command_bytes,
                    };
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        commands.buffer,
                        ctx.buffer(local_commands),
                        &[region],
                    );
                }
                if let Some(((vsm_commands, _), v)) = vsm_lists.zip(vsm) {
                    // The first phase's command list (only the levels' index ranges are
                    // read), then the lists' header.
                    let list = vk::BufferCopy {
                        src_offset: commands.offset,
                        dst_offset: 0,
                        size: command_bytes,
                    };
                    let header = vk::BufferCopy {
                        src_offset: v.lists.header.offset,
                        dst_offset: command_bytes,
                        size: MESHLET_HEADER_BYTES,
                    };
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        commands.buffer,
                        ctx.buffer(vsm_commands),
                        &[list],
                    );
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        v.lists.header.buffer,
                        ctx.buffer(vsm_commands),
                        &[header],
                    );
                }
            }
        });
        let flag_buffer = flags.map(|(buffer, _)| buffer);
        // One thread per instance.
        let cull_threads = Dispatch::Threads([instance_count, 1, 1]);
        let cull = &self.cull;
        let first_phase = if occlusion.is_some() {
            PHASE_EARLY
        } else {
            PHASE_ALL
        };
        let name = if occlusion.is_some() {
            "cull early"
        } else {
            names.cull
        };
        let first_cull = cull_pass(
            fg,
            bindless,
            name,
            culled,
            visible,
            flag_buffer,
            first_phase,
            scene_instances,
        );
        let first_push = move |ctx: &PassContext<'_>| Push {
            visible: ctx.address(visible),
            commands: ctx.address(culled),
            phase: first_phase,
            ..base
        };
        match (meshlet_shaders, meshlet_draws) {
            (Some(shaders), Some(m)) => {
                record_meshlet_cull(
                    first_cull,
                    geometry,
                    culled,
                    shaders,
                    m.lists(0),
                    cull_threads,
                    first_push,
                );
            }
            _ => first_cull.dispatch(cull, cull_threads, move |ctx| Some(first_push(ctx))),
        }

        // Shadow casters of what each cascade draws this frame, then each piece of it drawn
        // into the cascade's layer (ADR 0093): with the view's cache, the strips the
        // camera's movement exposed (nothing at all for a still camera over a still
        // scene); otherwise every cascade whole, into a transient map. A probe's later
        // faces read its first face's map.
        let shared_shadow = capture_shadow.and_then(|c| c.map.get());
        let geometry = *geometry;
        let materials = *materials;
        let shadow_map = match shared_shadow {
            Some(map) => map,
            None => {
                let (count, reach) = (fit.count, fit.reach);
                let layers = cache.map_or(1, |_| settings.cascade_count());
                // Without a cache, the cascades are tiles of one layer, drawn in one rendering
                // (each layer rendered on its own costs the RTX some 4 µs a cascade with
                // meshlets, one layered rendering 18 % on Sponza; ADR 0093).
                let (columns, _) = settings.atlas_grid();
                let shadow_map = match cache {
                    Some(c) => c.import(fg),
                    None => {
                        let (width, height) = settings.atlas_size();
                        fg.create_texture(
                            "shadow map",
                            TextureDesc::new_2d_array(settings.format, width, height, 1),
                        )
                    }
                };
                let drawing = cache.is_none() || plans[..count].iter().any(|p| p.rect_count > 0);
                if drawing {
                    let shadow_cull = fg
                        .add_compute(names.shadow_cull, bindless)
                        .read(
                            shadow_commands,
                            Access::StorageReadWrite(ShaderStages::COMPUTE),
                        )
                        .write(casters, Access::StorageWrite(ShaderStages::COMPUTE))
                        .maybe_read(scene_instances, Access::StorageRead(ShaderStages::COMPUTE));
                    let shadow_push = move |ctx: &PassContext<'_>| Push {
                        visible: ctx.address(casters),
                        commands: ctx.address(shadow_commands),
                        ..base
                    };
                    match (shadow_shaders, shadow_meshlets) {
                        // With meshlets (ADR 0085): the casters, then the meshlet cull over
                        // the work items of each cascade drawn.
                        (Some(shaders), Some(m)) => {
                            shadow_cull
                                .read(shadow_commands, Access::Indirect)
                                .read(geometry.indices, Access::StorageRead(ShaderStages::COMPUTE))
                                .record(move |rec| {
                                    let push = shadow_push(rec.context());
                                    let args = rec.context().buffer(shadow_commands);
                                    rec.dispatch(&shaders.shadow_instances, cull_threads, &push);
                                    rec.barrier_for_indirect();
                                    for k in 0..cascade_count {
                                        if plans[k as usize].rect_count == 0 {
                                            continue;
                                        }
                                        let push = Push {
                                            phase: PHASE_SHADOW_MESHLETS + k,
                                            ..push
                                        };
                                        // Drawn in several pieces at once: its runs get
                                        // their pieces.
                                        let cull = match pieces {
                                            Some((cull, ..))
                                                if plans[k as usize].piece_count() > 1 =>
                                            {
                                                cull
                                            }
                                            _ => &shaders.meshlets,
                                        };
                                        rec.dispatch_indirect(
                                            cull,
                                            args,
                                            m.lists(k) + MESHLET_ARGS,
                                            &push,
                                        );
                                    }
                                });
                        }
                        _ => shadow_cull.dispatch(&self.shadow_cull, cull_threads, move |ctx| {
                            Some(shadow_push(ctx))
                        }),
                    }
                    let shadow_pipelines = &self.shadow;
                    // A cache's cascades render into their layers one at a time (four layers
                    // of one layered rendering cost the RTX 18 % on Sponza): a layer drawn
                    // whole is cleared as its rendering begins, each piece of one drawn in
                    // strips before it is drawn; layers drawn whole all at once need nothing
                    // kept. Without a cache, the atlas is cleared and every tile drawn in one
                    // rendering.
                    let transient = cache.is_none();
                    let discards =
                        transient || (count == layers && plans[..count].iter().all(|p| p.full));
                    let pass = materials
                        .read(geometry.read(fg.add_pass(names.shadow)))
                        .maybe_read(scene_instances, Access::StorageRead(ShaderStages::VERTEX))
                        .read(shadow_commands, Access::Indirect)
                        .read(casters, Access::StorageRead(ShaderStages::VERTEX));
                    let pass = if transient {
                        pass.depth(shadow_map, depth_clear, StoreOp::Store)
                    } else {
                        pass.depth_layers(shadow_map, !discards)
                    };
                    pass.execute(move |ctx| {
                        let push = Push {
                            visible: ctx.address(casters),
                            ..base
                        };
                        let mut drawn_together = 0;
                        for (cascade, plan) in plans[..count].iter().enumerate() {
                            if plan.rect_count == 0 {
                                continue;
                            }
                            let load = if transient || plan.full {
                                depth_clear
                            } else {
                                LoadOp::Load
                            };
                            if !transient {
                                // SAFETY: the pass declared the map's layers; the map has
                                // this one; no rendering is begun.
                                unsafe {
                                    ctx.begin_depth_layer(
                                        shadow_map,
                                        cascade as u32,
                                        load,
                                        StoreOp::Store,
                                    );
                                }
                            }
                            // The vertex shaders project with the cascade's constants.
                            let push = Push {
                                phase: cascade as u32,
                                ..push
                            };
                            // Its tile of the atlas, or its own layer.
                            let tile = if transient {
                                let k = cascade as u32;
                                vk::Offset2D {
                                    x: ((k % columns) * resolution) as i32,
                                    y: ((k / columns) * resolution) as i32,
                                }
                            } else {
                                vk::Offset2D::default()
                            };
                            let area_of = |piece: &Piece| DrawArea {
                                viewport: vk::Rect2D {
                                    offset: vk::Offset2D {
                                        x: tile.x + piece.viewport[0],
                                        y: tile.y + piece.viewport[1],
                                    },
                                    extent: vk::Extent2D {
                                        width: resolution,
                                        height: resolution,
                                    },
                                },
                                scissor: vk::Rect2D {
                                    offset: vk::Offset2D {
                                        x: tile.x + piece.scissor.offset.x,
                                        y: tile.y + piece.scissor.offset.y,
                                    },
                                    extent: piece.scissor.extent,
                                },
                            };
                            let clear = vk::ClearAttachment {
                                aspect_mask: vk::ImageAspectFlags::DEPTH,
                                color_attachment: 0,
                                clear_value: vk::ClearValue {
                                    depth_stencil: vk::ClearDepthStencilValue {
                                        depth: 1.0,
                                        stencil: 0,
                                    },
                                },
                            };
                            let clear_rect = |piece: &Piece| vk::ClearRect {
                                rect: piece.scissor,
                                base_array_layer: 0,
                                layer_count: 1,
                            };
                            // A cache's cascade in several pieces with the meshlet path:
                            // its meshlet draws at once, each run's instances into its
                            // pieces' viewports (a piece drawn on its own draws every
                            // caster any of the cascade's rectangles reaches); the rest
                            // piece by piece.
                            let together = pieces
                                .zip(shadow_meshlets.as_ref())
                                .filter(|_| plan.piece_count() > 1);
                            if let Some(((_, pipelines, _), m)) = together {
                                drawn_together += 1;
                                let storage = held[cascade].map_or([0, 0], |h| h.storage_origin());
                                let whole = vk::Viewport {
                                    x: 0.0,
                                    y: 0.0,
                                    width: resolution as f32,
                                    height: resolution as f32,
                                    min_depth: 0.0,
                                    max_depth: 1.0,
                                };
                                let mut viewports = [whole; MAX_PIECES];
                                let mut scissors = [vk::Rect2D::default(); MAX_PIECES];
                                let mut rects = [vk::ClearRect::default(); MAX_PIECES];
                                let mut cleared = 0;
                                for (slot, piece) in plan.piece_slots(storage) {
                                    let area = area_of(piece);
                                    viewports[slot] = vk::Viewport {
                                        x: area.viewport.offset.x as f32,
                                        y: area.viewport.offset.y as f32,
                                        ..whole
                                    };
                                    scissors[slot] = area.scissor;
                                    rects[cleared] = clear_rect(piece);
                                    cleared += 1;
                                }
                                // SAFETY: recording inside the pass's rendering scope (as
                                // below); the pieces are inside the layer.
                                unsafe {
                                    if load == LoadOp::Load {
                                        ctx.device.cmd_clear_attachments(
                                            ctx.cmd,
                                            &[clear],
                                            &rects[..cleared],
                                        );
                                    }
                                    ctx.device.cmd_set_viewport(ctx.cmd, 0, &viewports);
                                    ctx.device.cmd_set_scissor(ctx.cmd, 0, &scissors);
                                    states.draw_meshlets(
                                        ctx,
                                        bindless,
                                        &geometry,
                                        pipelines,
                                        &push,
                                        true,
                                        DEPTH_STATES,
                                        ctx.buffer(shadow_commands),
                                        m.lists(cascade as u32),
                                        m,
                                        DrawAreas::Set,
                                    );
                                }
                            }
                            for piece in plan.rects().iter().flat_map(|r| r.pieces()) {
                                let area = area_of(piece);
                                // SAFETY: recording inside the pass's rendering scope; the
                                // pass reads the commands (indirect), the caster lists,
                                // the pool and the material table; the piece is inside
                                // the layer.
                                unsafe {
                                    if load == LoadOp::Load && together.is_none() {
                                        ctx.device.cmd_clear_attachments(
                                            ctx.cmd,
                                            &[clear],
                                            &[clear_rect(piece)],
                                        );
                                    }
                                    states.draw(
                                        ctx,
                                        bindless,
                                        &geometry,
                                        [&shadow_pipelines[0], &shadow_pipelines[1]],
                                        // Material types cast as their fallbacks (ADR 0092).
                                        &TypedPipelines::BUILTIN,
                                        &push,
                                        true,
                                        DEPTH_STATES,
                                        ctx.buffer(shadow_commands),
                                        cascade as u64 * command_bytes,
                                        area,
                                    );
                                    if let Some(m) =
                                        shadow_meshlets.as_ref().filter(|_| together.is_none())
                                    {
                                        states.draw_meshlets(
                                            ctx,
                                            bindless,
                                            &geometry,
                                            [&shadow_pipelines[0], &shadow_pipelines[1]],
                                            &push,
                                            true,
                                            DEPTH_STATES,
                                            ctx.buffer(shadow_commands),
                                            m.lists(cascade as u32),
                                            m,
                                            area,
                                        );
                                    }
                                }
                            }
                            if !transient {
                                // SAFETY: its rendering was begun above.
                                unsafe { ctx.end_rendering() };
                            }
                        }
                        // The layers hold what was drawn once it is recorded: a frame
                        // that is never recorded leaves the cache as it was.
                        if let Some(c) = cache {
                            c.commit(&plans, held, count, reach, drawn_together);
                        }
                    });
                } else if let Some(c) = cache {
                    c.idle();
                }
                if let Some(c) = capture_shadow {
                    c.map.set(Some(shadow_map));
                }
                shadow_map
            }
        };
        // Local lights' shadow views (ADR 0071): culled per view, then each drawn into its
        // tile of the atlas (cleared first); the rest of the atlas keeps its contents.
        if let (Some(state), Some((local_commands, local_casters)), Some(atlas)) =
            (local, local_lists, local_frame.atlas)
        {
            let pipelines = &state.pipelines;
            fg.add_compute("local shadow cull", bindless)
                .read(
                    local_commands,
                    Access::StorageReadWrite(ShaderStages::COMPUTE),
                )
                .write(local_casters, Access::StorageWrite(ShaderStages::COMPUTE))
                .maybe_read(scene_instances, Access::StorageRead(ShaderStages::COMPUTE))
                .dispatch(
                    &pipelines.cull,
                    Dispatch::Threads([instance_count, shadow_draws, 1]),
                    move |ctx| {
                        Some(Push {
                            visible: ctx.address(local_casters),
                            commands: ctx.address(local_commands),
                            ..base
                        })
                    },
                );
            let rects = &state.rects;
            let range = local_frame.rects.clone();
            materials
                .read(geometry.read(fg.add_pass("local shadows")))
                .maybe_read(scene_instances, Access::StorageRead(ShaderStages::VERTEX))
                .read(local_commands, Access::Indirect)
                .read(local_casters, Access::StorageRead(ShaderStages::VERTEX))
                .depth(atlas, LoadOp::Load, StoreOp::Store)
                .execute(move |ctx| {
                    let rects = rects.borrow();
                    let push = Push {
                        visible: ctx.address(local_casters),
                        ..base
                    };
                    for (k, rect) in rects[range].iter().enumerate() {
                        let clear = vk::ClearAttachment {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            color_attachment: 0,
                            clear_value: vk::ClearValue {
                                depth_stencil: vk::ClearDepthStencilValue {
                                    depth: 1.0,
                                    stencil: 0,
                                },
                            },
                        };
                        // SAFETY: recording inside the pass's rendering scope; the pass
                        // reads the commands (indirect), the caster lists, the pool and the
                        // material table.
                        unsafe {
                            ctx.device.cmd_clear_attachments(
                                ctx.cmd,
                                &[clear],
                                &[vk::ClearRect {
                                    rect: *rect,
                                    base_array_layer: 0,
                                    layer_count: 1,
                                }],
                            );
                            states.draw(
                                ctx,
                                bindless,
                                &geometry,
                                [&pipelines.depth[0], &pipelines.depth[1]],
                                &TypedPipelines::BUILTIN,
                                &Push {
                                    phase: k as u32,
                                    ..push
                                },
                                true,
                                DEPTH_STATES,
                                ctx.buffer(local_commands),
                                k as u64 * command_bytes,
                                *rect,
                            );
                        }
                    }
                });
        }
        // Motion vectors: a second target, last frame's transforms read by the vertex
        // shader, and this frame's copied for the next one afterwards.
        let velocity = temporal.map(|_| fg.create_texture("motion vectors", multi(MOTION_FORMAT)));
        const COPIED: BufferState = BufferState {
            stages: vk::PipelineStageFlags2::COPY,
            access: vk::AccessFlags2::TRANSFER_WRITE,
        };
        let previous = temporal.map(|t| {
            // The scene keeps last frame's instances itself.
            if let Source::Scene { previous, .. } = source {
                return previous;
            }
            let buffer = fg.import_buffer(
                "previous instances",
                ImportedBuffer {
                    buffer: t.buffer,
                    size: u64::from(t.capacity.max(1)) * size_of::<InstanceData>() as u64,
                    address: t.address,
                    initial: t.state.get(),
                    final_state: Some(COPIED),
                },
            );
            t.state.set(COPIED);
            buffer
        });
        // The tiles' light bitmasks, from the lights' rectangles.
        let light_tiles = match uploaded {
            Some((_, rects, _)) if self.light_culling => {
                let words = light_layout.mask_words();
                let lights = light_layout.count;
                let buffer = fg.create_buffer("light tiles", BufferDesc { size: words * 4 });
                fg.add_pass(names.light_tiles_clear)
                    .write(buffer, Access::TransferWrite)
                    .execute(move |ctx| {
                        // SAFETY: recording outside a rendering scope.
                        unsafe {
                            ctx.device.cmd_fill_buffer(
                                ctx.cmd,
                                ctx.buffer(buffer),
                                0,
                                vk::WHOLE_SIZE,
                                0,
                            );
                        }
                    });
                // One workgroup per light: it sets the light's bit over its rectangle.
                fg.add_compute(names.light_tiles, bindless)
                    .read(buffer, Access::StorageReadWrite(ShaderStages::COMPUTE))
                    .dispatch(
                        &self.light_tiles,
                        Dispatch::Groups([lights, 1, 1]),
                        move |ctx| {
                            Some(Push {
                                visible: rects,
                                light_tiles: ctx.address(buffer),
                                ..base
                            })
                        },
                    );
                Some(buffer)
            }
            _ => None,
        };
        let mut targets = Targets {
            color,
            color_load: load,
            velocity,
            previous,
            light_tiles,
            environment,
            ao: None,
            local_atlas: local_frame.atlas.filter(|_| local_frame.shadowed),
            contact: None,
            vsm: None,
            decals: None,
            gi: None,
            reflect: None,
            resolves: None,
        };

        let ao_frame = if temporal.is_some() {
            let frame = self.ao_frame.get().wrapping_add(1);
            self.ao_frame.set(frame);
            frame
        } else {
            // Without a temporal resolve changing noise would flicker.
            0
        };
        let contact_view = ContactView {
            clip_from_world,
            to_light: -view.light.direction.normalize_or(Vec3::NEG_Y),
        };
        // Reflections need it too (ADR 0073): their inputs are written by the shading
        // after it.
        targets.reflect = reflecting.then(|| {
            (
                fg.create_texture("reflection normals", multi(NORMAL_ROUGHNESS_FORMAT)),
                fg.create_texture("reflection weights", multi(WEIGHT_FORMAT)),
            )
        });
        // With MSAA: the lit image screen-space reflections trace (resolved by the last
        // pass that writes the color before them), and their inputs, resolved.
        let scattering = subsurface.is_some() && subsurface_draws;
        let ms_resolves = ms.map(|_| MsResolves {
            lit: (reflecting && view.reflections.ssr.is_some())
                .then(|| fg.create_texture("lit resolved", single(self.color_format))),
            reflect: targets.reflect.map(|_| {
                [
                    fg.create_texture(
                        "reflection normals resolved",
                        single(NORMAL_ROUGHNESS_FORMAT),
                    ),
                    fg.create_texture("reflection weights resolved", single(WEIGHT_FORMAT)),
                    fg.create_texture("reflection far normals", single(NORMAL_ROUGHNESS_FORMAT)),
                    fg.create_texture("reflection far weights", single(WEIGHT_FORMAT)),
                    fg.create_texture("reflection far depth", single(msaa::FAR_DEPTH_FORMAT)),
                ]
            }),
        });
        targets.resolves = ms_resolves.map(|r| MsResolves {
            // The scatter pass resolves the lit image when subsurface materials draw.
            lit: r.lit.filter(|_| !scattering),
            ..r
        });
        // With a prepass, the prepass writes the motion vectors and the shading after it
        // draws without them (the still variants' registers: on the Radeon its vertex
        // stage takes 49 VGPRs instead of 78).
        let prepass_motion = prepass && velocity.is_some();
        if prepass_motion {
            targets.velocity = None;
            targets.previous = None;
        }
        let prepass_pipelines = match (ms, prepass_motion) {
            (Some(m), false) => [&m.prepass[0], &m.prepass[1]],
            (Some(m), true) => [&m.prepass_motion[0], &m.prepass_motion[1]],
            (None, false) => [&self.prepass[0], &self.prepass[1]],
            (None, true) => [&self.prepass_motion[0], &self.prepass_motion[1]],
        };
        // What the prepass phases attach for motion vectors: the target (cleared by the
        // first phase) and last frame's instances.
        let prepass_velocity = velocity.zip(previous).filter(|_| prepass_motion);
        // After the prepass every state shades with the equal-depth pipeline: masked
        // materials were alpha-tested there, and nothing is left to discard.
        let variant = usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
        // With shadowed local lights in reach, the shading that looks their shadows up
        // (ADR 0071; only then, so other views carry no code for it).
        let shadowed = local.filter(|_| local_frame.shadowed).map(|s| &s.pipelines);
        let shading = match shadowed {
            Some(p) => &p.shading[shadowed_index(variant)],
            None => &self.shading[variant],
        };
        // With ambient occlusion, the shading that reads it (the prepass is on then).
        let ao_shading = ao.map(|(_, passes)| match shadowed.and_then(|p| p.ao.as_ref()) {
            Some(p) => &p[shadowed_index(variant)],
            None => &passes.shading[variant],
        });
        // With contact shadows (the prepass is on then), the shading that reads their mask.
        let contact_shading = contact.and_then(|(_, c)| {
            let k = shadowed_index(variant);
            match (ao.is_some(), shadowed.is_some()) {
                (false, false) => Some(&c.shading[variant]),
                (true, false) => c.ao.as_ref().map(|p| &p[variant]),
                (false, true) => c.shadowed.as_ref().map(|p| &p[k]),
                (true, true) => c.shadowed_ao.as_ref().map(|p| &p[k]),
            }
        });
        // With reflections, the shading that also writes their inputs.
        let reflect_shading = reflection_passes.filter(|_| reflecting).and_then(|r| {
            r.pipeline(
                variant & 1 != 0,
                ao.is_some(),
                contact.is_some(),
                shadowed.is_some(),
            )
        });
        // With decals (the prepass is on then), the shading that also blends their buffer
        // in; without its pipeline (not described), the view draws without decals.
        let decal_shading = decals.and_then(|(d, _)| {
            d.pipeline(
                variant,
                reflect_shading.is_some(),
                ao.is_some(),
                contact.is_some(),
                shadowed.is_some(),
            )
        });
        let decals = decals.filter(|_| decal_shading.is_some());
        // With probe GI, the shading that reads its irradiance; without its pipeline (not
        // described), the view draws with the sky's light.
        let gi_shading = gi_view.and_then(|_| {
            self.gi.as_deref()?.pipeline(
                variant,
                reflect_shading.is_some(),
                decals.is_some(),
                ao.is_some(),
                contact.is_some(),
                shadowed.is_some(),
            )
        });
        let gi_view = gi_view.filter(|_| gi_shading.is_some());
        // What makes the GI shading's irradiance after the prepass.
        let gi_resolve = GiResolve {
            view: gi_view,
            sky: gi_sky,
            depth: read_depth,
            size: [depth_desc.width, depth_desc.height],
            clip_from_world,
            screen: ScreenGiInput {
                depth: read_depth,
                motion: prepass_velocity.map(|(velocity, _)| velocity),
                camera: Camera {
                    clip_from_world: unjittered,
                    position: view.camera.position,
                },
                previous_clip_from_world,
                environment: environment.map(|e| (e.source, e.intensity)),
                reset,
            },
        };
        let main_pipelines = if let Some(m) = ms {
            let k = shadowed_index(variant);
            let ms_shadowed = m.shadowed.as_ref().filter(|_| shadowed.is_some());
            let p: &LazyPipeline = if reflect_shading.is_some() {
                m.reflect_pipeline(
                    variant & 1 != 0,
                    ao.is_some(),
                    contact.is_some(),
                    shadowed.is_some(),
                )?
            } else if contact.is_some() {
                let c = m.contact.as_ref()?;
                match (ao.is_some(), shadowed.is_some()) {
                    (false, false) => &c.shading[variant],
                    (true, false) => &c.ao.as_ref()?[variant],
                    (false, true) => &c.shadowed.as_ref()?[k],
                    (true, true) => &c.shadowed_ao.as_ref()?[k],
                }
            } else if ao.is_some() {
                match ms_shadowed {
                    Some(s) => &s.ao.as_ref()?[k],
                    None => &m.ao.as_ref()?[variant],
                }
            } else {
                match ms_shadowed {
                    Some(s) => &s.shading[k].shade,
                    None => &m.shading[variant].shade,
                }
            };
            [p, p]
        } else {
            match (contact_shading, prepass, velocity.is_some()) {
                _ if gi_shading.is_some() => {
                    let p = gi_shading?;
                    [p, p]
                }
                _ if decal_shading.is_some() => {
                    let p = decal_shading?;
                    [p, p]
                }
                _ if reflect_shading.is_some() => {
                    let p = reflect_shading?;
                    [p, p]
                }
                (Some(p), _, _) => [p, p],
                // After the prepass (which wrote the motion vectors).
                (None, true, _) => match ao_shading {
                    Some(p) => [p, p],
                    None => [&shading.shade, &shading.shade],
                },
                (None, false, false) => [&shading.main[0], &shading.main[1]],
                (None, false, true) => [&shading.main_motion[0], &shading.main_motion[1]],
            }
        };
        // With ambient occlusion's bent normals, the variants that read them (ADR 0058).
        let bent = ao
            .as_ref()
            .is_some_and(|((settings, _), _)| settings.bent_normals);
        let plain_pipelines = main_pipelines;
        let main_pipelines = if bent {
            main_pipelines.map(|p| self.bent_twins.twin(p))
        } else {
            main_pipelines
        };
        // Material types' variants of them (ADR 0092); a type without its bent-normal
        // variant (still compiling, or not in its pack) draws its plain one.
        let main_typed = self.typed_pipelines_or(
            &states,
            main_pipelines,
            bent.then_some(plain_pipelines),
            PLAIN_STATES,
        );
        // Made now if this is their first frame (ADR 0079).
        let main_pipelines = [ready(main_pipelines[0])?, ready(main_pipelines[1])?];
        // `contact`: the sun's contact shadows, whose mask's index the shading that reads
        // it takes in `phase` (ADR 0071); `decals`: the decal buffer (ADR 0082).
        // `types`: material types draw with their pipelines (the shading passes); otherwise
        // with the engine's, as glTF (the depth prepass and the subsurface pass).
        let draw_phase = move |ctx: &mut crate::graph::PassContext<'_>,
                               pipelines: [&Pipeline; 2],
                               types: bool,
                               phase: u32,
                               mask: u32,
                               ao: Option<Texture>,
                               contact: Option<Texture>,
                               decals: Option<DecalTargets>,
                               gi: Option<Texture>| {
            let push = Push {
                visible: ctx.address(visible),
                texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                light_tiles: light_tiles.map_or(0, |b| ctx.address(b)),
                ao: ao.and_then(|t| ctx.sampled_index(t)).unwrap_or(NO_AO),
                phase: contact
                    .and_then(|t| ctx.sampled_index(t))
                    .unwrap_or(base.phase),
                decal_albedo: decals
                    .and_then(|d| ctx.sampled_index(d.albedo))
                    .unwrap_or(0),
                decal_surface: decals
                    .and_then(|d| ctx.sampled_index(d.surface))
                    .unwrap_or(0),
                gi: gi.and_then(|t| ctx.sampled_index(t)).unwrap_or(0),
                ..base
            };
            let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
            // SAFETY: recording inside the pass's rendering scope; the pass reads the pool
            // and the material table.
            unsafe {
                states.draw(
                    ctx,
                    bindless,
                    &geometry,
                    pipelines,
                    if types {
                        &main_typed
                    } else {
                        &TypedPipelines::BUILTIN
                    },
                    &push,
                    false,
                    mask,
                    ctx.buffer(culled),
                    offset,
                    ctx.render_area(),
                );
                if let Some(m) = &meshlet_draws {
                    states.draw_meshlets(
                        ctx,
                        bindless,
                        &geometry,
                        pipelines,
                        &push,
                        false,
                        mask,
                        ctx.buffer(culled),
                        m.lists(u32::from(phase == PHASE_LATE)),
                        m,
                        ctx.render_area(),
                    );
                }
            }
        };
        // The depth is needed after the frame's passes for motion vectors and upsampling.
        let depth_store = if velocity.is_some() || environment.is_some() {
            StoreOp::Store
        } else {
            StoreOp::DontCare
        };
        let draw_lists = DrawLists {
            push: base,
            commands: culled,
            visible,
            instances: scene_instances,
            phases,
            command_bytes,
            states,
            meshlets: meshlet_draws,
        };
        let mut passes = match flag_buffer {
            None => {
                let depth_use = if prepass {
                    let mut pass = prepass_pass(
                        fg,
                        names.depth_prepass,
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, depth_clear, StoreOp::Store);
                    if let Some(resolved) = resolved_depth {
                        pass = pass.resolve(resolved, nearest);
                    }
                    if let Some((velocity, previous)) = prepass_velocity {
                        pass = pass
                            .color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                            .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                    }
                    pass.execute(move |ctx| {
                        draw_phase(
                            ctx,
                            prepass_pipelines,
                            false,
                            PHASE_ALL,
                            DEPTH_STATES,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                    DepthUse::ReadOnly
                } else {
                    DepthUse::Write(depth_clear, depth_store)
                };
                if let Some(((settings, view_space), passes)) = &ao {
                    targets.ao = Some(
                        passes
                            .passes
                            .add_passes(fg, bindless, read_depth, view_space, settings, ao_frame),
                    );
                }
                targets.vsm = vsm.zip(vsm_lists).map(|(v, lists)| {
                    v.add_passes(
                        fg,
                        bindless,
                        &geometry,
                        &materials,
                        states,
                        lists,
                        base,
                        scene_instances,
                        instance_count,
                        command_bytes,
                        read_depth,
                    )
                });
                if let Some(((settings, _), state)) = contact {
                    targets.contact = Some(state.passes.add_passes(
                        fg,
                        bindless,
                        read_depth,
                        &contact_view,
                        &settings,
                    ));
                }
                if let Some((d, prepared)) = &decals {
                    targets.decals =
                        Some(d.passes.add_prepared(fg, bindless, read_depth, prepared));
                }
                targets.gi = gi_resolve.add(fg, ring, bindless);
                let ao_texture = targets.ao;
                let contact_texture = targets.contact;
                let decal_texture = targets.decals;
                let gi_texture = targets.gi;
                shading_pass(
                    fg,
                    names.forward,
                    &geometry,
                    &materials,
                    shadow_map,
                    culled,
                    visible,
                    targets,
                    true,
                    depth,
                    depth_use,
                    scene_instances,
                )
                .execute(move |ctx| {
                    draw_phase(
                        ctx,
                        main_pipelines,
                        true,
                        PHASE_ALL,
                        PLAIN_STATES,
                        ao_texture,
                        contact_texture,
                        decal_texture,
                        gi_texture,
                    )
                });
                ForwardPasses {
                    instances: instance_count,
                    batches: batch_count,
                    draw_commands: drawn_commands,
                    list_commands: command_count,
                    occlusion: false,
                    commands: Some(culled),
                    motion: None,
                    ao: targets.ao,
                    atmosphere: atmosphere.and_then(|a| a.textures),
                    contact_shadows: targets.contact,
                    virtual_shadows: targets.vsm,
                    decals: targets.decals,
                    gi: targets.gi,
                    reflections: None,
                    resolved_depth,
                    draws: Some(draw_lists),
                    meshlets: meshlet_lists,
                }
            }
            Some(flags) => {
                // Two phases: draw what was visible, build the pyramid from its depth, then
                // test everything against it and draw what the first phase missed. With the
                // prepass the phases draw depth only, and the forward pass shades both
                // lists once.
                if prepass {
                    let mut pass = prepass_pass(
                        fg,
                        "depth prepass early",
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, depth_clear, StoreOp::Store);
                    // The pyramid keeps the farthest depth: conservative for culling.
                    if let Some(resolved) = resolved_depth {
                        pass = pass.resolve(resolved, farthest);
                    }
                    if let Some((velocity, previous)) = prepass_velocity {
                        pass = pass
                            .color(velocity, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                            .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                    }
                    pass.execute(move |ctx| {
                        draw_phase(
                            ctx,
                            prepass_pipelines,
                            false,
                            PHASE_EARLY,
                            DEPTH_STATES,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                } else {
                    shading_pass(
                        fg,
                        "forward early",
                        &geometry,
                        &materials,
                        shadow_map,
                        culled,
                        visible,
                        targets,
                        true,
                        depth,
                        DepthUse::Write(depth_clear, StoreOp::Store),
                        scene_instances,
                    )
                    .execute(move |ctx| {
                        draw_phase(
                            ctx,
                            main_pipelines,
                            true,
                            PHASE_EARLY,
                            PLAIN_STATES,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                }
                let pyramid = fg.create_buffer(
                    "depth pyramid",
                    BufferDesc {
                        size: pyramid_layout.bytes(),
                    },
                );
                self.pyramid.add_pass(fg, bindless, read_depth, pyramid);
                let late_cull = cull_pass(
                    fg,
                    bindless,
                    "cull late",
                    culled,
                    visible,
                    Some(flags),
                    PHASE_LATE,
                    scene_instances,
                )
                .read(pyramid, Access::StorageRead(ShaderStages::COMPUTE));
                let late_push = move |ctx: &PassContext<'_>| Push {
                    visible: ctx.address(visible) + visible_bytes,
                    commands: ctx.address(culled) + command_bytes,
                    pyramid: ctx.address(pyramid),
                    phase: PHASE_LATE,
                    ..base
                };
                match (meshlet_shaders, meshlet_draws) {
                    (Some(shaders), Some(m)) => record_meshlet_cull(
                        late_cull,
                        &geometry,
                        culled,
                        shaders,
                        m.lists(1),
                        cull_threads,
                        late_push,
                    ),
                    _ => late_cull.dispatch(cull, cull_threads, move |ctx| Some(late_push(ctx))),
                }
                if prepass {
                    let mut pass = prepass_pass(
                        fg,
                        "depth prepass late",
                        &geometry,
                        &materials,
                        culled,
                        visible,
                        scene_instances,
                    )
                    .depth(depth, LoadOp::Load, StoreOp::Store);
                    if let Some(resolved) = resolved_depth {
                        pass = pass.resolve(resolved, nearest);
                    }
                    if let Some((velocity, previous)) = prepass_velocity {
                        pass = pass
                            .color(velocity, LoadOp::Load, StoreOp::Store)
                            .read(previous, Access::StorageRead(ShaderStages::VERTEX));
                    }
                    pass.execute(move |ctx| {
                        draw_phase(
                            ctx,
                            prepass_pipelines,
                            false,
                            PHASE_LATE,
                            DEPTH_STATES,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                    if let Some(((settings, view_space), passes)) = &ao {
                        targets.ao =
                            Some(passes.passes.add_passes(
                                fg, bindless, read_depth, view_space, settings, ao_frame,
                            ));
                    }
                    targets.vsm = vsm.zip(vsm_lists).map(|(v, lists)| {
                        v.add_passes(
                            fg,
                            bindless,
                            &geometry,
                            &materials,
                            states,
                            lists,
                            base,
                            scene_instances,
                            instance_count,
                            command_bytes,
                            read_depth,
                        )
                    });
                    if let Some(((settings, _), state)) = contact {
                        targets.contact = Some(state.passes.add_passes(
                            fg,
                            bindless,
                            read_depth,
                            &contact_view,
                            &settings,
                        ));
                    }
                    if let Some((d, prepared)) = &decals {
                        targets.decals =
                            Some(d.passes.add_prepared(fg, bindless, read_depth, prepared));
                    }
                    targets.gi = gi_resolve.add(fg, ring, bindless);
                    let ao_texture = targets.ao;
                    let contact_texture = targets.contact;
                    let decal_texture = targets.decals;
                    let gi_texture = targets.gi;
                    shading_pass(
                        fg,
                        names.forward,
                        &geometry,
                        &materials,
                        shadow_map,
                        culled,
                        visible,
                        targets,
                        true,
                        depth,
                        DepthUse::ReadOnly,
                        scene_instances,
                    )
                    .execute(move |ctx| {
                        draw_phase(
                            ctx,
                            main_pipelines,
                            true,
                            PHASE_EARLY,
                            PLAIN_STATES,
                            ao_texture,
                            contact_texture,
                            decal_texture,
                            gi_texture,
                        );
                        draw_phase(
                            ctx,
                            main_pipelines,
                            true,
                            PHASE_LATE,
                            PLAIN_STATES,
                            ao_texture,
                            contact_texture,
                            decal_texture,
                            gi_texture,
                        );
                    });
                } else {
                    shading_pass(
                        fg,
                        "forward late",
                        &geometry,
                        &materials,
                        shadow_map,
                        culled,
                        visible,
                        targets,
                        false,
                        depth,
                        DepthUse::Write(LoadOp::Load, depth_store),
                        scene_instances,
                    )
                    .execute(move |ctx| {
                        draw_phase(
                            ctx,
                            main_pipelines,
                            true,
                            PHASE_LATE,
                            PLAIN_STATES,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                }
                ForwardPasses {
                    instances: instance_count,
                    batches: batch_count,
                    draw_commands: drawn_commands,
                    list_commands: command_count,
                    occlusion: true,
                    commands: Some(culled),
                    motion: None,
                    ao: targets.ao,
                    atmosphere: atmosphere.and_then(|a| a.textures),
                    contact_shadows: targets.contact,
                    virtual_shadows: targets.vsm,
                    decals: targets.decals,
                    gi: targets.gi,
                    reflections: None,
                    resolved_depth,
                    draws: Some(draw_lists),
                    meshlets: meshlet_lists,
                }
            }
        };
        // Subsurface materials (ADR 0059): shaded after the others, also writing their
        // diffuse light and albedo, then scattered into the color target.
        if let Some(pipelines) = subsurface.filter(|_| subsurface_draws) {
            let variant =
                usize::from(light_layout.count > 0) | usize::from(environment.is_some()) << 1;
            let irradiance = fg.create_texture("subsurface irradiance", multi(IRRADIANCE_FORMAT));
            let albedo = fg.create_texture("subsurface albedo", multi(ALBEDO_FORMAT));
            // With MSAA the scatter pass reads them resolved (averaged).
            let resolved = ms.map(|_| {
                (
                    fg.create_texture("subsurface irradiance resolved", single(IRRADIANCE_FORMAT)),
                    fg.create_texture("subsurface albedo resolved", single(ALBEDO_FORMAT)),
                )
            });
            // (After the prepass, which wrote their motion vectors.)
            let ms_subsurface = ms.and_then(|m| m.subsurface.as_ref());
            let ms_shadowed = ms
                .and_then(|m| m.shadowed.as_ref())
                .filter(|_| shadowed.is_some())
                .and_then(|p| p.subsurface.as_ref());
            let gi_subsurface =
                gi_view.and_then(|_| self.gi.as_deref()?.subsurface(variant, shadowed.is_some()));
            let pipeline = match (ms, shadowed.and_then(|p| p.subsurface.as_ref())) {
                _ if gi_subsurface.is_some() => gi_subsurface?,
                (Some(_), _) => match ms_shadowed {
                    Some(p) => &p[shadowed_index(variant)],
                    None => &ms_subsurface?.0[variant],
                },
                (None, Some(p)) => &p[shadowed_index(variant)],
                (None, None) => &pipelines.shading[variant],
            };
            let pipeline = ready(if bent {
                self.bent_twins.twin(pipeline)
            } else {
                pipeline
            })?;
            let two_phases = flag_buffer.is_some();
            // Their diffuse light is occluded by the view's ambient occlusion too, and
            // comes from the probe volume with GI.
            let ao_texture = targets.ao;
            let gi_texture = targets.gi;
            // They keep the sky's reflections (their pass writes no reflection inputs).
            let targets = Targets {
                reflect: None,
                resolves: None,
                decals: None,
                ..targets
            };
            shading_pass(
                fg,
                names.forward_subsurface,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                targets,
                false,
                depth,
                DepthUse::ReadOnly,
                scene_instances,
            )
            .color(irradiance, LoadOp::Clear([0.0; 4]), StoreOp::Store)
            .color(albedo, LoadOp::DontCare, StoreOp::Store)
            .execute(move |ctx| {
                let pipelines = [pipeline, pipeline];
                if two_phases {
                    draw_phase(
                        ctx,
                        pipelines,
                        false,
                        PHASE_EARLY,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                        None,
                        gi_texture,
                    );
                    draw_phase(
                        ctx,
                        pipelines,
                        false,
                        PHASE_LATE,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                        None,
                        gi_texture,
                    );
                } else {
                    draw_phase(
                        ctx,
                        pipelines,
                        false,
                        PHASE_ALL,
                        SUBSURFACE_STATES,
                        ao_texture,
                        None,
                        None,
                        gi_texture,
                    );
                }
            });
            // With MSAA: their nearest sample's light and albedo (depth and profile in
            // alpha must stay one sample's).
            if let (Some(m), Some((irradiance_1x, albedo_1x))) = (ms_subsurface, resolved) {
                msaa::add_nearest_resolve(
                    fg,
                    bindless,
                    "subsurface resolve",
                    &m.2,
                    [irradiance, albedo],
                    None,
                    &[irradiance_1x, albedo_1x],
                );
            }
            // The scatter pass: the same batches, adding where their depth is.
            let scatter = ready(match ms_subsurface {
                Some((_, scatter, _)) => scatter,
                None => &pipelines.scatter,
            })?;
            // With MSAA the neighbours' light comes from the resolve, the fragment's own
            // light and albedo from its samples (`scatterFragmentMs`).
            let (light, albedo, own_light) = match resolved {
                Some((light, _)) => (light, albedo, Some(irradiance)),
                None => (irradiance, albedo, None),
            };
            let lit = ms_resolves.and_then(|r| r.lit);
            let mut pass = forward_pass(
                fg,
                names.subsurface_scatter,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                scene_instances,
            )
            .read(light, Access::Sampled(ShaderStages::FRAGMENT))
            .read(albedo, Access::Sampled(ShaderStages::FRAGMENT))
            .color(color, LoadOp::Load, StoreOp::Store);
            if let Some(own) = own_light {
                pass = pass.read(own, Access::Sampled(ShaderStages::FRAGMENT));
            }
            if let Some(lit) = lit {
                pass = pass.resolve(lit, ResolveMode::Average);
            }
            pass.depth_read_only(depth).execute(move |ctx| {
                let (Some(light), Some(albedo)) =
                    (ctx.sampled_index(light), ctx.sampled_index(albedo))
                else {
                    return;
                };
                let push = Push {
                    visible: ctx.address(visible),
                    texture: light,
                    phase: albedo,
                    ao: own_light
                        .and_then(|t| ctx.sampled_index(t))
                        .unwrap_or(base.ao),
                    ..base
                };
                let phases: &[u32] = if two_phases {
                    &[PHASE_EARLY, PHASE_LATE]
                } else {
                    &[PHASE_ALL]
                };
                for &phase in phases {
                    let offset = u64::from(phase.saturating_sub(1)) * command_bytes;
                    // SAFETY: recording inside the pass's rendering scope; the pass reads
                    // the pool, the material table and the commands.
                    unsafe {
                        states.draw(
                            ctx,
                            bindless,
                            &geometry,
                            [scatter, scatter],
                            &TypedPipelines::BUILTIN,
                            &push,
                            false,
                            SUBSURFACE_STATES,
                            ctx.buffer(culled),
                            offset,
                            ctx.render_area(),
                        );
                    }
                }
            });
        }
        // Reflections (ADR 0073): screen-space reflections and probes replace the sky's
        // specular light on the opaque image, before the atmosphere and the transparent
        // surfaces.
        if let (Some(r), Some(prepared), Some(env), Some((normal_roughness, weight))) =
            (reflection_passes, prepared, environment, targets.reflect)
        {
            if let Some((probes, _)) = imported.probes.filter(|_| prepared.probes > 0) {
                probes.mark_used(true, false);
            }
            // With MSAA: the nearest sample's inputs, the multisampled color as the
            // composite's target.
            let (normal_roughness, weight) = match (
                ms_resolves.and_then(|r| r.reflect),
                ms.and_then(|m| m.reflect.as_deref()),
            ) {
                (Some(resolved), Some(r)) => {
                    msaa::add_nearest_resolve(
                        fg,
                        bindless,
                        "reflection inputs resolve",
                        &r.2,
                        [normal_roughness, weight],
                        Some(depth),
                        &resolved,
                    );
                    (resolved[0], resolved[1])
                }
                _ => (normal_roughness, weight),
            };
            let lit = ms_resolves.and_then(|r| r.lit).unwrap_or(color);
            let composite = ms
                .and_then(|m| m.reflect.as_deref())
                .zip(ms_resolves)
                .and_then(|(r, resolves)| {
                    let [_, _, far_nr, far_weight, far_depth] = resolves.reflect?;
                    Some(MsComposite {
                        pipelines: &r.1,
                        depth,
                        samples: samples.as_raw(),
                        far: [far_depth, far_nr, far_weight],
                    })
                });
            let ssr = r.passes.add_view_passes(
                fg,
                bindless,
                &prepared,
                &ViewInputs {
                    color,
                    lit,
                    depth: read_depth,
                    normal_roughness,
                    weight,
                    environment: env,
                    probes: imported.probes.map(|(_, t)| t),
                },
                composite,
            );
            passes.reflections = Some(crate::reflections::ReflectionOutput {
                normal_roughness,
                weight,
                ssr,
            });
        }
        // With an atmosphere or fog: the sky, aerial perspective and fog over the opaque
        // image (ADR 0069). A probe's capture fills its sky in its face copy instead.
        if capture {
        } else if let (Some(env), Some(atmosphere)) = (environment, atmosphere) {
            // With MSAA per sample, from each sample's depth.
            crate::atmosphere::add_composite_pass(
                fg,
                bindless,
                ready(ms.map_or(&self.composite, |m| &m.composite))?,
                color,
                depth,
                &env,
                &atmosphere,
            );
        } else if let Some(env) = environment {
            // The sky where no geometry was drawn (depth still 1).
            let sky = ms.map_or(&self.sky, |m| &m.sky);
            fg.add_pass("sky")
                .color(color, LoadOp::Load, StoreOp::Store)
                .depth_read_only(depth)
                .read(env.background, Access::Sampled(ShaderStages::FRAGMENT))
                .execute(move |ctx| {
                    let area = ctx.render_area();
                    // SAFETY: recording inside the pass's rendering scope; the frame
                    // constants are in the ring.
                    unsafe {
                        sky.bind(ctx.cmd);
                        bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                        set_viewport(ctx.device, ctx.cmd, area);
                        bindless.push_constants(ctx.cmd, push_bytes(&base));
                        ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                    }
                });
        }
        // Transparent surfaces, back to front over the opaque image: depth tested, not
        // written, lit like the rest (ADR 0054). They were culled in the last phase only.
        let blended_commands: u32 = (0..STATES)
            .filter(|s| s & BLENDED != 0)
            .map(|s| states.counts[s])
            .sum();
        let reactive = (blended_commands > 0 && velocity.is_some())
            .then(|| fg.create_texture("reactive mask", multi(REACTIVE_FORMAT)));
        // With MSAA, resolved (averaged) for the temporal resolve.
        let reactive_resolved = reactive.and_then(|_| {
            ms.map(|_| fg.create_texture("reactive resolved", single(REACTIVE_FORMAT)))
        });
        // Transparent effects drawn after the scene (particles) mark the pixels they cover
        // in a mask of their own when no transparent surface made one; they draw
        // single-sampled (into the resolved color with MSAA).
        let effects_reactive = (reactive.is_none() && self.reactive_effects && velocity.is_some())
            .then(|| {
                let mask = fg.create_texture("reactive mask (effects)", single(REACTIVE_FORMAT));
                fg.add_pass("reactive clear")
                    .color(mask, LoadOp::Clear([0.0; 4]), StoreOp::Store)
                    .execute(|_| {});
                mask
            });
        if blended_commands > 0 {
            let reactive_index = usize::from(reactive.is_some());
            let pipelines = match (ms, atmosphere, shadowed) {
                (Some(m), _, _) => {
                    let ms_shadowed = m.shadowed.as_ref().filter(|_| shadowed.is_some());
                    match (atmosphere, ms_shadowed) {
                        (Some(_), Some(p)) => &p.blended_atmosphere[reactive_index],
                        (Some(_), None) => &m.blended_atmosphere[variant & 1][reactive_index],
                        (None, Some(p)) => {
                            &p.shading[shadowed_index(variant)].blended[reactive_index]
                        }
                        (None, None) => &m.shading[variant].blended[reactive_index],
                    }
                }
                // Under an atmosphere or fog (the environment variants).
                (None, Some(_), Some(p)) => &p.blended_atmosphere[reactive_index],
                (None, Some(_), None) => &self.blended_atmosphere[variant & 1][reactive_index],
                (None, None, _) => &shading.blended[reactive_index],
            };
            let blended_typed =
                self.typed_pipelines(&states, [pipelines, pipelines], BLENDED_STATES);
            let pipelines = ready(pipelines)?;
            let mut pass = forward_pass(
                fg,
                names.transparent,
                &geometry,
                &materials,
                shadow_map,
                culled,
                visible,
                scene_instances,
            )
            .color(color, LoadOp::Load, StoreOp::Store);
            if let Some(reactive) = reactive {
                pass = pass.color(reactive, LoadOp::Clear([0.0; 4]), StoreOp::Store);
                if let Some(resolved) = reactive_resolved {
                    pass = pass.resolve(resolved, ResolveMode::Average);
                }
            }
            if let Some(tiles) = light_tiles {
                pass = pass.read(tiles, Access::StorageRead(ShaderStages::FRAGMENT));
            }
            if let Some(env) = environment {
                pass = pass
                    .read(env.prefiltered, Access::Sampled(ShaderStages::FRAGMENT))
                    .read(env.dfg, Access::Sampled(ShaderStages::FRAGMENT))
                    .read(env.sh, Access::StorageRead(ShaderStages::FRAGMENT));
            }
            if let Some(t) = atmosphere.and_then(|a| a.textures) {
                pass = pass.read(t.aerial, Access::Sampled(ShaderStages::FRAGMENT));
            }
            if let Some(atlas) = local_frame.atlas.filter(|_| local_frame.shadowed) {
                pass = pass.read(atlas, Access::Sampled(ShaderStages::FRAGMENT));
            }
            let offset = u64::from(phases - 1) * command_bytes;
            pass.depth_read_only(depth).execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(visible),
                    texture: ctx.sampled_index(shadow_map).unwrap_or(0),
                    light_tiles: light_tiles.map_or(0, |b| ctx.address(b)),
                    ..base
                };
                // SAFETY: recording inside the pass's rendering scope; the pass reads the
                // pool, the material table and the commands.
                unsafe {
                    states.draw(
                        ctx,
                        bindless,
                        &geometry,
                        [pipelines, pipelines],
                        &blended_typed,
                        &push,
                        false,
                        BLENDED_STATES,
                        ctx.buffer(culled),
                        offset,
                        ctx.render_area(),
                    );
                }
            });
        }
        let (Some(temporal), Some(velocity), Some(previous)) = (temporal, velocity, previous)
        else {
            return Some(passes);
        };

        // Where no geometry was drawn (depth still 1), the camera's motion.
        let background = ms.map_or(&self.background, |m| &m.background);
        fg.add_pass("motion background")
            .color(velocity, LoadOp::Load, StoreOp::Store)
            .depth_read_only(depth)
            .execute(move |ctx| {
                let area = ctx.render_area();
                // SAFETY: recording inside the pass's rendering scope; the frame constants
                // are in the ring.
                unsafe {
                    background.bind(ctx.cmd);
                    bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
                    set_viewport(ctx.device, ctx.cmd, area);
                    bindless.push_constants(ctx.cmd, push_bytes(&base));
                    ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
                }
            });
        // This frame's transforms are next frame's previous ones (the scene path keeps
        // them in the scene).
        let instances_staged = match source {
            Source::Ring(staged) => Some(staged),
            Source::Scene { .. } => None,
        };
        let copied = instances_staged.map_or(0, |_| instance_count.min(temporal.capacity));
        if let Some(instances_staged) = instances_staged.filter(|_| copied > 0) {
            let size = u64::from(copied) * size_of::<InstanceData>() as u64;
            fg.add_pass("remember instances")
                .write(previous, Access::TransferWrite)
                .execute(move |ctx| {
                    // SAFETY: recording outside a rendering scope; both ranges are in
                    // bounds (the ring holds every instance, the state `capacity`).
                    unsafe {
                        ctx.device.cmd_copy_buffer(
                            ctx.cmd,
                            instances_staged.buffer,
                            ctx.buffer(previous),
                            &[vk::BufferCopy {
                                src_offset: instances_staged.offset,
                                dst_offset: 0,
                                size,
                            }],
                        );
                    }
                });
        }
        temporal.previous_count.set(copied);
        temporal.previous_clip_from_world.set(Some(unjittered));
        temporal.frame.set(temporal.frame.get() + 1);
        Some(ForwardPasses {
            motion: Some(MotionOutput {
                velocity,
                jitter,
                reset,
                reactive: reactive_resolved.or(reactive).or(effects_reactive),
                clip_from_world,
                previous_clip_from_world,
            }),
            ..passes
        })
    }
}

/// What a frame's shading and passes take of the local lights' shadows (ADR 0071).
#[derive(Debug, Default)]
struct LocalFrame {
    /// Some kept light has a shadow: the shading looks them up.
    shadowed: bool,
    /// Addresses of the per-light first views, the views and the drawn views (0: none).
    light_shadows: u64,
    views: u64,
    draws: u64,
    /// The atlas in this frame's graph.
    atlas: Option<Texture>,
    /// This view's drawn tiles among the frame's.
    rects: core::ops::Range<usize>,
}

/// The targets of the shading passes.
#[derive(Debug, Clone, Copy)]
struct Targets {
    color: Texture,
    /// What the first shading pass does with the color target.
    color_load: LoadOp,
    /// Motion vectors, with a temporal state.
    velocity: Option<Texture>,
    /// Last frame's instances, read by the vertex shader for motion vectors.
    previous: Option<Buffer>,
    /// The tiles' light bitmasks, with light culling.
    light_tiles: Option<Buffer>,
    /// The environment's lighting.
    environment: Option<EnvironmentFrame>,
    /// The view's ambient occlusion (ADR 0058), computed after the depth prepass.
    ao: Option<Texture>,
    /// The local lights' shadow atlas, when the shading looks it up (ADR 0071).
    local_atlas: Option<Texture>,
    /// The sun's contact shadows (ADR 0071), computed after the depth prepass.
    contact: Option<Texture>,
    /// The sun's virtual shadow pages (ADR 0093 step 3b), kept and drawn after the depth
    /// prepass.
    vsm: Option<VsmFrame>,
    /// The decal buffer (ADR 0082), computed after the depth prepass.
    decals: Option<DecalTargets>,
    /// The diffuse irradiance per pixel of probe GI (ADR 0088), after the depth prepass.
    gi: Option<Texture>,
    /// Reflections' inputs (ADR 0073): normal and roughness, specular weight.
    reflect: Option<(Texture, Texture)>,
    /// With MSAA (ADR 0075): what the shading pass resolves its targets into.
    resolves: Option<MsResolves>,
}

/// A view's virtual shadow pages in one frame (ADR 0093 step 3b): what draws them, their
/// lists' layout and the frame prepared before the frame's data.
#[derive(Clone, Copy)]
struct PagesFrame<'g> {
    pages: &'g VirtualShadows,
    draw: &'g VsmDraw,
    lists: layout::MeshletLayout,
    prepared: VsmPrepared,
}

impl<'g> PagesFrame<'g> {
    /// The pages' passes after the depth prepass: their bookkeeping over `depth`, then the
    /// casters culled into their dirty pages (`lists`: the commands and the casters' slots)
    /// and drawn there, one rendering without attachments over a level's 4096 × 4096 texels
    /// for every level.
    #[allow(clippy::too_many_arguments)]
    fn add_passes(
        self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        geometry: &GeometryFrame<'g>,
        materials: &MaterialFrame<'g>,
        states: States,
        (commands, casters): (Buffer, Buffer),
        base: Push,
        scene_instances: Option<Buffer>,
        instance_count: u32,
        command_bytes: u64,
        depth: Texture,
    ) -> VsmFrame {
        let frame = self.pages.add_prepared(fg, bindless, self.prepared, depth);
        let Self { draw, lists, .. } = self;
        let push = move |ctx: &PassContext<'_>| Push {
            visible: ctx.address(casters),
            commands: ctx.address(commands),
            phase: PHASE_VSM,
            ..base
        };
        let rw = Access::StorageReadWrite(ShaderStages::COMPUTE);
        fg.add_compute("vsm cull", bindless)
            .write(commands, rw)
            .read(commands, Access::Indirect)
            .write(casters, Access::StorageWrite(ShaderStages::COMPUTE))
            .read(frame.hierarchy, Access::StorageRead(ShaderStages::COMPUTE))
            .read(geometry.indices, Access::StorageRead(ShaderStages::COMPUTE))
            .maybe_read(scene_instances, Access::StorageRead(ShaderStages::COMPUTE))
            .record(move |rec| {
                let push = push(rec.context());
                let args = rec.context().buffer(commands);
                rec.dispatch(&draw.cull, Dispatch::Threads([instance_count, 1, 1]), &push);
                rec.barrier_for_indirect();
                rec.dispatch_indirect(&draw.meshlets, args, command_bytes + MESHLET_ARGS, &push);
            });
        let geometry = *geometry;
        let side = VIRTUAL_TEXELS;
        let area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: side,
                height: side,
            },
        };
        materials
            .read(geometry.read(fg.add_pass("vsm draw")))
            .maybe_read(scene_instances, Access::StorageRead(ShaderStages::VERTEX))
            .read(commands, Access::Indirect)
            .read(casters, Access::StorageRead(ShaderStages::VERTEX))
            .read(frame.table, Access::StorageRead(ShaderStages::FRAGMENT))
            .write(frame.pool, Access::StorageReadWrite(ShaderStages::FRAGMENT))
            .execute(move |ctx| {
                let push = Push {
                    visible: ctx.address(casters),
                    ..base
                };
                let draws = MeshletDraws {
                    layout: lists,
                    base: command_bytes,
                    bytes: lists.list_bytes(),
                };
                // SAFETY: the pass has no attachments and no rendering is begun; it reads the
                // commands (indirect), the casters' slots, the pool's geometry and the
                // material table, and writes only the pages' pool, which it declared.
                unsafe {
                    ctx.begin_rendering_without_attachments(area.extent);
                    states.draw_meshlets(
                        ctx,
                        bindless,
                        &geometry,
                        [&draw.pipelines[0], &draw.pipelines[1]],
                        &push,
                        true,
                        DEPTH_STATES,
                        ctx.buffer(commands),
                        command_bytes,
                        &draws,
                        area,
                    );
                    ctx.end_rendering();
                }
            });
        frame
    }
}

/// The single-sampled textures a multisampled view's shading resolves into (ADR 0075).
#[derive(Debug, Clone, Copy)]
struct MsResolves {
    /// The lit image, for screen-space reflections.
    lit: Option<Texture>,
    /// Reflections' inputs: normal and roughness, specular weight (the nearest
    /// sample's), and the farthest sample's with its depth.
    reflect: Option<[Texture; 5]>,
}

/// How a shading pass uses the depth target.
#[derive(Debug, Clone, Copy)]
enum DepthUse {
    /// Tested and written.
    Write(LoadOp, StoreOp),
    /// Tested for equality with the prepass's depth.
    ReadOnly,
}

/// A shading pass with its targets; `first` starts them (clearing motion vectors), later
/// ones load them.
#[allow(clippy::too_many_arguments)]
fn shading_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
    targets: Targets,
    first: bool,
    depth: Texture,
    depth_use: DepthUse,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    let mut pass = forward_pass(
        fg, name, geometry, materials, shadow_map, culled, visible, instances,
    )
    .color(
        targets.color,
        if first {
            targets.color_load
        } else {
            LoadOp::Load
        },
        StoreOp::Store,
    );
    let resolves = targets.resolves;
    if let Some(lit) = resolves.and_then(|r| r.lit) {
        pass = pass.resolve(lit, ResolveMode::Average);
    }
    if let Some(velocity) = targets.velocity {
        let load = if first {
            LoadOp::Clear([0.0; 4])
        } else {
            LoadOp::Load
        };
        pass = pass.color(velocity, load, StoreOp::Store);
    }
    if let Some((normal_roughness, weight)) = targets.reflect {
        // Cleared to zero where nothing writes them (the sky, subsurface materials): no
        // reflections there.
        let load = if first {
            LoadOp::Clear([0.0; 4])
        } else {
            LoadOp::Load
        };
        pass =
            pass.color(normal_roughness, load, StoreOp::Store)
                .color(weight, load, StoreOp::Store);
    }
    if let Some(previous) = targets.previous {
        pass = pass.read(previous, Access::StorageRead(ShaderStages::VERTEX));
    }
    if let Some(tiles) = targets.light_tiles {
        pass = pass.read(tiles, Access::StorageRead(ShaderStages::FRAGMENT));
    }
    if let Some(env) = targets.environment {
        pass = pass
            .read(env.prefiltered, Access::Sampled(ShaderStages::FRAGMENT))
            .read(env.dfg, Access::Sampled(ShaderStages::FRAGMENT))
            .read(env.sh, Access::StorageRead(ShaderStages::FRAGMENT));
    }
    if let Some(ao) = targets.ao {
        pass = pass.read(ao, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(atlas) = targets.local_atlas {
        pass = pass.read(atlas, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(contact) = targets.contact {
        pass = pass.read(contact, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(decals) = targets.decals {
        pass = pass
            .read(decals.albedo, Access::Sampled(ShaderStages::FRAGMENT))
            .read(decals.surface, Access::Sampled(ShaderStages::FRAGMENT));
    }
    if let Some(gi) = targets.gi {
        pass = pass.read(gi, Access::Sampled(ShaderStages::FRAGMENT));
    }
    match depth_use {
        DepthUse::Write(load, store) => pass.depth(depth, load, store),
        DepthUse::ReadOnly => pass.depth_read_only(depth),
    }
}

/// A culling pass of `phase` with its buffer accesses (`flags`: occlusion culling's
/// visibility flags, read early and rewritten late).
#[allow(clippy::too_many_arguments)]
fn cull_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    bindless: &'g Bindless,
    name: &'static str,
    culled: Buffer,
    visible: Buffer,
    flags: Option<Buffer>,
    phase: u32,
    instances: Option<Buffer>,
) -> ComputePass<'a, 'g> {
    let pass = fg
        .add_compute(name, bindless)
        .read(culled, Access::StorageReadWrite(ShaderStages::COMPUTE))
        .write(visible, Access::StorageWrite(ShaderStages::COMPUTE))
        .maybe_read(instances, Access::StorageRead(ShaderStages::COMPUTE));
    match flags {
        Some(flags) if phase == PHASE_LATE => {
            pass.read(flags, Access::StorageReadWrite(ShaderStages::COMPUTE))
        }
        Some(flags) => pass.read(flags, Access::StorageRead(ShaderStages::COMPUTE)),
        None => pass,
    }
}

/// Records a forward phase's culling with the meshlet path (ADR 0085) into `pass`:
/// `cullMainMeshlets` over the instances, then the meshlet cull over the work items it
/// wrote into the phase's meshlet lists at `lists` in `culled` (their dispatch arguments
/// there too). The meshlets are read from the geometry pool's index buffer.
#[allow(clippy::too_many_arguments)]
fn record_meshlet_cull<'a, 'g>(
    pass: ComputePass<'a, 'g>,
    geometry: &GeometryFrame<'_>,
    culled: Buffer,
    shaders: &'g MeshletCull,
    lists: u64,
    threads: Dispatch,
    push: impl Fn(&PassContext<'_>) -> Push + 'g,
) {
    pass.read(culled, Access::Indirect)
        .read(geometry.indices, Access::StorageRead(ShaderStages::COMPUTE))
        .record(move |rec| {
            let push = push(rec.context());
            let args = rec.context().buffer(culled);
            rec.dispatch(&shaders.instances, threads, &push);
            rec.barrier_for_indirect();
            rec.dispatch_indirect(&shaders.meshlets, args, lists + MESHLET_ARGS, &push);
        });
}

/// A depth prepass phase that draws the culled commands, without its attachment.
#[allow(clippy::too_many_arguments)]
fn prepass_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    culled: Buffer,
    visible: Buffer,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    materials
        .read(geometry.read(fg.add_pass(name)))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
        .maybe_read(instances, Access::StorageRead(ShaderStages::VERTEX))
}

/// A forward pass that draws the culled commands, without its attachments.
#[allow(clippy::too_many_arguments)]
fn forward_pass<'a, 'g>(
    fg: &'a mut FrameGraph<'g>,
    name: &'static str,
    geometry: &GeometryFrame<'_>,
    materials: &MaterialFrame<'_>,
    shadow_map: Texture,
    culled: Buffer,
    visible: Buffer,
    instances: Option<Buffer>,
) -> PassBuilder<'a, 'g> {
    materials
        .read(geometry.read(fg.add_pass(name)))
        .read(shadow_map, Access::Sampled(ShaderStages::FRAGMENT))
        .read(culled, Access::Indirect)
        .read(visible, Access::StorageRead(ShaderStages::VERTEX))
        .maybe_read(instances, Access::StorageRead(ShaderStages::VERTEX))
}

/// The shader's view of the frame's cascades: projections as scale and offset, atlas
/// tiles, selection limits and caster bounds.
/// Each cascade's constants: its projection; its tile of the atlas of `atlas` columns and
/// rows, or where its window starts in its own layer, which `held` says; the light view
/// rectangles it draws this frame (`plans`); and the error its casters' detail levels may
/// have, `lod_threshold` of its texels (ADR 0093).
fn cascade_data(
    fit: &CascadeFit,
    plans: &[ShadowPlan; MAX_CASCADES],
    held: &[Option<Held>; MAX_CASCADES],
    lod_threshold: f32,
    atlas: Option<(u32, u32)>,
    masks: Option<u32>,
) -> [CascadeData; MAX_CASCADES] {
    // Reached by nothing: the shaders test boxes against it without checking it.
    const EMPTY: [f32; 4] = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    let mut out = [CascadeData::default(); MAX_CASCADES];
    for (k, (data, cascade)) in out.iter_mut().zip(&fit.cascades[..fit.count]).enumerate() {
        let Some(h) = held[k] else {
            continue;
        };
        let m = cascade.clip_from_light();
        let uv = match atlas {
            Some((columns, rows)) => {
                let tile = [(k as u32 % columns) as f32, (k as u32 / columns) as f32];
                let grid = [columns as f32, rows as f32];
                [
                    0.5 / grid[0],
                    0.5 / grid[1],
                    (0.5 + tile[0]) / grid[0],
                    (0.5 + tile[1]) / grid[1],
                ]
            }
            None => {
                let n = h.resolution as f32;
                let start = h.storage_origin();
                [
                    0.5,
                    0.5,
                    0.5 + start[0] as f32 / n,
                    0.5 + start[1] as f32 / n,
                ]
            }
        };
        // Half a texel around each rectangle: whatever touches its texels is drawn.
        let pad = 0.5 * cascade.texel;
        let mut drawn = [EMPTY; crate::shadow_cache::MAX_RECTS];
        for (d, rect) in drawn.iter_mut().zip(plans[k].rects()) {
            let [x0, y0, x1, y1] = h.light_rect(rect.window);
            *d = [x0 - pad, y0 - pad, x1 + pad, y1 + pad];
        }
        // Drawn in several pieces at once (ADR 0093): where its layer's seams lie in light
        // view space (none: beyond every rectangle), the pieces it draws, and where its
        // meshlet draws' pieces go.
        let (seam, pieces, masks) = match masks {
            Some(masks) if plans[k].piece_count() > 1 => {
                let n = h.resolution as i32;
                let s = h.storage_origin();
                let seam = [
                    if s[0] == 0 {
                        f32::MAX
                    } else {
                        (h.origin[0] + n - s[0] as i32) as f32 * h.texel
                    },
                    if s[1] == 0 {
                        f32::MIN
                    } else {
                        -((h.origin[1] + n - s[1] as i32) as f32) * h.texel
                    },
                ];
                let pieces = plans[k]
                    .piece_slots(s)
                    .fold(0u32, |bits, (slot, _)| bits | 1 << slot);
                (seam, pieces, masks)
            }
            _ => ([0.0; 2], 0, 0),
        };
        *data = CascadeData {
            scale: [m.x_axis.x, m.y_axis.y, m.z_axis.z, cascade.texel],
            offset: [m.w_axis.x, m.w_axis.y, m.w_axis.z, cascade.limit],
            atlas: uv,
            drawn,
            far: cascade.far,
            lod_error: lod_threshold * cascade.texel,
            seam,
            pieces,
            masks,
            rects: plans[k].rect_count as u32,
            _pad: 0,
        };
    }
    out
}

/// What makes the GI shading's irradiance after the depth prepass (ADR 0088, ADR 0090):
/// the probe volume's resolve or a screen GI's passes.
#[derive(Clone, Copy)]
struct GiResolve<'a> {
    view: Option<ViewGi<'a>>,
    /// The sky's harmonics and intensity outside the volume.
    sky: Option<(u64, f32)>,
    depth: Texture,
    size: [u32; 2],
    clip_from_world: Mat4,
    screen: ScreenGiInput,
}

impl<'g> GiResolve<'g> {
    /// Declares the passes; `None` without GI this frame.
    fn add(
        self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
    ) -> Option<Texture> {
        match self.view? {
            ViewGi::Probes { volume, .. } => {
                let (sh, intensity) = self.sky?;
                volume
                    .add_resolve_pass(
                        fg,
                        ring,
                        bindless,
                        self.depth,
                        self.size,
                        self.clip_from_world,
                        self.screen.camera.position,
                        sh,
                        intensity,
                    )
                    .ok()
            }
            ViewGi::Screen(screen) => screen.add_passes(fg, ring, bindless, &self.screen),
        }
    }
}
