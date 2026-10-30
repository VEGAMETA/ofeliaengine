//! Assets made in the background (ADR 0108's E4-4): glTF files read and imported, their
//! meshes put together, their textures read, decoded and given their mip levels, on a
//! thread of their own with a pool of workers below normal priority. The frame's thread
//! gives the jobs and takes back each result as soon as it is done; it makes the GPU's
//! meshes and textures from them, whose copies the pools spread over frames.
//!
//! With a derived data cache (E4-8) a texture is loaded from it compressed when an earlier
//! run stored it there. Otherwise it is made uncompressed as before, and compressed (BC7,
//! BC5 for normal maps) on a second thread whose pool runs below the loading's priority,
//! so compression never holds a load back; the result goes into the cache and comes back
//! to the frame, which swaps it in.
//!
//! Models' colliders (ADR 0117) are cooked on a third thread, below the loading's priority:
//! a prop's convex parts can take seconds, and the frame that waits for a scene's assets
//! does not wait for them.
//!
//! Each thread is started with its first job: a player whose scenes use no glTF file never
//! has them.

use crate::assets::{
    CookedColliders, GltfFile, TextureKey, cook_model_colliders, gltf_mesh, gltf_primitives,
    open_gltf,
};
use crate::skins::{Rig, SkinPart};
use engine_assets::{
    DerivedDataCache, EncodedTexture, TextureImage, TextureSource, build_mips, cached_texture,
    decode_image, import_texture, read_texture,
};
use engine_core::jobs::ThreadPool;
use engine_core::spsc;
use engine_render::mesh::{OwnedMeshData, VertexAttributes, encode_attributes};
use engine_scene::{AssetId, AssetMeta, AssetRef, TextureCompression};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Jobs a thread's queue holds at once (more wait on the frame's side).
const QUEUE: usize = 256;

/// Work for the loader. `generation` is the file's when the job was given: a file read
/// again since makes its result stale.
pub(crate) enum Job {
    /// Read and import glTF file `file` at `path` (under the root), its id checked against
    /// its `.meta`.
    Import {
        file: AssetId,
        path: String,
        generation: u32,
    },
    /// Mesh `mesh` of an imported file as one mesh.
    Mesh {
        asset: AssetRef,
        file: Arc<GltfFile>,
        mesh: usize,
        generation: u32,
    },
    /// Mesh `mesh` of an imported file as a mesh per primitive (a model with material
    /// slots, ADR 0115).
    Primitives {
        asset: AssetRef,
        file: Arc<GltfFile>,
        mesh: usize,
        generation: u32,
    },
    /// Mesh `mesh` of an imported file skinned (ADR 0124's E12-1a): its parts (the whole
    /// mesh, or a part per primitive with `split`) with their influences, and its rig.
    Skin {
        asset: AssetRef,
        split: bool,
        file: Arc<GltfFile>,
        mesh: usize,
        generation: u32,
    },
    /// A texture of an imported file read, decoded and given its levels for its use, or
    /// loaded compressed from `cache` when an earlier run stored it there.
    Texture {
        key: TextureKey,
        file: Arc<GltfFile>,
        cache: Option<Arc<DerivedDataCache>>,
        generation: u32,
    },
    /// An image file (a material type's texture parameter, ADR 0120's E10-1b) read,
    /// decoded and given its levels for its use, or loaded compressed from `cache`.
    Image {
        key: TextureKey,
        path: PathBuf,
        cache: Option<Arc<DerivedDataCache>>,
        generation: u32,
    },
    /// A texture made uncompressed, compressed into `cache` (on the compressing thread):
    /// its image read again from `source`, beside the glTF file in `dir`.
    Compress {
        key: TextureKey,
        source: Source,
        cache: Arc<DerivedDataCache>,
        generation: u32,
    },
    /// The colliders of every mesh of glTF file `file` (on the cooking thread), convex
    /// parts from `cache` when it has them.
    Colliders {
        file: AssetId,
        gltf: Arc<GltfFile>,
        cache: Option<Arc<DerivedDataCache>>,
        generation: u32,
    },
}

/// Where a texture's image is read from: the glTF file's texture, and the file's folder
/// (its file itself not kept: a texture waiting to be compressed holds its image only).
pub(crate) struct Source {
    texture: TextureSource,
    dir: PathBuf,
}

