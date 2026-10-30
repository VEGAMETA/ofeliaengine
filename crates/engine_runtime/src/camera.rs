//! Cameras that render (ADR 0072): a [`Camera`] component on an entity, read every
//! rendered frame by a [`CameraView`] into the frame renderer's camera and settings. An
//! orthographic camera's view is a 2D view (ADR 0125): the frame renderer draws the 2D
//! world in it, its colours as painted unless the camera asks for the post chain.

use engine_core::math::Quat;
use engine_ecs::{
    Entity, GlobalTransform, Parent, QueryState, Transform, World, bytemuck, reflect_component,
};
use engine_frame::{
    Antialiasing, Exposure, Msaa, PostAntialiasing, Quality, SmaaQuality, Upscale, UpscalerKind,
    ViewCamera, ViewKind, ViewSettings,
};
use engine_reflect::Reflect;
use engine_render::grading::ColorGrading;
use engine_render::motion_blur::MotionBlurSettings;
use engine_render::post::ToneCurve;
use engine_render::postfx::ExposureSettings;

/// A camera that renders: its projection, exposure and antialiasing. Put it on an entity
/// with a [`Transform`] (it looks down its −Z axis, +Y up). The active camera with the
/// highest `priority` is the main view ([`CameraView`]).
///
/// An orthographic camera ([`Self::orthographic`], [`Self::pixel_art`]) is a 2D camera
/// (ADR 0125): left unturned it looks down −Z onto the XY plane, y up, and its view draws
/// the 2D world (sprites), not the 3D one. Turn it about Z only.
///
/// The player's camera is moved every rendered frame (ADR 0019's addendum): write its
/// `Transform` in `Runtime::frame`'s `present` step, from the latest input. The renderer
/// reads it right after, without waiting for a tick's transform propagation.
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct Camera {
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane distance.
    pub near: f32,
    /// Far plane distance.
    pub far: f32,
    /// The exposure: a linear multiplier, or with `auto_exposure` a compensation in EV
    /// (+1 twice as bright).
    pub exposure: f32,
    /// Nonzero: the exposure adapts to the image (ADR 0047).
    pub auto_exposure: u32,
    /// [`Antialiasing`] as stored ([`Self::antialiasing`]): bits 0-3 the upscale (0
    /// bilinear, 1 TAAU, 2 FSR 3.1, 3 DLSS, 4 nearest, 5 integer, 6 FSR 1, 7 SMAA T2x,
    /// 8 Catmull-Rom, 9 Lanczos-3), bits 4-5 log2 of the samples, bits 8-10 the post
    /// filter (0 off, 1 FXAA, 2 MLAA, 3-6 SMAA low to ultra). 0 is off, 1 TAAU.
    pub antialiasing: u32,
    /// [`Quality`] as stored: its index in [`Quality::ALL`] ([`Self::quality`]).
    pub quality: u32,
    /// Nonzero: a cinematic camera (cutscenes, sequences; ADR 0067), the only kind that
    /// may get motion blur.
    pub cinematic: u32,
    /// Among active cameras, the highest priority is the main view.
    pub priority: i32,
    /// Nonzero: the camera may render.
    pub active: u32,
    /// Cut generation: [`Self::cut`] bumps it, and the next frame drops the view's
    /// history (teleports, edits of the camera that are not motion).
    pub cut_generation: u32,
    /// A cinematic camera's motion blur (ADR 0096): the share of the frame its shutter is
    /// open (0.5: film's 180°); 0 none. A gameplay camera has none, whatever this holds.
    pub shutter: f32,
    /// The projection (ADR 0125): [`Self::PERSPECTIVE`] (0, through `fov_y`) or
    /// [`Self::ORTHOGRAPHIC`] (1, through `height`): a 2D view.
    pub projection: u32,
    /// An orthographic camera's view: the world units (metres) it shows from the image's
    /// bottom edge to its top; its width follows the image's aspect.
    pub height: f32,
    /// An orthographic camera's pixel-art resolution (ADR 0125): its view renders at this
    /// size and is shown at the largest whole multiple of it that fits the output,
    /// centered, black around (ADR 0016). `[0, 0]`: the output's size.
    pub reference: [u32; 2],
    /// Nonzero: an orthographic camera's view goes through the post chain of the player's
    /// view settings (bloom, the tone curve, grading), for glows. Zero (the default): its
    /// colours reach the screen as they were painted (no curve, no bloom, no grading).
    pub post_effects: u32,
}

