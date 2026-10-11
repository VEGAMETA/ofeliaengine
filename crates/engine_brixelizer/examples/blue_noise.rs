//! Generates the blue noise Brixelizer GI samples (ADR 0090; the SDK's sample loads
//! Christoph Peters' textures from files): two 128 × 128 tiles of ranks made by
//! void-and-cluster (Ulichney, "The void-and-cluster method for dither array generation",
//! 1993) with a Gaussian of σ = 1.5 on the torus, one per channel, written as
//! `data/blue_noise_128.bin`: 128 × 128 texels of two bytes (R, G), each rank × 256 / 16,384.
//!
//! `cargo run -p engine_brixelizer --release --example blue_noise`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use std::path::Path;

const N: usize = 128;
const SIGMA: f32 = 1.5;
/// The Gaussian is below 1e-6 of its peak beyond 8 texels.
const RADIUS: i32 = 8;

struct Field {
    energy: Vec<f32>,
    on: Vec<bool>,
    kernel: Vec<(i32, i32, f32)>,
}

impl Field {
    fn new() -> Self {
        let mut kernel = Vec::new();
        for dy in -RADIUS..=RADIUS {
            for dx in -RADIUS..=RADIUS {
                let d2 = (dx * dx + dy * dy) as f32;
                kernel.push((dx, dy, (-d2 / (2.0 * SIGMA * SIGMA)).exp()));
            }
        }
        Self {
            energy: vec![0.0; N * N],
            on: vec![false; N * N],
            kernel,
        }
    }

    fn set(&mut self, i: usize, on: bool) {
        if self.on[i] == on {
            return;
        }
        self.on[i] = on;
        let sign = if on { 1.0 } else { -1.0 };
        let (x, y) = ((i % N) as i32, (i / N) as i32);
        for &(dx, dy, w) in &self.kernel {
            let px = (x + dx).rem_euclid(N as i32) as usize;
            let py = (y + dy).rem_euclid(N as i32) as usize;
            self.energy[py * N + px] += sign * w;
        }
    }

    /// The set texel with the most energy around it.
    fn tightest_cluster(&self) -> usize {
        (0..N * N)
            .filter(|&i| self.on[i])
            .max_by(|&a, &b| self.energy[a].total_cmp(&self.energy[b]))
            .unwrap()
    }

    /// The unset texel with the least energy around it.
    fn largest_void(&self) -> usize {
        (0..N * N)
            .filter(|&i| !self.on[i])
            .min_by(|&a, &b| self.energy[a].total_cmp(&self.energy[b]))
            .unwrap()
    }
}

/// Ranks 0 .. N² of every texel.
fn void_and_cluster(seed: u64) -> Vec<u32> {
    let size = N * N;
    let mut field = Field::new();
    // A tenth of the texels at random (xorshift), then relaxed: the tightest cluster moves
    // to the largest void until it is the largest void itself.
    let mut state = seed | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let initial = size / 10;
    let mut placed = 0;
    while placed < initial {
        let i = (next() % size as u64) as usize;
        if !field.on[i] {
            field.set(i, true);
            placed += 1;
        }
    }
    loop {
        let cluster = field.tightest_cluster();
        field.set(cluster, false);
        let void = field.largest_void();
        if void == cluster {
            field.set(cluster, true);
            break;
        }
        field.set(void, true);
    }
    let pattern = field.on.clone();
    let mut rank = vec![0u32; size];
    // Phase 1: the initial points, ranked by removing the tightest cluster each time.
    for r in (0..initial).rev() {
        let i = field.tightest_cluster();
        rank[i] = r as u32;
        field.set(i, false);
    }
    // Phases 2 and 3: from the initial pattern, filling the largest void each time.
    let mut field2 = Field::new();
    for (i, &on) in pattern.iter().enumerate() {
        if on {
            field2.set(i, true);
        }
    }
    for r in initial..size {
        let i = field2.largest_void();
        rank[i] = r as u32;
        field2.set(i, true);
    }
    rank
}

fn main() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tiles = [
        void_and_cluster(0x9e37_79b9_7f4a_7c15),
        void_and_cluster(0xc2b2_ae3d_27d4_eb4f),
    ];
    let mut bytes = Vec::with_capacity(N * N * 2);
    for i in 0..N * N {
        for tile in &tiles {
            bytes.push((tile[i] as usize * 256 / (N * N)) as u8);
        }
    }
    std::fs::create_dir_all(crate_dir.join("data")).unwrap();
    std::fs::write(crate_dir.join("data/blue_noise_128.bin"), &bytes).unwrap();
    println!("wrote {} bytes", bytes.len());
}
