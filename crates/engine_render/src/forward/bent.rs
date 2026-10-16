//! Ambient occlusion's bent normals (ADR 0058): the variants of the shading pipelines that
//! read them, derived from the pipelines that read the visibility only.
//!
//! With `AoSettings::bent_normals` the ambient occlusion target is RGBA8: the visibility in
//! R, the bent normal in GBA. Every pipeline that reads ambient occlusion then has a twin
//! whose entry point (`fragmentMainAo…` → `fragmentMainAoBent…`, and so on) also reads the
//! bent normal: the same description with that entry point, made the first time a frame
//! asks for it ([`BentTwins::twin`]), by the device's warm-up thread. Until it exists the
//! frame draws with the pipeline it derives from, which reads R as ever: never a hitch,
//! and nothing of it in a renderer that never turns the bent normals on.

use core::cell::RefCell;
use core::ffi::CStr;
use engine_rhi::{Device, LazyPipeline, spirv_words};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;

/// The entry points that read the bent normal, and their SPIR-V.
const BENT_FRAGMENTS: [(&CStr, &[u8]); 40] = [
    (
        c"fragmentMainAoBent",
        include_bytes!("../../shaders/forward.fragmentMainAoBent.spv"),
    ),
    (
        c"fragmentReflectAoBent",
        include_bytes!("../../shaders/forward.fragmentReflectAoBent.spv"),
    ),
    (
        c"fragmentReflectAoBentShadowed",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentShadowed.spv"),
    ),
    (
        c"fragmentReflectAoBentContact",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContact.spv"),
    ),
    (
        c"fragmentReflectAoBentContactShadowed",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactShadowed.spv"),
    ),
    (
        c"fragmentSubsurfaceBent",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBent.spv"),
    ),
    (
        c"fragmentMainAoBentShadowed",
        include_bytes!("../../shaders/forward.fragmentMainAoBentShadowed.spv"),
    ),
    (
        c"fragmentSubsurfaceBentShadowed",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentShadowed.spv"),
    ),
    (
        c"fragmentMainAoBentContact",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContact.spv"),
    ),
    (
        c"fragmentMainAoBentContactShadowed",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactShadowed.spv"),
    ),
    (
        c"fragmentMainAoBentDecals",
        include_bytes!("../../shaders/forward.fragmentMainAoBentDecals.spv"),
    ),
    (
        c"fragmentMainAoBentShadowedDecals",
        include_bytes!("../../shaders/forward.fragmentMainAoBentShadowedDecals.spv"),
    ),
    (
        c"fragmentMainAoBentContactDecals",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactDecals.spv"),
    ),
    (
        c"fragmentMainAoBentContactShadowedDecals",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactShadowedDecals.spv"),
    ),
    (
        c"fragmentReflectAoBentDecals",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentDecals.spv"),
    ),
    (
        c"fragmentReflectAoBentShadowedDecals",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentShadowedDecals.spv"),
    ),
    (
        c"fragmentReflectAoBentContactDecals",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactDecals.spv"),
    ),
    (
        c"fragmentReflectAoBentContactShadowedDecals",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactShadowedDecals.spv"),
    ),
    (
        c"fragmentMainAoBentGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentGi.spv"),
    ),
    (
        c"fragmentMainAoBentShadowedGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentShadowedGi.spv"),
    ),
    (
        c"fragmentMainAoBentContactGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactGi.spv"),
    ),
    (
        c"fragmentMainAoBentContactShadowedGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactShadowedGi.spv"),
    ),
    (
        c"fragmentMainAoBentDecalsGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentDecalsGi.spv"),
    ),
    (
        c"fragmentMainAoBentShadowedDecalsGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentShadowedDecalsGi.spv"),
    ),
    (
        c"fragmentMainAoBentContactDecalsGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactDecalsGi.spv"),
    ),
    (
        c"fragmentMainAoBentContactShadowedDecalsGi",
        include_bytes!("../../shaders/forward.fragmentMainAoBentContactShadowedDecalsGi.spv"),
    ),
    (
        c"fragmentReflectAoBentGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentGi.spv"),
    ),
    (
        c"fragmentReflectAoBentShadowedGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentShadowedGi.spv"),
    ),
    (
        c"fragmentReflectAoBentContactGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactGi.spv"),
    ),
    (
        c"fragmentReflectAoBentContactShadowedGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactShadowedGi.spv"),
    ),
    (
        c"fragmentReflectAoBentDecalsGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentDecalsGi.spv"),
    ),
    (
        c"fragmentReflectAoBentShadowedDecalsGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentShadowedDecalsGi.spv"),
    ),
    (
        c"fragmentReflectAoBentContactDecalsGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactDecalsGi.spv"),
    ),
    (
        c"fragmentReflectAoBentContactShadowedDecalsGi",
        include_bytes!("../../shaders/forward.fragmentReflectAoBentContactShadowedDecalsGi.spv"),
    ),
    (
        c"fragmentSubsurfaceBentGi",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentGi.spv"),
    ),
    (
        c"fragmentSubsurfaceBentShadowedGi",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentShadowedGi.spv"),
    ),
    // With the sun's virtual pages (ADR 0093 step 3d).
    (
        c"fragmentSubsurfaceBentVsm",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentVsm.spv"),
    ),
    (
        c"fragmentSubsurfaceBentShadowedVsm",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentShadowedVsm.spv"),
    ),
    (
        c"fragmentSubsurfaceBentGiVsm",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentGiVsm.spv"),
    ),
    (
        c"fragmentSubsurfaceBentShadowedGiVsm",
        include_bytes!("../../shaders/forward.fragmentSubsurfaceBentShadowedGiVsm.spv"),
    ),
];