/// A texture as the loader made it.
pub(crate) enum Made {
    /// Decoded and given its levels, uncompressed. `compress`: where its image is, to be
    /// compressed from, when there is a cache (which did not have it).
    Decoded {
        image: TextureImage,
        compress: Option<Source>,
    },
    /// Compressed, from the cache.
    Cached(EncodedTexture),
}

/// A job's result.
pub(crate) enum Done {
    Imported {
        file: AssetId,
        generation: u32,
        result: Result<Arc<GltfFile>, String>,
    },
    /// The mesh and its vertices' attributes encoded as the geometry pool keeps them.
    Mesh {
        asset: AssetRef,
        generation: u32,
        result: Result<(OwnedMeshData, Vec<VertexAttributes>), String>,
    },
    /// Each primitive's, in the mesh's order.
    Primitives {
        asset: AssetRef,
        generation: u32,
        result: Result<Vec<(OwnedMeshData, Vec<VertexAttributes>)>, String>,
    },
    /// A skinned mesh's parts and rig.
    Skin {
        asset: AssetRef,
        split: bool,
        generation: u32,
        result: Result<(Vec<SkinPart>, Arc<Rig>), String>,
    },
    Texture {
        key: TextureKey,
        generation: u32,
        result: Result<Made, String>,
    },
    Compressed {
        key: TextureKey,
        generation: u32,
        result: Result<EncodedTexture, String>,
    },
    Colliders {
        file: AssetId,
        generation: u32,
        result: Result<CookedColliders, String>,
    },
}

/// What a thread of the loader does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Reads, imports and decodes: each job on a worker of its pool.
    Load,
    /// Compresses textures two at a time, each over every worker of its pool (one reads
    /// and filters its levels while the other's blocks keep the workers busy), the thread
    /// helping; it runs below the loading's priority.
    Compress,
    /// Cooks colliders: each job on a worker of its pool, below the loading's priority.
    Cook,
}

/// The loader: its threads, the jobs going to them and the results coming back.
pub(crate) struct Loader {
    root: PathBuf,
    loading: Lane,
    compressing: Lane,
    cooking: Lane,
}

/// A thread of the loader, its jobs and its results.
struct Lane {
    kind: Kind,
    running: Option<Running>,
    /// Jobs not yet in the queue (it was full).
    backlog: VecDeque<Job>,
    /// Jobs given and not yet taken back.
    in_flight: usize,
    /// No thread could start: jobs are done where they are given (compression fails),
    /// their results kept here.
    here: Option<VecDeque<Done>>,
}

struct Running {
    jobs: spsc::Producer<Job>,
    done: mpsc::Receiver<Done>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loader {
    /// A loader for files under `root` (its threads not started yet).
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            loading: Lane::new(Kind::Load),
            compressing: Lane::new(Kind::Compress),
            cooking: Lane::new(Kind::Cook),
        }
    }

    /// Gives `job` to the loader: compression to its compressing thread, colliders to its
    /// cooking thread, the rest to its loading thread.
    pub(crate) fn give(&mut self, job: Job) {
        let lane = match job {
            Job::Compress { .. } => &mut self.compressing,
            Job::Colliders { .. } => &mut self.cooking,
            _ => &mut self.loading,
        };
        lane.in_flight += 1;
        lane.backlog.push_back(job);
        lane.send(&self.root);
    }

    /// Whether loading jobs are given and not taken back (compression and colliders
    /// aside).
    pub(crate) fn busy(&self) -> bool {
        self.loading.in_flight > 0
    }

    /// Textures given to be compressed and not taken back.
    pub(crate) fn compressing(&self) -> usize {
        self.compressing.in_flight
    }

    /// Appends the results come back since the last call to `out`.
    pub(crate) fn take(&mut self, out: &mut Vec<Done>) {
        self.loading.take(out, &self.root);
        self.compressing.take(out, &self.root);
        self.cooking.take(out, &self.root);
    }

    /// Waits a moment for loading results (the frame waiting for its assets): until one
    /// comes or a fraction of a millisecond passes.
    pub(crate) fn wait(&mut self, out: &mut Vec<Done>) {
        if let Some(running) = &self.loading.running
            && let Ok(done) = running
                .done
                .recv_timeout(std::time::Duration::from_micros(500))
        {
            self.loading.in_flight -= 1;
            out.push(done);
        }
        self.take(out);
    }
}

