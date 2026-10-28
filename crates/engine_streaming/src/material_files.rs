//! Material files at run time (ADR 0092, ADR 0120): materials made from typed `.material`
//! files without cooking, kept live in development. Their paths are read from the project's
//! root.
//!
//! [`LiveMaterialFiles::load`] reads a file, registers its type with the forward renderer,
//! lays its parameters out by the type's reflection (a Slang session of its own, opened on
//! first use) and adds the material to the table. With hot reload on, the files and their
//! types' modules are watched: when one changes, [`LiveMaterialFiles::update`] (once per
//! frame) reads the files again and replaces their materials' parameters in place. A file
//! that does not parse or lay out keeps its previous parameters; its error is returned and
//! kept ([`LiveMaterialFiles::error`]). The types' own code reloads in the forward renderer
//! (`ForwardRenderer::set_material_hot_reload`).

use engine_assets::material_file::{Alpha, MaterialFile, TextureEncoding};
use engine_platform::fs_watch::{FileEventKind, WatchSet};
use engine_render::forward::{ForwardRenderer, MaterialTypeDesc, reflect_material_params};
use engine_render::material::{
    AlphaMode, MaterialDesc, MaterialId, MaterialParams, MaterialTable, PARAMS_BYTES, TextureRef,
};
use engine_scene::AssetPaths;
use engine_shader::{Compiler, SlangLibrary, StructLayout, path_key};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a changed file must stay unchanged before it is read again.
const QUIET: Duration = Duration::from_millis(30);

/// Resolves a material file's texture parameter (its image and encoding) to a texture of
/// the pool; `None` leaves the field without a texture (white).
pub type TextureResolver<'a> = dyn FnMut(&Path, TextureEncoding) -> Option<TextureRef> + 'a;

struct LiveFile {
    path: PathBuf,
    /// [`path_key`]s of the file and of its type's module.
    keys: Vec<PathBuf>,
    material: MaterialId,
    /// The glTF look given at load (the file's `fallback` overrides it).
    fallback: MaterialDesc,
    changed: Option<Instant>,
    error: Option<String>,
}

/// Materials from material files, kept live (see the module documentation).
pub struct LiveMaterialFiles {
    shader_dir: PathBuf,
    /// The project's root, which the files' asset references are read from.
    root: PathBuf,
    compiler: Option<Compiler>,
    files: Vec<LiveFile>,
    watch: Option<WatchSet>,
    /// Types' parameter layouts by module ([`path_key`]) and type: reflected again only
    /// when the module changes (a file's edit costs no compile).
    layouts: Vec<(PathBuf, String, StructLayout)>,
}

impl core::fmt::Debug for LiveMaterialFiles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LiveMaterialFiles")
            .field("files", &self.files.len())
            .field("hot_reload", &self.watch.is_some())
            .finish_non_exhaustive()
    }
}

impl LiveMaterialFiles {
    /// Materials whose types compile from `forward.slang` in `shader_dir` (the engine's,
    /// `engine_render::forward::ENGINE_SHADER_DIR`), from files of the project in `root`
    /// (their shaders' and textures' paths are read from it).
    pub fn new(shader_dir: &Path, root: &Path) -> Self {
        Self {
            shader_dir: shader_dir.to_owned(),
            root: root.to_owned(),
            compiler: None,
            files: Vec::new(),
            watch: None,
            layouts: Vec::new(),
        }
    }

    /// Watches the loaded files and their types' modules (see the module documentation).
    pub fn set_hot_reload(&mut self, enabled: bool) {
        if !enabled {
            self.watch = None;
            return;
        }
        if self.watch.is_none() {
            let mut watch = WatchSet::new();
            for file in &self.files {
                for key in &file.keys {
                    watch_folder(&mut watch, key);
                }
            }
            self.watch = Some(watch);
        }
    }

    /// A material from the file at `path`, added to `table` (see the module
    /// documentation); `fallback` is its glTF look unless the file sets one.
    pub fn load(
        &mut self,
        path: &Path,
        fallback: MaterialDesc,
        forward: &mut ForwardRenderer,
        table: &mut MaterialTable,
        texture: &mut TextureResolver<'_>,
    ) -> Result<MaterialId, String> {
        let built = self.build(path, fallback, forward, texture)?;
        let material = table.add_typed(built.ty, built.desc, Some(built.params));
        let keys = vec![path_key(path), path_key(&built.module)];
        if let Some(watch) = &mut self.watch {
            for key in &keys {
                watch_folder(watch, key);
            }
        }
        self.files.push(LiveFile {
            path: path.to_owned(),
            keys,
            material,
            fallback,
            changed: None,
            error: None,
        });
        Ok(material)
    }

