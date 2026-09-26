//! "Compile All Shaders" (CLAUDE.md §9): every shader source, in parallel, through the
//! cache, with a report.

use crate::ShaderError;
use crate::cache::{CacheOutcome, ShaderCache};
use crate::compiler::{CompileOptions, CompileOutput};
use crate::pool::CompilerPool;
use engine_core::jobs::ThreadPool;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The result for one source file.
#[derive(Debug)]
pub struct UnitResult {
    /// The source file.
    pub path: PathBuf,
    /// Wall time for this file (cache lookup, plus compile on a miss).
    pub time: Duration,
    /// SPIR-V of every entry point and whether it came from the cache, or the error.
    /// Shared so that [`compile_all_streaming`] callbacks can pass results on while the
    /// report keeps them.
    pub result: Result<(Arc<CompileOutput>, CacheOutcome), Arc<ShaderError>>,
}

/// Outcome of [`compile_all`].
#[derive(Debug)]
pub struct BuildReport {
    /// One result per source, in the order given.
    pub units: Vec<UnitResult>,
    /// Wall time of the whole build.
    pub elapsed: Duration,
    /// How many files were compiled concurrently at most.
    pub parallelism: usize,
}

impl BuildReport {
    /// Compiled entry points (shader variants) across all successful files.
    pub fn entry_points(&self) -> usize {
        self.successes()
            .map(|(output, _)| output.entries.len())
            .sum()
    }

    /// Files served from the cache.
    pub fn cache_hits(&self) -> usize {
        self.successes()
            .filter(|(_, outcome)| *outcome == CacheOutcome::Hit)
            .count()
    }

    /// Files that were compiled.
    pub fn compiled(&self) -> usize {
        self.successes()
            .filter(|(_, outcome)| *outcome == CacheOutcome::Miss)
            .count()
    }

    /// Files that failed.
    pub fn failures(&self) -> impl Iterator<Item = (&PathBuf, &ShaderError)> {
        self.units
            .iter()
            .filter_map(|u| u.result.as_ref().err().map(|e| (&u.path, &**e)))
    }

    /// Whether every file compiled.
    pub fn is_success(&self) -> bool {
        self.failures().next().is_none()
    }

    fn successes(&self) -> impl Iterator<Item = (&CompileOutput, CacheOutcome)> {
        self.units
            .iter()
            .filter_map(|u| u.result.as_ref().ok().map(|(o, c)| (&**o, *c)))
    }
}

impl fmt::Display for BuildReport {
    /// A summary line, then every error with its diagnostics (file:line).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failed = self.failures().count();
        write!(
            f,
            "{} shader files, {} entry points in {:.2} s ({} compiled, {} from cache, {} failed; up to {} in parallel)",
            self.units.len(),
            self.entry_points(),
            self.elapsed.as_secs_f64(),
            self.compiled(),
            self.cache_hits(),
            failed,
            self.parallelism,
        )?;
        for (_, error) in self.failures() {
            write!(f, "\n\n{error}")?;
        }
        Ok(())
    }
}

/// Compiles every file in `sources` (all entry points of each, see
/// [`crate::find_shader_sources`]) with `options`, through `cache`.
///
/// Runs at most `max_parallel` compilations at once on `jobs`; each one needs its own
/// Slang session (~150 MB), which `compilers` creates on first use and keeps. Cache hits
/// never create a session. Files are claimed dynamically, so a slow file does not hold up
/// the others.
///
/// # Examples
///
/// ```no_run
/// use engine_core::jobs::ThreadPool;
/// use engine_shader::{CompileOptions, CompilerPool, ShaderCache, SlangLibrary};
///
/// let jobs = ThreadPool::builder().build().unwrap();
/// let compilers = CompilerPool::new(SlangLibrary::load().unwrap());
/// let cache = ShaderCache::new("target/shader-cache").unwrap();
/// let sources = engine_shader::find_shader_sources(&["shaders".into()]).unwrap();
/// let report = engine_shader::compile_all(
///     &jobs, &compilers, &cache, &sources, &CompileOptions::default(), 8,
/// );
/// println!("{report}");
/// ```
pub fn compile_all(
    jobs: &ThreadPool,
    compilers: &CompilerPool,
    cache: &ShaderCache,
    sources: &[PathBuf],
    options: &CompileOptions,
    max_parallel: usize,
) -> BuildReport {
    compile_all_streaming(
        jobs,
        compilers,
        cache,
        sources,
        options,
        max_parallel,
        |_, _| {},
    )
}

/// [`compile_all`] that also calls `on_done(index into sources, result)` on the worker
/// thread as soon as each file is done, so results can be used before the slowest file
/// finishes (hot reload of a module imported by many shaders).
pub fn compile_all_streaming(
    jobs: &ThreadPool,
    compilers: &CompilerPool,
    cache: &ShaderCache,
    sources: &[PathBuf],
    options: &CompileOptions,
    max_parallel: usize,
    on_done: impl Fn(usize, &UnitResult) + Sync,
) -> BuildReport {
    engine_profiler::zone!("shader::compile_all");
    let start = Instant::now();
    let parallelism = max_parallel
        .min(sources.len())
        .min(jobs.num_workers() + 1)
        .max(1);
    let next = AtomicUsize::new(0);
    let mut buckets: Vec<Vec<(usize, UnitResult)>> = (0..parallelism).map(|_| Vec::new()).collect();
    {
        let (next, on_done) = (&next, &on_done);
        jobs.scope(|scope| {
            for bucket in &mut buckets {
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = sources.get(index) else {
                            break;
                        };
                        let unit_start = Instant::now();
                        let result = cache
                            .compile(compilers, path, &[], options)
                            .map(|(output, outcome)| (Arc::new(output), outcome))
                            .map_err(Arc::new);
                        let unit = UnitResult {
                            path: path.clone(),
                            time: unit_start.elapsed(),
                            result,
                        };
                        on_done(index, &unit);
                        bucket.push((index, unit));
                    }
                });
            }
        });
    }
    let mut units: Vec<(usize, UnitResult)> = buckets.into_iter().flatten().collect();
    units.sort_by_key(|(index, _)| *index);
    BuildReport {
        units: units.into_iter().map(|(_, unit)| unit).collect(),
        elapsed: start.elapsed(),
        parallelism,
    }
}