reflect_component!(pod Camera);

impl Camera {
    /// [`Self::projection`]'s perspective.
    pub const PERSPECTIVE: u32 = 0;
    /// [`Self::projection`]'s orthographic: a 2D view (ADR 0125).
    pub const ORTHOGRAPHIC: u32 = 1;

    /// An active perspective camera: `fov_y` radians, planes at `near` and `far`, fixed
    /// exposure 1, the engine's TAAU at the output size.
    pub fn perspective(fov_y: f32, near: f32, far: f32) -> Self {
        Self {
            fov_y,
            near,
            far,
            exposure: 1.0,
            auto_exposure: 0,
            antialiasing: encode_antialiasing(Antialiasing::TAAU),
            quality: 0,
            cinematic: 0,
            priority: 0,
            active: 1,
            cut_generation: 0,
            shutter: 0.0,
            projection: Self::PERSPECTIVE,
            height: 0.0,
            reference: [0, 0],
            post_effects: 0,
        }
    }

    /// An active orthographic camera (ADR 0125) showing `height` metres from the bottom of
    /// the image to its top, at the output's size: a 2D view, neutral (colours as painted,
    /// fixed exposure 1, no antialiasing: sprites keep their hard edges), its planes a
    /// kilometre before and behind it.
    pub fn orthographic(height: f32) -> Self {
        Self {
            projection: Self::ORTHOGRAPHIC,
            height,
            near: -1000.0,
            far: 1000.0,
            antialiasing: encode_antialiasing(Antialiasing::OFF),
            ..Self::perspective(0.0, -1000.0, 1000.0)
        }
    }

    /// A pixel-art camera (ADR 0125): [`Self::orthographic`] rendering at `reference`
    /// pixels, each a pixel of art drawn at `pixels_per_unit` pixels a metre (an image's
    /// `.meta` setting, 16 by default), shown at the largest whole multiple that fits the
    /// output.
    pub fn pixel_art(reference: [u32; 2], pixels_per_unit: f32) -> Self {
        Self {
            reference,
            ..Self::orthographic(reference[1] as f32 / pixels_per_unit.max(f32::MIN_POSITIVE))
        }
    }

    /// Whether it is orthographic: a 2D view (ADR 0125).
    pub fn is_orthographic(&self) -> bool {
        self.projection == Self::ORTHOGRAPHIC
    }

    /// Its pixel-art resolution, if it has one ([`Self::reference`]).
    pub fn reference_size(&self) -> Option<(u32, u32)> {
        let [width, height] = self.reference;
        (width > 0 && height > 0).then_some((width, height))
    }

    /// This camera with `antialiasing` at `quality`.
    pub fn with_antialiasing(mut self, antialiasing: Antialiasing, quality: Quality) -> Self {
        self.antialiasing = encode_antialiasing(antialiasing);
        self.quality = Quality::ALL.iter().position(|&q| q == quality).unwrap_or(0) as u32;
        self
    }

    /// The antialiasing (an unknown upscale reads as TAAU, unknown parts as off).
    pub fn antialiasing(&self) -> Antialiasing {
        decode_antialiasing(self.antialiasing)
    }