    /// Files that changed (with hot reload on), read again once they are quiet: their
    /// materials' type, parameters and look replaced in `table`. Returns each file read
    /// and how it went; a file that failed keeps its previous parameters.
    pub fn update(
        &mut self,
        forward: &mut ForwardRenderer,
        table: &mut MaterialTable,
        texture: &mut TextureResolver<'_>,
    ) -> Vec<(PathBuf, Result<(), String>)> {
        let Some(watch) = &self.watch else {
            return Vec::new();
        };
        let now = Instant::now();
        while let Some(event) = watch.try_next() {
            let key = path_key(&event.path);
            // A module changed: its types' layouts may have.
            self.layouts
                .retain(|(module, ..)| event.kind != FileEventKind::Rescan && *module != key);
            for file in &mut self.files {
                if event.kind == FileEventKind::Rescan || file.keys.contains(&key) {
                    file.changed = Some(now);
                }
            }
        }
        let mut read = Vec::new();
        for k in 0..self.files.len() {
            let file = &self.files[k];
            if file.changed.is_none_or(|at| now.duration_since(at) < QUIET) {
                continue;
            }
            let (path, fallback, material) = (file.path.clone(), file.fallback, file.material);
            let result = self.build(&path, fallback, forward, texture).map(|built| {
                table.set_typed(material, built.ty, built.desc, Some(built.params));
                built.module
            });
            let file = &mut self.files[k];
            file.changed = None;
            match result {
                Ok(module) => {
                    file.error = None;
                    // The type may now come from another module.
                    let key = path_key(&module);
                    if !file.keys.contains(&key) {
                        if let Some(watch) = &mut self.watch {
                            watch_folder(watch, &key);
                        }
                        file.keys.push(key);
                    }
                    read.push((path, Ok(())));
                }
                Err(e) => {
                    file.error = Some(e.clone());
                    read.push((path, Err(e)));
                }
            }
        }
        read
    }

    /// The latest error of the file at `path` (its material keeps its previous
    /// parameters), if its last read failed.
    pub fn error(&self, path: &Path) -> Option<&str> {
        let key = path_key(path);
        self.files
            .iter()
            .find(|f| f.keys.first() == Some(&key))?
            .error
            .as_deref()
    }

    /// Reads, lays out and resolves the file at `path`.
    fn build(
        &mut self,
        path: &Path,
        fallback: MaterialDesc,
        forward: &mut ForwardRenderer,
        texture: &mut TextureResolver<'_>,
    ) -> Result<Built, String> {
        let (source, report): (MaterialFile, _) =
            MaterialFile::load(path).map_err(|e| e.to_string())?;
        let paths: &AssetPaths = &report.assets;
        let file = source
            .typed(paths, &self.root)
            .map_err(|e| format!("{}: {}", path.display(), e.message))?
            .ok_or_else(|| {
                format!(
                    "{}: the standard material, not a material type's instance",
                    path.display()
                )
            })?;
        let desc = MaterialTypeDesc {
            name: file.type_name.clone(),
            module: file.shader.clone(),
            type_name: file.type_name.clone(),
        };
        let module_key = path_key(&desc.module);
        let cached = self
            .layouts
            .iter()
            .position(|(m, t, _)| *m == module_key && *t == desc.type_name);
        let k = match cached {
            Some(k) => k,
            None => {
                if self.compiler.is_none() {
                    let library = SlangLibrary::load().map_err(|e| e.to_string())?;
                    self.compiler = Some(Compiler::new(library).map_err(|e| e.to_string())?);
                }
                let Some(compiler) = self.compiler.as_mut() else {
                    return Err(String::from("no Slang compiler"));
                };
                let layout = reflect_material_params(compiler, &self.shader_dir, &desc)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                self.layouts
                    .push((module_key, desc.type_name.clone(), layout));
                self.layouts.len() - 1
            }
        };
        let layout = &self.layouts[k].2;
        let built = file
            .build_params(layout)
            .map_err(|e| format!("{}: {}", path.display(), e.message))?;
        if built.bytes.len() > PARAMS_BYTES {
            return Err(format!(
                "{}: `{}` is {} bytes; parameters are at most {PARAMS_BYTES}",
                path.display(),
                layout.name,
                built.bytes.len()
            ));
        }
        let textures = built
            .textures
            .iter()
            .filter_map(|t| Some((t.offset, texture(&t.path, t.encoding)?)))
            .collect();
        let mut look = match file.fallback {
            Some(f) => MaterialDesc {
                base_color: f.base_color,
                metallic: f.metallic,
                roughness: f.roughness,
                emissive: f.emissive,
                ..fallback
            },
            None => fallback,
        };
        look.alpha = match file.alpha {
            Alpha::Opaque => AlphaMode::Opaque,
            Alpha::Mask => AlphaMode::Mask {
                cutoff: file.alpha_cutoff,
            },
            Alpha::Blend => AlphaMode::Blend,
        };
        look.double_sided = file.double_sided;
        let module = desc.module.clone();
        let ty = forward.register_material_type(desc);
        Ok(Built {
            ty,
            desc: look,
            params: MaterialParams {
                bytes: built.bytes,
                textures,
            },
            module,
        })
    }
}

struct Built {
    ty: engine_render::material::MaterialTypeId,
    desc: MaterialDesc,
    params: MaterialParams,
    module: PathBuf,
}

/// Watches the folder of `key` (a [`path_key`]).
fn watch_folder(watch: &mut WatchSet, key: &Path) {
    if let Some(folder) = key.parent()
        && let Err(e) = watch.watch(folder)
    {
        log::warn!(
            "material files: cannot watch {} for hot reload: {e}",
            folder.display()
        );
    }
}
