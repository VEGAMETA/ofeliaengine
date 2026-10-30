//! The game runtime (ADR 0064): the frame loop that ties the ECS world, the fixed-step
//! simulation and the renderer together.
//!
//! - [`FixedStep`] (`engine_physics::ecs`, ADR 0066): the simulation clock. Real time
//!   accumulates; the simulation advances in fixed ticks (ADR 0019: 64 Hz by default), at
//!   most a few per frame, and the remainder becomes the interpolation factor of the
//!   frame.
//! - [`RenderExtract`]: mirrors the entities with a [`MeshRenderer`] and a
//!   `GlobalTransform` into a [`GpuScene`](engine_render::gpu_scene::GpuScene) (any
//!   [`SceneSink`]), with a cost that follows what changed: chunk change detection for
//!   additions and edits, the world's removal log for despawns and removed components.
//!   Moving instances are interpolated between the last two ticks every render frame.
//! - [`Runtime`]: one frame of the game in the order that keeps input latency low:
//!   input is sampled, the due ticks run (each consuming the input, then transform
//!   propagation, then extraction), then a per-frame presentation step updates what
//!   must follow input every frame (the player's camera, the local character's
//!   presentation) from the latest input, then movers are interpolated and the frame is
//!   rendered.
//! - [`PointLight`], [`SpotLight`] and [`Sun`] (ADR 0099 step 3): lights on entities,
//!   mirrored into the renderer's light list after every tick by [`LightExtract`] (each
//!   light keeps its slot while it lives: its cached shadow is known by it).
//! - [`SkinnedMesh`]: an entity's skinned instance, placed at its scene instance every
//!   frame by [`Runtime::place_skinned`].
//! - [`ReflectionProbe`] and [`GiVolume`] (ADR 0099 step 4): reflection probes for the
//!   boxes around their entities, mirrored into the renderer's probe set by a
//!   [`ProbeExtract`]; the box a volume of global illumination probes covers, found by a
//!   [`GiExtract`].
//! - [`Camera`] and [`CameraView`] (ADR 0072): the main camera, read right after the
//!   presentation step every rendered frame, becomes the frame renderer's view
//!   (`engine_frame`): projection, exposure, antialiasing, cuts.
//!
//! **One world, one simulation.** Singleplayer runs one ECS world and one simulation,
//! with no network layer: no loopback client and server, no snapshot serialization, no
//! interpolation delay for remote entities. Multiplayer (ADR 0019) reuses the same tick
//! systems on the server, with client prediction; nothing here assumes a server.
//!
//! **Responsiveness.** The camera is not part of the tick: driven by ticks and
//! interpolated, it would trail input by up to one tick plus the time to the next tick
//! (31 ms at 64 Hz in the worst case). The tick still consumes the same input (aim,
//! movement) for the simulation, as sub-tick inputs do in CS2 (ADR 0019's addendum).
//!
//! **Interpolation happens at extraction**, for every renderable and without writing the
//! simulation's components. `engine_physics::ecs::PhysicsSync::interpolate` (which writes
//! blended poses into the `Transform` of physics roots) is for loops without this crate:
//! under [`Runtime`], leave it out, or physics bodies would be blended twice.

mod activity;
mod animator;
mod camera;
mod extract;
mod lights;
mod probes;
mod runtime;
mod skinned;
mod timing;

pub use activity::{ActivityEpoch, ActivityPropagation, Disabled, Inactive, is_inactive};
pub use animator::{
    AnimationEvent, AnimationEvents, Animator, Animators, JointAttachment, RootMotion,
};
pub use camera::{Camera, CameraView, MainCamera, look_rotation};
pub use engine_physics::ecs::FixedStep;
pub use extract::{Derived, ExtractStats, MeshRenderer, RenderExtract, SceneSink};
pub use lights::{LightExtract, PointLight, SpotLight, Sun};
pub use probes::{GiExtract, GiVolume, ProbeExtract, ProbeSink, ReflectionProbe};
pub use runtime::{Driven, FrameStages, Runtime};
pub use skinned::SkinnedMesh;
pub use timing::{Ease, FieldRef, Repeat, Timer, Timing, Tween};
