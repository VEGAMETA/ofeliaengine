//! glam (SSE2) against a plain scalar implementation of the same operations: what writing
//! our own math types would start from (ADR 0012). Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_core --bench math
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::needless_range_loop)]

use engine_core::math::{Mat4, Quat, Vec3, Vec4};
use std::hint::black_box;
use std::time::Instant;

/// Column-major 4×4 matrix without SIMD.
#[derive(Clone, Copy)]
struct ScalarMat4([[f32; 4]; 4]);

impl ScalarMat4 {
    fn mul_vec4(&self, v: [f32; 4]) -> [f32; 4] {
        let m = &self.0;
        let mut out = [0.0; 4];
        for row in 0..4 {
            out[row] = m[0][row] * v[0] + m[1][row] * v[1] + m[2][row] * v[2] + m[3][row] * v[3];
        }
        out
    }

    fn mul(&self, other: &Self) -> Self {
        let mut out = [[0.0; 4]; 4];
        for col in 0..4 {
            out[col] = self.mul_vec4(other.0[col]);
        }
        Self(out)
    }
}

fn scalar_quat_rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    // v + 2 w (q × v) + 2 q × (q × v)
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    let t = [
        2.0 * (y * v[2] - z * v[1]),
        2.0 * (z * v[0] - x * v[2]),
        2.0 * (x * v[1] - y * v[0]),
    ];
    [
        v[0] + w * t[0] + (y * t[2] - z * t[1]),
        v[1] + w * t[1] + (z * t[0] - x * t[2]),
        v[2] + w * t[2] + (x * t[1] - y * t[0]),
    ]
}

fn best_of<F: FnMut() -> f32>(runs: usize, mut f: F) -> f64 {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            black_box(f());
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

fn main() {
    const N: usize = 1 << 20;
    let points: Vec<Vec4> = (0..N)
        .map(|i| Vec4::new(i as f32, (i * 3) as f32, (i * 7) as f32, 1.0))
        .collect();
    let scalar_points: Vec<[f32; 4]> = points.iter().map(|p| p.to_array()).collect();
    let m = Mat4::from_scale_rotation_translation(
        Vec3::new(1.5, 2.0, 0.5),
        Quat::from_rotation_y(0.7) * Quat::from_rotation_x(0.3),
        Vec3::new(10.0, -3.0, 2.0),
    );
    let sm = ScalarMat4(m.to_cols_array_2d());
    let q = Quat::from_rotation_z(1.1).normalize();
    let sq = q.to_array();

    let glam_transform = best_of(5, || {
        let mut acc = 0.0;
        for p in &points {
            acc += (m * *p).x;
        }
        acc
    });
    let scalar_transform = best_of(5, || {
        let mut acc = 0.0;
        for p in &scalar_points {
            acc += sm.mul_vec4(*p)[0];
        }
        acc
    });
    let glam_matmul = best_of(5, || {
        let mut acc = Mat4::IDENTITY;
        for _ in 0..N / 16 {
            acc = black_box(m) * acc;
        }
        acc.x_axis.x
    });
    let scalar_matmul = best_of(5, || {
        let mut acc = ScalarMat4(Mat4::IDENTITY.to_cols_array_2d());
        for _ in 0..N / 16 {
            acc = black_box(sm).mul(&acc);
        }
        acc.0[0][0]
    });
    let glam_rotate = best_of(5, || {
        let mut acc = 0.0;
        for p in &points {
            acc += (q * p.truncate()).y;
        }
        acc
    });
    let scalar_rotate = best_of(5, || {
        let mut acc = 0.0;
        for p in &scalar_points {
            acc += scalar_quat_rotate(sq, [p[0], p[1], p[2]])[1];
        }
        acc
    });

    let ns = |seconds: f64, count: usize| seconds * 1e9 / count as f64;
    println!("### Math: glam (SSE2) vs scalar\n");
    println!("Best of 5 runs, release, `x86-64-v2` target.\n");
    println!("| Operation | glam ns | scalar ns | scalar / glam |\n|---|---|---|---|");
    for (name, g, s, count) in [
        (
            "Mat4 × Vec4 (1M points)",
            glam_transform,
            scalar_transform,
            N,
        ),
        (
            "Mat4 × Mat4 (dependent chain)",
            glam_matmul,
            scalar_matmul,
            N / 16,
        ),
        (
            "Quat rotate Vec3 (1M points)",
            glam_rotate,
            scalar_rotate,
            N,
        ),
    ] {
        println!(
            "| {name} | {:.2} | {:.2} | {:.2}x |",
            ns(g, count),
            ns(s, count),
            s / g
        );
    }
}
