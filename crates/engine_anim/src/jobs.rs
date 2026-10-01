//! Animating many characters on the job system.

use engine_core::jobs::ThreadPool;

/// Runs `update` on every item of `items` in parallel on `pool`, `grain` items per job
/// (characters are independent: each owns its poses and outputs). Allocates nothing: the
/// jobs live in the pool's per-thread arenas.
///
/// ```
/// use engine_core::jobs::ThreadPool;
/// let pool = ThreadPool::builder().workers(2).build().unwrap();
/// let mut times = vec![0.0f32; 100];
/// engine_anim::for_each_parallel(&pool, &mut times, 16, |t| *t += 1.0 / 60.0);
/// assert!(times.iter().all(|&t| t > 0.0));
/// ```
pub fn for_each_parallel<T: Send>(
    pool: &ThreadPool,
    items: &mut [T],
    grain: usize,
    update: impl Fn(&mut T) + Sync,
) {
    engine_profiler::zone!("anim::for_each_parallel");
    let grain = grain.max(1);
    if items.len() <= grain {
        items.iter_mut().for_each(&update);
        return;
    }
    let update = &update;
    pool.scope(|s| {
        for chunk in items.chunks_mut(grain) {
            s.spawn(move || chunk.iter_mut().for_each(update));
        }
    });
}