impl Lane {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            running: None,
            backlog: VecDeque::new(),
            in_flight: 0,
            here: None,
        }
    }

    /// Appends the lane's results come back to `out`.
    fn take(&mut self, out: &mut Vec<Done>, root: &Path) {
        if let Some(running) = &self.running {
            while let Ok(done) = running.done.try_recv() {
                self.in_flight -= 1;
                out.push(done);
            }
        }
        if let Some(here) = &mut self.here {
            self.in_flight -= here.len();
            out.extend(here.drain(..));
        }
        self.send(root);
    }

    /// The backlog into the queue, the thread started for the first job.
    fn send(&mut self, root: &Path) {
        if self.backlog.is_empty() {
            return;
        }
        if let Some(here) = &mut self.here {
            for job in self.backlog.drain(..) {
                here.push_back(match job {
                    // Not on the frame's thread: seconds a texture, or a prop's parts.
                    Job::Compress { .. } => failed(&job, "no thread to compress it on"),
                    Job::Colliders { .. } => failed(&job, "no thread to cook them on"),
                    job => run_caught(job, root, None),
                });
            }
            return;
        }
        let running = match &mut self.running {
            Some(running) => running,
            None => match start(root.to_path_buf(), self.kind) {
                Ok(running) => self.running.insert(running),
                Err(e) => {
                    // No thread: the jobs are done where they are given.
                    log::warn!(target: "engine_app::assets", "the loading thread: {e}");
                    self.here = Some(VecDeque::new());
                    return self.send(root);
                }
            },
        };
        let mut sent = false;
        while let Some(job) = self.backlog.pop_front() {
            if let Err(job) = running.jobs.push(job) {
                self.backlog.push_front(job);
                break;
            }
            sent = true;
        }
        if sent && let Some(thread) = &running.thread {
            thread.thread().unpark();
        }
    }
}

impl Drop for Loader {
    /// The threads told to stop. The loading one is waited for (its jobs are short); the
    /// compressing and cooking ones are not: they stop after the textures they compress or
    /// the files they cook, which can take seconds, and an exit does not wait for them (a
    /// cache entry is written whole or not at all, ADR 0041).
    fn drop(&mut self) {
        for lane in [&self.loading, &self.compressing, &self.cooking] {
            if let Some(running) = &lane.running {
                running.stop.store(true, Ordering::Release);
                if let Some(thread) = &running.thread {
                    thread.thread().unpark();
                }
            }
        }
        if let Some(thread) = self
            .loading
            .running
            .as_mut()
            .and_then(|running| running.thread.take())
        {
            let _ = thread.join();
        }
    }
}

/// A loader's thread: takes every job given, runs them on its pool and sends each result
/// back once done; parks while there is nothing to do. The compressing thread stops
/// between two pairs of textures when the loader goes.
fn start(root: PathBuf, kind: Kind) -> std::io::Result<Running> {
    let (name, priority) = match kind {
        Kind::Load => ("engine-load", Priority::BelowNormal),
        Kind::Compress => ("engine-compress", Priority::Lowest),
        Kind::Cook => ("engine-cook", Priority::Lowest),
    };
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(1, 8);
    let pool = ThreadPool::builder()
        .workers(workers)
        .thread_name(name)
        .on_thread_start(move |_| set_priority(priority))
        .build()
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    let (jobs, mut incoming) = spsc::channel::<Job>(QUEUE);
    let (done, results) = mpsc::channel::<Done>();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(String::from(name))
        .spawn(move || {
            set_priority(priority);
            if kind == Kind::Compress {
                // It helps while it waits for its textures. Not registered: it waits idle.
                let _ = pool.register_current_thread();
            }
            let mut batch = Vec::new();
            loop {
                while let Some(job) = incoming.pop() {
                    batch.push(job);
                }
                if batch.is_empty() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::park();
                    continue;
                }
                let (root, done) = (&root, &done);
                match kind {
                    Kind::Load | Kind::Cook => pool.scope(|s| {
                        for job in batch.drain(..) {
                            s.spawn(move || {
                                // The frame's side gone: nobody to tell.
                                let _ = done.send(run_caught(job, root, None));
                            });
                        }
                    }),
                    Kind::Compress => {
                        while !batch.is_empty() && !stopping.load(Ordering::Acquire) {
                            let pool = &pool;
                            let two = batch.len().min(2);
                            pool.scope(|s| {
                                for job in batch.drain(..two) {
                                    s.spawn(move || {
                                        let _ = done.send(run_caught(job, root, Some(pool)));
                                    });
                                }
                            });
                        }
                        // The loader gone: the rest is not compressed.
                        batch.clear();
                    }
                }
            }
        })?;
    Ok(Running {
        jobs,
        done: results,
        stop,
        thread: Some(thread),
    })
}

