//! What a game uses most, from one crate: `use engine_app::prelude::*;` (the quick start,
//! `docs/dev-guide/quick-start.md`).
//!
//! The player and its hooks, the world and its components, math, input, meshes and
//! materials (by asset: `Model`, `builtin`), names and scene ids, lights and cameras,
//! timers, sound, saves and networking. Everything else is in the crates named in the
//! guides.

pub use crate::{
    AudioListener, AudioSource, AudioSource3D, FrameParts, Game, MaterialFile, MaterialSlots,
    Mixer, Model, Network, PlayFrom, Player, PlayerSettings, ReverbZone, Room, Saves, Setup,
    Window, builtin,
};
pub use engine_core::math::{Affine3A, Mat4, Quat, Vec2, Vec3, Vec4};
pub use engine_ecs::{
    Children, Entity, GlobalTransform, Parent, QueryState, Transform, With, Without, World,
};
pub use engine_input::{Binding, GamepadButton, Input, InputMap, Key, MouseButton, Stick};
pub use engine_render::material::{MaterialDesc, MaterialId};
pub use engine_render::mesh::{MeshId, cube};
pub use engine_runtime::{
    Animator, Camera, Disabled, Ease, MeshRenderer, PointLight, Repeat, SpotLight, Sun, Timer,
    Tween, look_rotation,
};
pub use engine_scene::{AssetId, AssetRef, Name, SceneId, find_named};