/// The bent twin's entry point of `entry`, if it reads ambient occlusion.
fn bent_entry(entry: &CStr) -> Option<CString> {
    let name = entry.to_str().ok()?;
    let bent = if let Some(rest) = name.strip_prefix("fragmentSubsurface") {
        format!("fragmentSubsurfaceBent{rest}")
    } else if name.starts_with("fragmentMainAo") || name.starts_with("fragmentReflect") {
        let at = name.find("Ao")?;
        format!("{}AoBent{}", &name[..at], &name[at + 2..])
    } else {
        return None;
    };
    CString::new(bent).ok()
}

/// A pipeline that reads ambient occlusion and its twin (`None`: it has none, logged).
struct Twin {
    /// Kept so that its id stays unique while the entry exists (as material types keep
    /// their roles): a pipeline made later at its address must not find this twin.
    _role: LazyPipeline,
    twin: Option<LazyPipeline>,
}

/// The bent-normal twins made so far, by the id of the pipeline each derives from.
#[derive(Default)]
pub(super) struct BentTwins {
    twins: HashMap<usize, Twin>,
    /// Pipelines frames asked a twin of and found none: made by [`Self::update`].
    requests: RefCell<Vec<LazyPipeline>>,
    /// Modules read so far, shared by their pipelines.
    modules: HashMap<&'static CStr, Arc<[u32]>>,
}

impl core::fmt::Debug for BentTwins {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BentTwins")
            .field("twins", &self.twins.len())
            .finish_non_exhaustive()
    }
}

impl BentTwins {
    /// What to draw with instead of `role` (a pipeline reading ambient occlusion) while
    /// the bent normals are on: its twin once made, until then `role` itself (asking for
    /// the twin).
    pub(super) fn twin<'a>(&'a self, role: &'a LazyPipeline) -> &'a LazyPipeline {
        match self.twins.get(&role.id()) {
            Some(Twin {
                twin: Some(twin), ..
            }) if twin.get().is_some() => twin,
            Some(_) => role,
            None => {
                self.requests.borrow_mut().push(role.clone());
                role
            }
        }
    }

    /// Describes the twins frames asked for; once per frame, before its passes are
    /// declared (with the material types' update).
    pub(super) fn update(&mut self, device: &Device) {
        let requests = core::mem::take(&mut *self.requests.borrow_mut());
        for role in requests {
            if self.twins.contains_key(&role.id()) {
                continue;
            }
            let twin = role
                .fragment_entry()
                .and_then(bent_entry)
                .and_then(|entry| {
                    let (name, bytes) = BENT_FRAGMENTS
                        .iter()
                        .find(|(name, _)| *name == entry.as_c_str())?;
                    let spirv = match self.modules.get(name) {
                        Some(words) => Arc::clone(words),
                        None => {
                            let words: Arc<[u32]> = Arc::from(spirv_words(bytes)?);
                            self.modules.insert(name, Arc::clone(&words));
                            words
                        }
                    };
                    role.with_fragment_entry(device, spirv, name)
                });
            if twin.is_none() {
                log::warn!(
                    "no bent-normal variant of {:?}: it draws without the bent normal",
                    role.fragment_entry()
                );
            }
            self.twins.insert(role.id(), Twin { _role: role, twin });
        }
    }

    /// `role`'s twin, described now if it was not (for statistics).
    pub(super) fn twin_now(
        &mut self,
        device: &Device,
        role: &LazyPipeline,
    ) -> Option<LazyPipeline> {
        if !self.twins.contains_key(&role.id()) {
            self.requests.borrow_mut().push(role.clone());
            self.update(device);
        }
        self.twins.get(&role.id()).and_then(|t| t.twin.clone())
    }

    /// Twins asked for and not made yet.
    pub(super) fn pending(&self) -> usize {
        let not_created = self
            .twins
            .values()
            .filter_map(|t| t.twin.as_ref())
            .filter(|t| !t.is_settled())
            .count();
        not_created + self.requests.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ambient_occlusion_entry_point_has_a_bent_twin() {
        for (name, expected) in [
            (c"fragmentMainAo", Some(c"fragmentMainAoBent")),
            (
                c"fragmentMainAoContactShadowedDecalsGi",
                Some(c"fragmentMainAoBentContactShadowedDecalsGi"),
            ),
            (
                c"fragmentReflectAoShadowed",
                Some(c"fragmentReflectAoBentShadowed"),
            ),
            (
                c"fragmentSubsurfaceShadowedGi",
                Some(c"fragmentSubsurfaceBentShadowedGi"),
            ),
            (
                c"fragmentSubsurfaceShadowedGiVsm",
                Some(c"fragmentSubsurfaceBentShadowedGiVsm"),
            ),
            (c"fragmentMain", None),
            (c"fragmentReflectGi", None),
        ] {
            assert_eq!(bent_entry(name).as_deref(), expected, "{name:?}");
        }
        for (name, bytes) in BENT_FRAGMENTS {
            assert!(spirv_words(bytes).is_some(), "{name:?}");
        }
    }
}
