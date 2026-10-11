//! Global allocator comparison for the decision CLAUDE.md §5 asks for (review 2 §3.9,
//! `docs/research/global-allocator.md`): the system heap against mimalloc 2 and 3 and
//! snmalloc, one workload per process so that its peak memory is its own.
//!
//! `cargo run --release --manifest-path bench/alloc-compare/Cargo.toml
//! [--features mimalloc-v2|mimalloc-v3|snmalloc] -- <workload> [reps]`
//!
//! Workloads:
//! - `churn`: one thread, 20 M allocations of 16 B–16 KiB (mostly small), 4,096 of them
//!   alive, the oldest freed first;
//! - `parallel`: the same on every hardware thread at once (5 M each);
//! - `handoff`: allocated on four threads, freed on four others (job results, messages);
//! - `import`: Sponza's glTF imported (JSON, buffers, the scene's tables);
//! - `cook`: Sponza cooked from a warm derived data cache and written to a pack;
//! - `load`: that pack loaded (entries read, decompressed, textures decoded);
//! - `load-collect`: the load, the scene dropped, the allocator asked to give memory back
//!   on the main thread; `load-collect-all`: on every worker of the pool too (the memory
//!   still held is printed after the last repetition).
//!
//! `--features segment-heap` builds the system heap with the process heap made a segment
//! heap (`build.rs`, `segment-heap.manifest`).
//!
//! Prints one line: workload, allocator, repetitions, median and fastest milliseconds,
//! peak working set and peak committed memory in MiB.

use engine_assets::{
    CookSettings, DerivedDataCache, cook_scene, import_gltf, load_pack, write_pack,
};
use engine_core::jobs::ThreadPool;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(any(feature = "mimalloc-v2", feature = "mimalloc-v3"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "snmalloc")]
#[global_allocator]
static GLOBAL: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

#[cfg(any(feature = "mimalloc-v2", feature = "mimalloc-v3"))]
unsafe extern "C" {
    // Part of mimalloc's C API in every build of the library.
    fn mi_collect(force: bool);
}

/// `mi_collect(force)` where mimalloc is the allocator.
fn collect_with(force: bool) {
    #[cfg(any(feature = "mimalloc-v2", feature = "mimalloc-v3"))]
    // SAFETY: a plain call into the linked mimalloc.
    unsafe {
        mi_collect(force);
    }
    let _ = force;
}

/// Gives the allocator's freed memory back to the OS where it can (mimalloc).
fn collect() {
    #[cfg(any(feature = "mimalloc-v2", feature = "mimalloc-v3"))]
    // SAFETY: a plain call into the linked mimalloc.
    unsafe {
        mi_collect(true);
    }
}

/// Working set and committed private memory now, MiB.
fn memory_now() -> (f64, f64) {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        // SAFETY: plain data.
        ..unsafe { core::mem::zeroed() }
    };
    // SAFETY: the pseudo-handle of this process and a counters structure of the given size.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), counters.cb) };
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    (mib(counters.WorkingSetSize), mib(counters.PrivateUsage))
}

fn allocator() -> &'static str {
    if cfg!(feature = "snmalloc") {
        "snmalloc-0.7.6"
    } else if cfg!(feature = "mimalloc-v2") {
        "mimalloc-2.3.2"
    } else if cfg!(feature = "mimalloc-v3") {
        "mimalloc-3.3.2"
    } else if cfg!(feature = "segment-heap") {
        "segment-heap"
    } else {
        "system"
    }
}

/// xorshift64: the same sizes for every allocator.
struct Sizes(u64);

impl Sizes {
    fn next(&mut self) -> usize {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // 70 % 16–128 B, 25 % 128 B–1 KiB, 5 % 1–16 KiB.
        let r = (x >> 32) as usize;
        match r % 100 {
            0..70 => 16 + (r >> 8) % 113,
            70..95 => 128 + (r >> 8) % 897,
            _ => 1024 + (r >> 8) % 15361,
        }
    }
}

fn churn(iterations: usize, seed: u64) -> usize {
    const LIVE: usize = 4096;
    let mut sizes = Sizes(seed);
    let mut ring: Vec<Vec<u8>> = (0..LIVE).map(|_| Vec::new()).collect();
    let mut touched = 0usize;
    for i in 0..iterations {
        let mut v = Vec::with_capacity(sizes.next());
        v.push(i as u8);
        touched += v.capacity();
        ring[i % LIVE] = v;
    }
    black_box(&ring);
    touched
}