/// The failure of `job`, for `why`.
fn failed(job: &Job, why: &str) -> Done {
    let why = String::from(why);
    match job {
        Job::Import {
            file, generation, ..
        } => Done::Imported {
            file: *file,
            generation: *generation,
            result: Err(why),
        },
        Job::Mesh {
            asset, generation, ..
        } => Done::Mesh {
            asset: *asset,
            generation: *generation,
            result: Err(why),
        },
        Job::Primitives {
            asset, generation, ..
        } => Done::Primitives {
            asset: *asset,
            generation: *generation,
            result: Err(why),
        },
        Job::Skin {
            asset,
            split,
            generation,
            ..
        } => Done::Skin {
            asset: *asset,
            split: *split,
            generation: *generation,
            result: Err(why),
        },
        Job::Texture {
            key, generation, ..
        } => Done::Texture {
            key: *key,
            generation: *generation,
            result: Err(why),
        },
        Job::Image {
            key, generation, ..
        } => Done::Texture {
            key: *key,
            generation: *generation,
            result: Err(why),
        },
        Job::Compress {
            key, generation, ..
        } => Done::Compressed {
            key: *key,
            generation: *generation,
            result: Err(why),
        },
        Job::Colliders {
            file, generation, ..
        } => Done::Colliders {
            file: *file,
            generation: *generation,
            result: Err(why),
        },
    }
}

/// A job done, a panic in it (a file the importer or a decoder trips on) its failure.
/// Compression runs over `pool`'s workers when given.
fn run_caught(job: Job, root: &Path, pool: Option<&ThreadPool>) -> Done {
    let failure = failed(&job, "loading it failed (a panic, in the log)");
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(job, root, pool)))
        .unwrap_or(failure)
}

/// The encoded image at `source`.
fn texture_bytes(source: &Source) -> Result<Vec<u8>, String> {
    read_texture(&source.texture, Some(&source.dir)).map_err(|e| e.to_string())
}