    /// The render size relative to the output.
    pub fn quality(&self) -> Quality {
        Quality::ALL
            .get(self.quality as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Whether it may render.
    pub fn is_active(&self) -> bool {
        self.active != 0
    }

    /// Whether it is cinematic.
    pub fn is_cinematic(&self) -> bool {
        self.cinematic != 0
    }

    /// Its view's kind: cinematic (with motion blur when it has a shutter) or gameplay.
    pub fn view_kind(&self) -> ViewKind {
        if !self.is_cinematic() {
            return ViewKind::Gameplay;
        }
        ViewKind::Cinematic {
            motion_blur: (self.shutter > 0.0).then(|| MotionBlurSettings {
                shutter: self.shutter,
                ..MotionBlurSettings::default()
            }),
        }
    }

    /// Marks a cut: the next rendered frame does not continue the last one.
    pub fn cut(&mut self) {
        self.cut_generation = self.cut_generation.wrapping_add(1);
    }
}

const UPSCALES: [Upscale; 10] = [
    Upscale::Bilinear,
    Upscale::Temporal(UpscalerKind::Taau),
    Upscale::Temporal(UpscalerKind::Fsr),
    Upscale::Temporal(UpscalerKind::Dlss),
    Upscale::Nearest,
    Upscale::Integer,
    Upscale::Fsr1,
    Upscale::SmaaT2x,
    Upscale::CatmullRom,
    Upscale::Lanczos3,
];

const POSTS: [PostAntialiasing; 7] = [
    PostAntialiasing::Off,
    PostAntialiasing::Fxaa,
    PostAntialiasing::Mlaa,
    PostAntialiasing::Smaa(SmaaQuality::Low),
    PostAntialiasing::Smaa(SmaaQuality::Medium),
    PostAntialiasing::Smaa(SmaaQuality::High),
    PostAntialiasing::Smaa(SmaaQuality::Ultra),
];

fn encode_antialiasing(antialiasing: Antialiasing) -> u32 {
    let upscale = UPSCALES
        .iter()
        .position(|&u| u == antialiasing.upscale)
        .unwrap_or(1) as u32;
    let samples = antialiasing.samples.count().trailing_zeros();
    let post = POSTS
        .iter()
        .position(|&p| p == antialiasing.post)
        .unwrap_or(0) as u32;
    upscale | samples << 4 | post << 8
}

fn decode_antialiasing(bits: u32) -> Antialiasing {
    Antialiasing {
        samples: Msaa::ALL[((bits >> 4) & 3) as usize],
        upscale: UPSCALES
            .get((bits & 15) as usize)
            .copied()
            .unwrap_or(Upscale::Temporal(UpscalerKind::Taau)),
        post: POSTS
            .get(((bits >> 8) & 7) as usize)
            .copied()
            .unwrap_or_default(),
    }
}

/// What [`CameraView::resolve`] found: the main camera's entity, its view and the
/// settings to render it with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MainCamera {
    /// The camera's entity.
    pub entity: Entity,
    /// Its view for `FrameRenderer::add_view` (a cut when the camera changed or was cut).
    pub camera: ViewCamera,
    /// Its settings for `FrameRenderer::prepare`.
    pub settings: ViewSettings,
}

type Cameras = QueryState<(
    Entity,
    &'static Camera,
    &'static Transform,
    Option<&'static Parent>,
)>;

/// Finds the main camera of a world every frame and turns it into what the frame
/// renderer takes (ADR 0072). Keep one per rendered view next to its `engine_frame::View`.
///
/// ```text
/// let stages = runtime.frame(dt, &input, &mut scene, tick, |world, input, _| {
///     // the player's camera follows the latest input, every frame
///     world.get_mut::<Transform>(player_camera).unwrap().rotation = aim(input);
/// });
/// if let Some(main) = cameras.resolve(&runtime.world) {
///     frame.prepare(&mut view, &main.settings, output, prepare_ctx)?;
///     // ... in the frame's graph:
///     frame.add_view(&mut fg, &mut ctx, &mut view, &scene_input, &main.camera, target)?;
/// }
/// ```
pub struct CameraView {
    cameras: Cameras,
    /// The other settings (bloom, tone curve, occlusion culling, ...).
    base: ViewSettings,
    /// The camera rendered last, and its cut generation.
    current: Option<(Entity, u32)>,
}

impl core::fmt::Debug for CameraView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CameraView")
            .field("base", &self.base)
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

impl CameraView {
    /// A view of `world`'s main camera; `base` holds the settings cameras do not carry.
    pub fn new(world: &mut World, base: ViewSettings) -> Self {
        Self {
            cameras: QueryState::new(world),
            base,
            current: None,
        }
    }