fn handoff(per_producer: usize) -> usize {
    use std::sync::mpsc::sync_channel;
    let mut threads = Vec::new();
    let mut freed = Vec::new();
    for p in 0..4u64 {
        let (send, receive) = sync_channel::<Vec<u8>>(1024);
        threads.push(std::thread::spawn(move || {
            let mut sizes = Sizes(0x9E37_79B9_7F4A_7C15 ^ (p + 1));
            for i in 0..per_producer {
                let mut v = Vec::with_capacity(sizes.next());
                v.push(i as u8);
                if send.send(v).is_err() {
                    break;
                }
            }
        }));
        freed.push(std::thread::spawn(move || {
            let mut bytes = 0usize;
            while let Ok(v) = receive.recv() {
                bytes += v.capacity();
                drop(v);
            }
            bytes
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    freed.into_iter().map(|t| t.join().unwrap()).sum()
}

fn assets() -> PathBuf {
    std::env::var_os("ENGINE_TEST_ASSETS").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets"),
        PathBuf::from,
    )
}

fn pack_path() -> PathBuf {
    std::env::temp_dir().join("alloc-compare-sponza.pack")
}

fn cook_once(jobs: &ThreadPool, gltf: &Path, bytes: &[u8], cache: &DerivedDataCache) {
    let scene = import_gltf(bytes, gltf.parent()).unwrap();
    let settings = CookSettings::default();
    let (cooked, _) = cook_scene(&scene, gltf.parent(), Some(cache), jobs, &settings).unwrap();
    write_pack(&pack_path(), &cooked, jobs, settings.compression_level).unwrap();
}

fn memory() -> (f64, f64) {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        // SAFETY: plain data.
        ..unsafe { core::mem::zeroed() }
    };
    // SAFETY: the pseudo-handle of this process and a counters structure of the given size.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let mib = |b: usize| b as f64 / (1 << 20) as f64;
    (
        mib(counters.PeakWorkingSetSize),
        mib(counters.PeakPagefileUsage),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let workload = args.next().unwrap_or_else(|| String::from("churn"));
    let reps: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(5);
    let jobs = ThreadPool::builder().build().unwrap();
    let gltf = assets().join("Sponza/Sponza.gltf");
    let cache = DerivedDataCache::new(assets().join(".ddc")).unwrap();
    let bytes = std::fs::read(&gltf).unwrap_or_default();
    if workload.starts_with("load") && !pack_path().exists() {
        cook_once(&jobs, &gltf, &bytes, &cache);
    }
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let mut times = Vec::new();
    for rep in 0..reps {
        let start = Instant::now();
        match workload.as_str() {
            "churn" => {
                black_box(churn(20_000_000, 0x2545_F491_4F6C_DD1D + rep as u64));
            }
            "parallel" => {
                std::thread::scope(|s| {
                    for t in 0..threads {
                        s.spawn(move || black_box(churn(5_000_000, 0x1234_5678 + t as u64)));
                    }
                });
            }
            "handoff" => {
                black_box(handoff(1_000_000));
            }
            "import" => {
                black_box(import_gltf(&bytes, gltf.parent()).unwrap());
            }
            "cook" => cook_once(&jobs, &gltf, &bytes, &cache),
            "load" => {
                black_box(load_pack(&pack_path(), &jobs, 8).unwrap());
            }
            // The load, then the scene dropped and the allocator asked to give memory
            // back: what a game holds after a level's loading temporaries are gone.
            "load-collect" => {
                drop(black_box(load_pack(&pack_path(), &jobs, 8).unwrap()));
                collect();
            }
            // As `load-collect`, every worker of the pool collecting too (the memory its
            // allocations held, freed by the main thread, is its heap's to give back).
            "load-collect-all" => {
                drop(black_box(load_pack(&pack_path(), &jobs, 8).unwrap()));
                collect();
                // 256 one-millisecond items over the pool: every worker takes some.
                jobs.parallel_for(256, 1, |_, range| {
                    for _ in range {
                        collect();
                        let start = Instant::now();
                        while start.elapsed().as_micros() < 1000 {
                            std::hint::spin_loop();
                        }
                    }
                });
            }
            // What a collect costs a thread: idle (nothing freed since the last one), and
            // after a burst of allocations it freed (a job's temporaries), plain and forced.
            "collect-cost" => {
                let costs: Vec<[f64; 4]> = std::thread::scope(|s| {
                    let handles: Vec<_> = (0..threads)
                        .map(|t| {
                            s.spawn(move || {
                                let ns = |f: &dyn Fn()| {
                                    let start = Instant::now();
                                    f();
                                    start.elapsed().as_nanos() as f64
                                };
                                collect_with(true);
                                let idle = ns(&|| collect_with(false));
                                black_box(churn(200_000, 7 + t as u64));
                                let after = ns(&|| collect_with(false));
                                black_box(churn(200_000, 9 + t as u64));
                                let forced = ns(&|| collect_with(true));
                                let idle_forced = ns(&|| collect_with(true));
                                [idle, after, forced, idle_forced]
                            })
                        })
                        .collect();
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                let median = |k: usize| {
                    let mut v: Vec<f64> = costs.iter().map(|c| c[k]).collect();
                    v.sort_by(f64::total_cmp);
                    v[v.len() / 2] / 1e3
                };
                eprintln!(
                    "collect us (median of {threads} threads): idle {:.1}, after a burst {:.1}, forced after a burst {:.1}, forced idle {:.1}",
                    median(0),
                    median(1),
                    median(2),
                    median(3)
                );
            }
            other => panic!("unknown workload {other}"),
        }
        times.push(start.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    let (working_set, commit) = memory();
    if workload.starts_with("load-collect") {
        let (now_set, now_private) = memory_now();
        eprintln!(
            "after the last collect: working set {now_set:.0} MiB, private {now_private:.0} MiB"
        );
    }
    println!(
        "{workload}\t{}\t{reps}\t{:.1}\t{:.1}\t{working_set:.0}\t{commit:.0}",
        allocator(),
        times[times.len() / 2],
        times[0],
    );
}
