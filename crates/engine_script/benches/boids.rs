//! Boids: the same simulation step in Rust and in Luau (interpreted, native, per-module
//! environment, and with positions in a `buffer`). 1,000 agents, every pair checked
//! (1M distance tests per step): numeric-heavy, the kind of loop that decides where
//! code should live (ADR 0032). Median of repeated steps, single thread. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_script --bench boids
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_script::mlua::{self, Function, Lua};
use std::time::Instant;

const N: usize = 1_000;
const STEPS: usize = 30;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn initial() -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut r = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 10_000) as f32 / 100.0
    };
    let pos = (0..N).map(|_| [r(), r(), r()]).collect();
    let vel = (0..N)
        .map(|_| [r() / 100.0 - 0.5, r() / 100.0 - 0.5, r() / 100.0 - 0.5])
        .collect();
    (pos, vel)
}

/// Cohesion and separation within radius 5, then integrate.
fn rust_step(pos: &mut [[f32; 3]], vel: &mut [[f32; 3]]) {
    let n = pos.len();
    for i in 0..n {
        let p = pos[i];
        let mut center = [0.0f32; 3];
        let mut push = [0.0f32; 3];
        let mut count = 0.0f32;
        for (j, q) in pos.iter().enumerate() {
            if i == j {
                continue;
            }
            let d = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
            let d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            if d2 < 25.0 {
                for k in 0..3 {
                    center[k] += q[k];
                    push[k] -= d[k] / (d2 + 0.01);
                }
                count += 1.0;
            }
        }
        if count > 0.0 {
            for k in 0..3 {
                vel[i][k] += (center[k] / count - p[k]) * 0.01 + push[k] * 0.05;
            }
        }
    }
    for i in 0..n {
        for k in 0..3 {
            vel[i][k] *= 0.99;
            pos[i][k] += vel[i][k] * 0.1;
        }
    }
}

const LUAU_VECTORS: &str = r#"
local M = {}
function M.step(pos: {vector}, vel: {vector}, n: number)
    for i = 1, n do
        local p = pos[i]
        local center = vector.zero
        local push = vector.zero
        local count = 0
        for j = 1, n do
            if i ~= j then
                local d = pos[j] - p
                local d2 = vector.dot(d, d)
                if d2 < 25 then
                    center += pos[j]
                    push -= d / (d2 + 0.01)
                    count += 1
                end
            end
        end
        if count > 0 then
            vel[i] += (center / count - p) * 0.01 + push * 0.05
        end
    end
    for i = 1, n do
        vel[i] *= 0.99
        pos[i] += vel[i] * 0.1
    end
end
return M
"#;

const LUAU_BUFFER: &str = r#"
local M = {}
-- Positions and velocities as packed f32 triples in buffers (12 bytes per agent).
function M.step(pos: buffer, vel: buffer, n: number)
    for i = 0, n - 1 do
        local o = i * 12
        local px, py, pz = buffer.readf32(pos, o), buffer.readf32(pos, o + 4), buffer.readf32(pos, o + 8)
        local cx, cy, cz, sx, sy, sz, count = 0, 0, 0, 0, 0, 0, 0
        for j = 0, n - 1 do
            if i ~= j then
                local q = j * 12
                local qx, qy, qz = buffer.readf32(pos, q), buffer.readf32(pos, q + 4), buffer.readf32(pos, q + 8)
                local dx, dy, dz = qx - px, qy - py, qz - pz
                local d2 = dx * dx + dy * dy + dz * dz
                if d2 < 25 then
                    cx += qx; cy += qy; cz += qz
                    local s = 1 / (d2 + 0.01)
                    sx -= dx * s; sy -= dy * s; sz -= dz * s
                    count += 1
                end
            end
        end
        if count > 0 then
            buffer.writef32(vel, o, buffer.readf32(vel, o) + (cx / count - px) * 0.01 + sx * 0.05)
            buffer.writef32(vel, o + 4, buffer.readf32(vel, o + 4) + (cy / count - py) * 0.01 + sy * 0.05)
            buffer.writef32(vel, o + 8, buffer.readf32(vel, o + 8) + (cz / count - pz) * 0.01 + sz * 0.05)
        end
    end
    for o = 0, n * 12 - 4, 4 do
        local v = buffer.readf32(vel, o) * 0.99
        buffer.writef32(vel, o, v)
        buffer.writef32(pos, o, buffer.readf32(pos, o) + v * 0.1)
    end
