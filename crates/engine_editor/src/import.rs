//! The inspector's import settings (ADR 0108's E4-9; the asset browser research's "import
//! settings in the inspector"). A file picked in the FileSystem dock is shown in the
//! inspector instead of the entities, as Godot's inspector shows a resource, until the
//! entity selection changes. Its importer's settings live in its `.meta`: an edit writes
//! it at once and the asset is made again in the background, with nothing to press. A
//! `.meta` is a file on disk, as a rename is, so an edit is not an undo step; ↺ puts a
//! setting back.

use engine_ecs::Entity;
use engine_scene::{AssetId, AssetKind, AssetMeta, TextureCompression};

/// The smallest and largest import scale.
pub(crate) const SCALES: (f32, f32) = (0.0001, 10000.0);

/// The compressions' names, in [`TextureCompression`]'s order.
pub(crate) const COMPRESSIONS: [&str; 2] = ["Auto", "Uncompressed"];

/// An import setting the inspector shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    /// A model's scale.
    Scale,
    /// An image's compression, or a model's for the images inside it.
    Compression,
}

impl Setting {
    /// Its field's name for an asset of `kind`.
    pub(crate) fn name(self, kind: AssetKind) -> &'static str {
        match (self, kind) {
            (Self::Scale, _) => "scale",
            (Self::Compression, AssetKind::Model) => "texture compression",
            (Self::Compression, _) => "compression",
        }
    }
}

/// The settings of a kind of asset, in the order shown.
pub(crate) fn settings(kind: AssetKind) -> &'static [Setting] {
    match kind {
        AssetKind::Model => &[Setting::Scale, Setting::Compression],
        AssetKind::Texture => &[Setting::Compression],
        _ => &[],
    }
}

/// A scale dragged `travel` logical pixels sideways from `start`: doubled or halved every
/// 100 pixels, as a multiplier is, within [`SCALES`].
pub(crate) fn dragged_scale(start: f32, travel: f32) -> f32 {
    (start * (travel / 100.0).exp2()).clamp(SCALES.0, SCALES.1)
}

/// The asset the inspector shows.
#[derive(Debug)]
pub(crate) struct Inspected {
    pub(crate) id: AssetId,
    pub(crate) path: String,
    pub(crate) kind: AssetKind,
    /// Its settings as shown: a scale being dragged as it is dragged.
    pub(crate) meta: AssetMeta,
    /// Its settings as its `.meta` holds them.
    pub(crate) saved: AssetMeta,
    /// The entity selection when it was picked: once that changes, the entities are shown.
    pub(crate) selection: Vec<Entity>,
    /// A scale being dragged: its value at the press, and whether it moved.
    pub(crate) drag: Option<(f32, bool)>,
    /// The scale is being typed.
    pub(crate) typing: bool,
    /// The choice whose list is open.
    pub(crate) open: Option<Setting>,
    /// The scale's text, as shown.
    pub(crate) scale_text: String,
}

impl Inspected {
    /// Asset `id`, a file of `kind` at `path`, with the settings `meta`, picked while
    /// `selection` was selected.
    pub(crate) fn new(
        id: AssetId,
        path: String,
        kind: AssetKind,
        meta: AssetMeta,
        selection: Vec<Entity>,
    ) -> Self {
        let mut inspected = Self {
            id,
            path,
            kind,
            meta: meta.clone(),
            saved: meta,
            selection,
            drag: None,
            typing: false,
            open: None,
            scale_text: String::new(),
        };
        inspected.refresh();
        inspected
    }

    /// The texts shown, from the settings.
    pub(crate) fn refresh(&mut self) {
        self.scale_text.clear();
        let _ = std::fmt::Write::write_fmt(
            &mut self.scale_text,
            format_args!("{}", self.meta.model.scale),
        );
    }

    /// The current choice of setting `s` (a choice).
    pub(crate) fn choice(&self, s: Setting) -> &'static str {
        match s {
            Setting::Compression => COMPRESSIONS[self.meta.texture.compression as usize],
            Setting::Scale => "",
        }
    }

    /// Whether setting `s` is not its default (a reset button follows it).
    pub(crate) fn changed(&self, s: Setting) -> bool {
        let defaults = AssetMeta::default();
        match s {
            Setting::Scale => self.meta.model.scale != defaults.model.scale,
            Setting::Compression => self.meta.texture.compression != defaults.texture.compression,
        }
    }
}

/// `meta` with setting `s` back to its default.
pub(crate) fn reset(meta: AssetMeta, s: Setting) -> AssetMeta {
    let defaults = AssetMeta::default();
    match s {
        Setting::Scale => AssetMeta {
            model: defaults.model,
            ..meta
        },
        Setting::Compression => AssetMeta {
            texture: defaults.texture,
            ..meta
        },
    }
}

/// `meta` with compression choice `i` ([`COMPRESSIONS`]).
pub(crate) fn chosen(meta: AssetMeta, i: usize) -> AssetMeta {
    let mut meta = meta;
    meta.texture.compression = if i == 1 {
        TextureCompression::Uncompressed
    } else {
        TextureCompression::Auto
    };
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scale_doubles_every_hundred_pixels() {
        assert_eq!(dragged_scale(1.0, 100.0), 2.0);
        assert_eq!(dragged_scale(1.0, -100.0), 0.5);
        assert_eq!(dragged_scale(0.01, 0.0), 0.01);
        assert_eq!(dragged_scale(1.0, -100_000.0), SCALES.0);
    }

    #[test]
    fn settings_reset_one_at_a_time() {
        let meta = chosen(AssetMeta::default(), 1);
        let meta = AssetMeta {
            model: engine_scene::ModelImport {
                scale: 0.01,
                ..engine_scene::ModelImport::default()
            },
            ..meta
        };
        let back = reset(meta.clone(), Setting::Scale);
        assert_eq!(back.model.scale, 1.0);
        assert_eq!(
            back.texture.compression,
            TextureCompression::Uncompressed,
            "the other kept"
        );
        let mut shown = Inspected::new(
            AssetId::new(),
            String::from("props/crate.glb"),
            AssetKind::Model,
            meta,
            Vec::new(),
        );
        assert_eq!(shown.scale_text, "0.01");
        assert!(shown.changed(Setting::Scale) && shown.changed(Setting::Compression));
        assert_eq!(shown.choice(Setting::Compression), "Uncompressed");
        shown.meta = back;
        shown.refresh();
        assert_eq!(shown.scale_text, "1");
        assert_eq!(settings(AssetKind::Sound), []);
    }
}
