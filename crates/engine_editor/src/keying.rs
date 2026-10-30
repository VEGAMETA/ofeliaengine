//! Keys made by editing a clip's fields (ADR 0124's E12-4b-2, the team's decision of
//! 2026-10-30): while the Animation tab previews a clip made in the editor, the editor's
//! frame runs with the fields the clip keys holding its values at the playhead, so the
//! inspector shows them and a drag starts from them. Undo records the scene's own values
//! for them ([`crate::History`]), and an action that changes one keys it in the clip at the
//! playhead, in the action's own undo step: the scene keeps its values, put back as the
//! frame ends (Unity keeps the values its animation mode drives out of the scene the same
//! way).

use engine_app::{ClipField, ClipFields};
use engine_ecs::{Entity, World};
use engine_scene::{AnimFile, AssetId, SceneId};

/// A field the clip drives through the frame, and the scene's value of it.
#[derive(Debug, Clone, Copy)]
struct Driven {
    field: ClipField,
    /// The scene's value.
    scene: [f32; 4],
    /// What it held while a record took the scene's value ([`Driving::scene_in`]).
    held: [f32; 4],
}

/// The clip driven through the editor's frame (see the module).
#[derive(Debug, Default)]
pub(crate) struct Driving {
    sampler: ClipFields,
    sampled: Vec<ClipField>,
    /// The clip's file, and the clip as the frame's actions have keyed it.
    file: AssetId,
    anim: AnimFile,
    /// The playhead, seconds into the clip.
    time: f32,
    /// Actions key its fields (it holds a moment; while it plays, an edit is not kept).
    keying: bool,
    fields: Vec<Driven>,
    /// It drives fields this frame.
    active: bool,
    /// Actions keyed it this frame.
    keyed: bool,
    /// Its file changed some other way this frame (undo, redo, K): keyed no more until the
    /// next frame samples it again.
    stale: bool,
}

impl Driving {
    /// Clip file `file` (holding `anim`) driving `entity`'s fields at `time` seconds: each
    /// field its tracks key given the clip's value there, the scene's kept.
    pub(crate) fn begin(
        &mut self,
        world: &mut World,
        entity: Entity,
        file: AssetId,
        anim: &AnimFile,
        time: f32,
        keying: bool,
    ) {
        if self.active {
            self.end(world);
        }
        if self.file != file || self.anim != *anim {
            self.file = file;
            self.anim = anim.clone();
        }
        self.time = time;
        self.keying = keying;
        self.keyed = false;
        self.stale = false;
        self.sampler
            .sample(world, entity, &self.anim, time, &mut self.sampled);
        self.fields.clear();
        for f in &self.sampled {
            let known = self
                .fields
                .iter()
                .any(|d| d.field.entity == f.entity && d.field.field == f.field);
            let mut scene = [0.0f32; 4];
            if known || !f.field.read(world, f.entity, &mut scene) {
                continue;
            }
            f.field.write(world, f.entity, &f.value[..f.field.lanes()]);
            self.fields.push(Driven {
                field: *f,
                scene,
                held: scene,
            });
        }
        self.active = true;
    }

    /// The frame ends: the scene's values back in the fields; the clip's file and the clip
    /// when actions keyed it.
    pub(crate) fn end(&mut self, world: &mut World) -> Option<(AssetId, &AnimFile)> {
        if !self.active {
            return None;
        }
        self.active = false;
        for d in &self.fields {
            let f = &d.field;
            f.field.write(world, f.entity, &d.scene[..f.field.lanes()]);
        }
        self.fields.clear();
        (self.keyed && !self.stale).then_some((self.file, &self.anim))
    }

    /// Before `entity` is recorded: its driven fields given the scene's values. Whether it
    /// has any (then [`Self::scene_out`] after).
    pub(crate) fn scene_in(&mut self, world: &mut World, entity: Entity) -> bool {
        if !self.active {
            return false;
        }
        let mut any = false;
        for d in self.fields.iter_mut().filter(|d| d.field.entity == entity) {
            let f = &d.field;
            f.field.read(world, entity, &mut d.held);
            f.field.write(world, entity, &d.scene[..f.field.lanes()]);
            any = true;
        }
        any
    }

    /// After `entity` was recorded: its driven fields given back what they held.
    pub(crate) fn scene_out(&self, world: &mut World, entity: Entity) {
        for d in self.fields.iter().filter(|d| d.field.entity == entity) {
            let f = &d.field;
            f.field.write(world, entity, &d.held[..f.field.lanes()]);
        }
    }

    /// Every driven field given the scene's value (a scene written whole); what they held,
    /// in order, for [`Self::all_back`].
    pub(crate) fn all_in(&self, world: &mut World) -> Vec<[f32; 4]> {
        if !self.active {
            return Vec::new();
        }
        self.fields
            .iter()
            .map(|d| {
                let f = &d.field;
                let mut held = d.scene;
                f.field.read(world, f.entity, &mut held);
                f.field.write(world, f.entity, &d.scene[..f.field.lanes()]);
                held
            })
            .collect()
    }

    /// What [`Self::all_in`] took, given back.
    pub(crate) fn all_back(&self, world: &mut World, held: &[[f32; 4]]) {
        for (d, held) in self.fields.iter().zip(held) {
            let f = &d.field;
            f.field.write(world, f.entity, &held[..f.field.lanes()]);
        }
    }

    /// Undo, redo or a cancel put entities back (those whose scene id `restored` says): their
    /// driven fields now hold the scene's values, kept, and get the clip's again. The clip's
    /// file may have been put back too: no more keys this frame.
    pub(crate) fn restored(&mut self, world: &mut World, restored: impl Fn(SceneId) -> bool) {
        if !self.active {
            return;
        }
        self.stale = true;
        for d in &mut self.fields {
            let f = &d.field;
            let back = world
                .get::<SceneId>(f.entity)
                .is_some_and(|&id| restored(id));
            if back && f.field.read(world, f.entity, &mut d.scene) {
                f.field.write(world, f.entity, &f.value[..f.field.lanes()]);
            }
        }
    }

    /// The clip's file changed outside the frame's actions (K, ◆): keyed no more until the
    /// next frame.
    pub(crate) fn changed(&mut self) {
        self.stale = true;
    }

    /// An action ends having touched the entities `touched` says: what it changed of their
    /// driven fields keyed at the playhead. The clip's file and its text before and after,
    /// when it keyed any.
    pub(crate) fn key(
        &mut self,
        world: &World,
        touched: impl Fn(Entity) -> bool,
    ) -> Option<(AssetId, String, String)> {
        if !self.active || !self.keying || self.stale {
            return None;
        }
        let mut before = None;
        for d in &mut self.fields {
            let f = &mut d.field;
            let mut now = f.value;
            if !touched(f.entity)
                || !f.field.read(world, f.entity, &mut now)
                || same(&now, &f.value, f.field.lanes())
            {
                continue;
            }
            if before.is_none() {
                before = Some(self.anim.to_text(self.file));
            }
            let key = f.key_of(&now);
            self.anim
                .set_key(self.sampler.target(f.track), self.time, &key[..f.lanes]);
            f.value = now;
            f.keyed = key;
        }
        let before = before?;
        self.keyed = true;
        Some((self.file, before, self.anim.to_text(self.file)))
    }
}

/// Whether two values of a field are the same but for rounding.
fn same(a: &[f32; 4], b: &[f32; 4], lanes: usize) -> bool {
    a.iter()
        .zip(b)
        .take(lanes)
        .all(|(x, y)| (x - y).abs() <= 1e-5 * (1.0 + x.abs().max(y.abs())))
}