    /// The settings cameras do not carry.
    pub fn base_mut(&mut self) -> &mut ViewSettings {
        &mut self.base
    }

    /// The main camera as it is now: the active [`Camera`] with the highest priority (the
    /// lowest entity index among equals), at its `Transform` (under its parent's
    /// `GlobalTransform` when it has a parent). A different camera than last time, or a
    /// bumped cut generation, is a cut. `None` without an active camera.
    pub fn resolve(&mut self, world: &World) -> Option<MainCamera> {
        engine_profiler::zone!("runtime::CameraView::resolve");
        let mut best: Option<(Entity, Camera, Transform, Option<Entity>)> = None;
        self.cameras
            .for_each_readonly(world, |(entity, camera, transform, parent)| {
                if !camera.is_active() {
                    return;
                }
                let better = best.as_ref().is_none_or(|(e, c, ..)| {
                    camera.priority > c.priority
                        || (camera.priority == c.priority && entity.index() < e.index())
                });
                if better {
                    best = Some((entity, *camera, *transform, parent.map(Parent::get)));
                }
            });
        let (entity, camera, transform, parent) = best?;
        let (rotation, position) = match parent.and_then(|p| world.get::<GlobalTransform>(p)) {
            Some(parent) => {
                let (_, rotation, translation) =
                    (parent.0 * transform.to_affine()).to_scale_rotation_translation();
                (rotation.normalize(), translation)
            }
            None => (transform.rotation, transform.translation),
        };
        let cut = self.current != Some((entity, camera.cut_generation));
        self.current = Some((entity, camera.cut_generation));
        let mut view = if camera.is_orthographic() {
            ViewCamera::orthographic(rotation, position, camera.height, camera.near, camera.far)
        } else {
            ViewCamera::from_rigid(rotation, position, camera.fov_y, camera.near, camera.far)
        };
        view.cut = cut;
        let exposure = if camera.auto_exposure != 0 {
            Exposure::Auto(ExposureSettings {
                compensation: camera.exposure,
                ..ExposureSettings::default()
            })
        } else {
            Exposure::Fixed(camera.exposure)
        };
        let settings = ViewSettings {
            antialiasing: camera.antialiasing(),
            quality: camera.quality(),
            exposure,
            kind: camera.view_kind(),
            ..self.base
        };
        Some(MainCamera {
            entity,
            camera: view,
            settings: if camera.is_orthographic() {
                settings_2d(&camera, settings)
            } else {
                settings
            },
        })
    }
}

/// A 2D view's settings (ADR 0125) from `camera` and the 3D view's `settings`: neutral
/// (no tone curve, bloom or grading) unless the camera asks for the post chain; at its
/// pixel-art resolution, scaled by whole numbers, when it has one. The rest of what a 2D
/// view does not use the frame renderer leaves out (`FrameRenderer::prepare_2d`).
fn settings_2d(camera: &Camera, settings: ViewSettings) -> ViewSettings {
    let reference = camera.reference_size();
    let neutral = camera.post_effects == 0;
    ViewSettings {
        antialiasing: match reference {
            Some(_) => Antialiasing {
                upscale: Upscale::Integer,
                ..settings.antialiasing
            },
            None => settings.antialiasing,
        },
        render_size: reference.or(settings.render_size),
        curve: if neutral {
            ToneCurve::None
        } else {
            settings.curve
        },
        bloom: settings.bloom && !neutral,
        grading: if neutral {
            ColorGrading::default()
        } else {
            settings.grading
        },
        ..settings
    }
}

/// The rotation of a camera at `eye` looking at `target` with `up` (the inverse of
/// `look_at`'s rotation): for placing camera entities.
pub fn look_rotation(
    eye: engine_core::math::Vec3,
    target: engine_core::math::Vec3,
    up: engine_core::math::Vec3,
) -> Quat {
    let view = engine_core::math::look_at(eye, target, up);
    Quat::from_mat4(&view.inverse()).normalize()
}