end
return M
"#;

/// Loads `src` and returns its `step`. `env`: give the chunk its own environment (as
/// `ScriptVm` does per module) to measure what that costs.
fn luau(native: bool, env: bool, env_safe: bool, src: &str) -> (Lua, Function) {
    let lua = Lua::new();
    lua.set_compiler(
        mlua::chunk::Compiler::new()
            .set_optimization_level(2)
            .set_type_info_level(1),
    );
    lua.enable_jit(native);
    lua.sandbox(true).unwrap();
    let mut chunk = lua.load(src);
    if env {
        let e = lua.create_table().unwrap();
        let m = lua.create_table().unwrap();
        m.set("__index", lua.globals()).unwrap();
        e.set_metatable(Some(m)).unwrap();
        if env_safe {
            // SAFETY: flag on the pushed table, as `ScriptVm` does.
            unsafe {
                lua.exec_raw::<()>(e.clone(), |state| mlua::ffi::lua_setsafeenv(state, -1, 1))
                    .unwrap();
            }
        }
        chunk = chunk.set_environment(e);
    }
    let module: mlua::Table = chunk.eval().unwrap();
    let step: Function = module.get("step").unwrap();
    (lua, step)
}

fn time_luau_vectors(native: bool, env: bool, env_safe: bool) -> f64 {
    let (lua, step) = luau(native, env, env_safe, LUAU_VECTORS);
    let (p, v) = initial();
    let to_table = |xs: &[[f32; 3]]| {
        let t = lua.create_table().unwrap();
        for (i, x) in xs.iter().enumerate() {
            t.set(i + 1, mlua::Vector::new(x[0], x[1], x[2])).unwrap();
        }
        t
    };
    let (pos, vel) = (to_table(&p), to_table(&v));
    median(
        (0..STEPS)
            .map(|_| {
                let t = Instant::now();
                step.call::<()>((pos.clone(), vel.clone(), N)).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    )
}

fn time_luau_buffer(native: bool) -> f64 {
    let (lua, step) = luau(native, false, false, LUAU_BUFFER);
    let (p, v) = initial();
    let to_buffer = |xs: &[[f32; 3]]| {
        let bytes: Vec<u8> = xs.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        lua.create_buffer(bytes).unwrap()
    };
    let (pos, vel) = (to_buffer(&p), to_buffer(&v));
    median(
        (0..STEPS)
            .map(|_| {
                let t = Instant::now();
                step.call::<()>((pos.clone(), vel.clone(), N)).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    )
}

fn main() {
    let (mut pos, mut vel) = initial();
    let rust = median(
        (0..STEPS)
            .map(|_| {
                let t = Instant::now();
                rust_step(&mut pos, &mut vel);
                std::hint::black_box(&pos);
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect(),
    );
    let rows = [
        ("Rust (f32 arrays)", rust),
        (
            "Luau native, vectors in tables",
            time_luau_vectors(true, false, false),
        ),
        (
            "Luau native, vectors, per-module environment",
            time_luau_vectors(true, true, false),
        ),
        (
            "Luau native, vectors, per-module environment marked safe (ScriptVm)",
            time_luau_vectors(true, true, true),
        ),
        (
            "Luau interpreted, vectors in tables",
            time_luau_vectors(false, false, false),
        ),
        ("Luau native, f32 in buffers", time_luau_buffer(true)),
        ("Luau interpreted, f32 in buffers", time_luau_buffer(false)),
    ];
    println!("### Boids step, {N} agents, all pairs (ms, median of {STEPS}; lower is better)\n");
    println!("| variant | ms | × Rust |");
    println!("|---|---:|---:|");
    for (name, ms) in rows {
        println!("| {name} | {ms:.2} | {:.1} |", ms / rust);
    }
    println!();
}
