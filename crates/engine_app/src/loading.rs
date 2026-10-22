//! Assets made in the background (ADR 0108's E4-4): glTF files read and imported, their
//! meshes put together, their textures read, decoded and given their mip levels, on a
//! thread of their own with a pool of workers below normal priority. The frame's thread
//! gives the jobs and takes back each result as soon as it is done; it makes the GPU's
//! meshes and textures from them, whose copies the pools spread over frames.
//!
//! The thread is started with the first job: a player whose scenes use no glTF file never
//! has it.

use crate::assets::{GltfFile, TextureKey, gltf_mesh, open_gltf};
use engine_assets::{TextureImage, build_mips, decode_image, read_texture};
use engine_core::jobs::ThreadPool;
use engine_core::spsc;
use engine_render::mesh::{OwnedMeshData, VertexAttributes, encode_attributes};
use engine_scene::{AssetId, AssetRef};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

/// Jobs the loader's queue holds at once (more wait on the frame's side).
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
    /// A texture of an imported file read, decoded and given its levels for its use.
    Texture {
        key: TextureKey,
        file: Arc<GltfFile>,
        generation: u32,
    },
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
    Texture {
        key: TextureKey,
        generation: u32,
        result: Result<TextureImage, String>,
    },
}

/// The loader: its thread, the jobs going to it and the results coming back.
pub(crate) struct Loader {
    root: PathBuf,
    running: Option<Running>,
    /// Jobs not yet in the queue (it was full).
    backlog: VecDeque<Job>,
    /// Jobs given and not yet taken back.
    in_flight: usize,
    /// No thread could start: jobs are done where they are given, their results kept here.
    here: Option<VecDeque<Done>>,
}

struct Running {
    jobs: spsc::Producer<Job>,
    done: mpsc::Receiver<Done>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loader {
    /// A loader for files under `root` (its thread not started yet).
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            running: None,
            backlog: VecDeque::new(),
            in_flight: 0,
            here: None,
        }
    }

    /// Gives `job` to the loader.
    pub(crate) fn give(&mut self, job: Job) {
        self.in_flight += 1;
        self.backlog.push_back(job);
        self.send();
    }

    /// Whether jobs are given and not taken back.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight > 0
    }

    /// Appends the results come back since the last call to `out`.
    pub(crate) fn take(&mut self, out: &mut Vec<Done>) {
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
        self.send();
    }

    /// Waits a moment for results (the frame waiting for its assets): until one comes or
    /// a fraction of a millisecond passes.
    pub(crate) fn wait(&mut self, out: &mut Vec<Done>) {
        if let Some(running) = &self.running
            && let Ok(done) = running
                .done
                .recv_timeout(std::time::Duration::from_micros(500))
        {
            self.in_flight -= 1;
            out.push(done);
        }
        self.take(out);
    }

    /// The backlog into the queue, the thread started for the first job.
    fn send(&mut self) {
        if self.backlog.is_empty() {
            return;
        }
        if let Some(here) = &mut self.here {
            for job in self.backlog.drain(..) {
                here.push_back(run_caught(job, &self.root));
            }
            return;
        }
        let running = match &mut self.running {
            Some(running) => running,
            None => match start(self.root.clone()) {
                Ok(running) => self.running.insert(running),
                Err(e) => {
                    // No thread: the jobs are done where they are given.
                    log::warn!(target: "engine_app::assets", "the loading thread: {e}");
                    self.here = Some(VecDeque::new());
                    return self.send();
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
    fn drop(&mut self) {
        if let Some(mut running) = self.running.take() {
            running.stop.store(true, Ordering::Release);
            if let Some(thread) = running.thread.take() {
                thread.thread().unpark();
                let _ = thread.join();
            }
        }
    }
}

/// The loading thread: takes every job given, runs them on its pool and sends each result
/// back once done; parks while there is nothing to do.
fn start(root: PathBuf) -> std::io::Result<Running> {
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(1, 8);
    let pool = ThreadPool::builder()
        .workers(workers)
        .thread_name("engine-load")
        .on_thread_start(|_| lower_priority())
        .build()
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    let (jobs, mut incoming) = spsc::channel::<Job>(QUEUE);
    let (done, results) = mpsc::channel::<Done>();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(String::from("engine-load"))
        .spawn(move || {
            lower_priority();
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
                pool.scope(|s| {
                    for job in batch.drain(..) {
                        s.spawn(move || {
                            // The frame's side gone: nobody to tell.
                            let _ = done.send(run_caught(job, root));
                        });
                    }
                });
            }
        })?;
    Ok(Running {
        jobs,
        done: results,
        stop,
        thread: Some(thread),
    })
}

/// A job done, a panic in it (a file the importer or a decoder trips on) its failure.
fn run_caught(job: Job, root: &Path) -> Done {
    let failed = |why: String| match &job {
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
        Job::Texture {
            key, generation, ..
        } => Done::Texture {
            key: *key,
            generation: *generation,
            result: Err(why),
        },
    };
    let failure = failed(String::from("loading it failed (a panic, in the log)"));
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(job, root))).unwrap_or(failure)
}

/// A job done.
fn run(job: Job, root: &Path) -> Done {
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
                    let data = gltf_mesh(mesh);
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
        Job::Texture {
            key,
            file,
            generation,
        } => {
            engine_profiler::zone!("assets::load::texture");
            let result = file
                .scene
                .textures
                .get(key.texture)
                .ok_or_else(|| String::from("the file has no such texture"))
                .and_then(|t| read_texture(&t.source, Some(&file.dir)).map_err(|e| e.to_string()))
                .and_then(|bytes| decode_image(&bytes).map_err(|e| e.to_string()))
                .map(|image| build_mips(&image, key.usage()));
            Done::Texture {
                key,
                generation,
                result,
            }
        }
    }
}

fn lower_priority() {
    #[cfg(windows)]
    let _ = engine_platform::thread::set_current_thread_priority(
        engine_platform::thread::ThreadPriority::BelowNormal,
    );
}