/// A job done.
fn run(job: Job, root: &Path, pool: Option<&ThreadPool>) -> Done {
    match job {
        Job::Import {
            file,
            path,
            generation,
        } => {
            engine_profiler::zone!("assets::load::import");
            Done::Imported {
                file,
                generation,
                result: open_gltf(root, &path, file).map(Arc::new),
            }
        }
        Job::Mesh {
            asset,
            file,
            mesh,
            generation,
        } => {
            engine_profiler::zone!("assets::load::mesh");
            let result = file
                .scene
                .meshes
                .get(mesh)
                .map(|mesh| {
                    let data = gltf_mesh(mesh, file.meta.model.applied_scale());
                    let mut attributes = Vec::with_capacity(data.positions.len());
                    encode_attributes(&data.as_data(), &mut attributes);
                    (data, attributes)
                })
                .ok_or_else(|| String::from("the file has no such mesh"));
            Done::Mesh {
                asset,
                generation,
                result,
            }
        }
        Job::Primitives {
            asset,
            file,
            mesh,
            generation,
        } => {
            engine_profiler::zone!("assets::load::primitives");
            let scale = file.meta.model.applied_scale();
            let result = file
                .scene
                .meshes
                .get(mesh)
                .map(|mesh| {
                    mesh.primitives
                        .iter()
                        .map(|p| {
                            let data = gltf_primitives(core::slice::from_ref(p), scale);
                            let mut attributes = Vec::with_capacity(data.positions.len());
                            encode_attributes(&data.as_data(), &mut attributes);
                            (data, attributes)
                        })
                        .collect()
                })
                .ok_or_else(|| String::from("the file has no such mesh"));
            Done::Primitives {
                asset,
                generation,
                result,
            }
        }
        Job::Skin {
            asset,
            split,
            file,
            mesh,
            generation,
        } => {
            engine_profiler::zone!("assets::load::skin");
            let result = crate::skins::build(&file, mesh, split).and_then(|built| {
                built
                    .map(|(parts, rig)| (parts, Arc::new(rig)))
                    .ok_or_else(|| String::from("no node draws the mesh with a skin"))
            });
            Done::Skin {
                asset,
                split,
                generation,
                result,
            }
        }
        Job::Image {
            key,
            path,
            cache,
            generation,
        } => {
            engine_profiler::zone!("assets::load::image");
            let result = std::fs::read(&path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|bytes| {
                    // The image's own settings (E4-9): one kept uncompressed skips the cache.
                    let settings = AssetMeta::read(&path);
                    let cache = cache.filter(|_| {
                        settings.texture.compression != TextureCompression::Uncompressed
                    });
                    if let Some(texture) = cache
                        .as_deref()
                        .and_then(|cache| cached_texture(cache, &bytes, key.usage()))
                    {
                        return Ok(Made::Cached(texture));
                    }
                    let image = decode_image(&bytes).map_err(|e| e.to_string())?;
                    let compress = cache.map(|_| Source {
                        texture: TextureSource::Uri(
                            path.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                        dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    });
                    Ok(Made::Decoded {
                        image: build_mips(&image, key.usage()),
                        compress,
                    })
                });
            Done::Texture {
                key,
                generation,
                result,
            }
        }
        Job::Texture {
            key,
            file,
            cache,
            generation,
        } => {
            engine_profiler::zone!("assets::load::texture");
            let result = file
                .scene
                .textures
                .get(key.texture)
                .ok_or_else(|| String::from("the file has no such texture"))
                .and_then(|texture| {
                    let bytes = read_texture(&texture.source, Some(&file.dir))
                        .map_err(|e| e.to_string())?;
                    // An image file's own settings, or its model's for one inside it (E4-9):
                    // one kept uncompressed skips the cache.
                    let settings = match &texture.source {
                        TextureSource::Uri(uri) => AssetMeta::read(&file.dir.join(uri)).texture,
                        TextureSource::Embedded { .. } => file.meta.texture,
                    };
                    let cache =
                        cache.filter(|_| settings.compression != TextureCompression::Uncompressed);
                    if let Some(texture) = cache
                        .as_deref()
                        .and_then(|cache| cached_texture(cache, &bytes, key.usage()))
                    {
                        return Ok(Made::Cached(texture));
                    }
                    let image = decode_image(&bytes).map_err(|e| e.to_string())?;
                    let compress = cache.map(|_| Source {
                        texture: match &texture.source {
                            TextureSource::Uri(uri) => TextureSource::Uri(uri.clone()),
                            // Its image read already: kept, without its file.
                            TextureSource::Embedded { mime, .. } => TextureSource::Embedded {
                                bytes,
                                mime: mime.clone(),
                            },
                        },
                        dir: file.dir.clone(),
                    });
                    Ok(Made::Decoded {
                        image: build_mips(&image, key.usage()),
                        compress,
                    })
                });
            Done::Texture {
                key,
                generation,
                result,
            }
        }
        Job::Compress {
            key,
            source,
            cache,
            generation,
        } => {
            engine_profiler::zone!("assets::load::compress");
            // A texture compressed meanwhile (the same image in another file, or by
            // another run) is the cache's.
            let result = texture_bytes(&source).and_then(|bytes| {
                import_texture(&bytes, key.usage(), Some(&cache), pool)
                    .map(|(texture, _)| texture)
                    .map_err(|e| e.to_string())
            });
            Done::Compressed {
                key,
                generation,
                result,
            }
        }
        Job::Colliders {
            file,
            gltf,
            cache,
            generation,
        } => {
            engine_profiler::zone!("assets::load::colliders");
            Done::Colliders {
                file,
                generation,
                result: cook_model_colliders(&gltf, cache.as_deref()),
            }
        }
    }
}

/// A thread's priority on Windows.
#[derive(Clone, Copy)]
enum Priority {
    BelowNormal,
    Lowest,
}

fn set_priority(priority: Priority) {
    #[cfg(windows)]
    {
        use engine_platform::thread::{ThreadPriority, set_current_thread_priority};
        let _ = set_current_thread_priority(match priority {
            Priority::BelowNormal => ThreadPriority::BelowNormal,
            Priority::Lowest => ThreadPriority::Lowest,
        });
    }
    #[cfg(not(windows))]
    let _ = priority;
}
