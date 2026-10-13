//! Hot reload of a material type (ADR 0092): its module edited while frames are drawn. The
//! new version is on screen without a restart, the one before drawing until it is made (and
//! destroyed under the validation layer only once the GPU is done with it); a module that
//! does not compile keeps the previous version and says why. And a material file edited:
//! its parameters replaced in place (`engine_streaming::material_files`).

use crate::common::{Bed, camera};
use engine_core::math::Vec3;
use engine_frame::{Antialiasing, View, ViewSettings};
use engine_render::forward::{ENGINE_SHADER_DIR, MaterialTypeDesc};
use engine_render::material::{MaterialDesc, MaterialParams};
use engine_streaming::material_files::LiveMaterialFiles;
use std::path::Path;
use std::time::{Duration, Instant};

/// An unlit type of one color.
fn glow(rgb: [f32; 3]) -> String {
    format!(
        r#"
struct GlowParams {{ float4 unused; }};
struct GlowMaterial : IMaterial
{{
    typealias Params = GlowParams;
    static uint model() {{ return MODEL_UNLIT; }}
    static GlowParams load(MaterialContext ctx) {{ return material_params<GlowParams>(ctx); }}
    static Surface surface(MaterialContext ctx, GlowParams p, bool masked)
    {{
        Surface s;
        s.base = float4({}, {}, {}, 1.0);
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = 0.0;
        s.roughness = 1.0;
        s.subsurface = 0;
        return surface_terms(s);
    }}
    static float occlusion(MaterialContext ctx, GlowParams p) {{ return 1.0; }}
    static float3 emission(MaterialContext ctx, GlowParams p) {{ return 0.0; }}
    static float alpha(MaterialContext ctx, GlowParams p) {{ return 1.0; }}
    static float alpha_cutoff(GlowParams p) {{ return 0.0; }}
    static float thickness(MaterialContext ctx, GlowParams p) {{ return 0.0; }}
}}
"#,
        rgb[0], rgb[1], rgb[2]
    )
}

/// Writes `text` the way editors save: to a temporary file, then renamed over the module.
fn save(module: &Path, text: &str) {
    let temporary = module.with_extension("slang.tmp");
    std::fs::write(&temporary, text).unwrap();
    std::fs::rename(&temporary, module).unwrap();
}

/// The view's center pixel's strongest channel: 0 red, 1 green, 2 blue.
fn strongest(rgba: &[u8], size: (u32, u32)) -> usize {
    let at = ((size.1 / 2 * size.0 + size.0 / 2) * 4) as usize;
    let pixel = &rgba[at..at + 3];
    (0..3).max_by_key(|&c| pixel[c]).unwrap_or(0)
}

#[test]
fn a_material_type_reloads_when_its_module_changes() {
    let Some(mut bed) = Bed::new((64, 64), true) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("engine_frame_hot_reload_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("glow.slang");
    save(&module, &glow([0.9, 0.05, 0.05]));
    let forward = bed.frame.forward_mut();
    forward.set_material_hot_reload(true);
    let ty = forward.register_material_type(MaterialTypeDesc {
        name: String::from("glow"),
        module: module.clone(),
        type_name: String::from("GlowMaterial"),
    });
    let material = bed.materials.add_typed(
        ty,
        MaterialDesc::solid([0.5; 3], 0.0, 0.5),
        Some(MaterialParams {
            bytes: vec![0; 16],
            textures: Vec::new(),
        }),
    );
    // A wall filling the view.
    let draws = vec![bed.cube_at(
        material,
        Vec3::new(30.0, 30.0, 0.2),
        Vec3::new(0.0, 0.0, -3.0),
    )];
    let eye = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::new(0.0, 0.0, -3.0), false);
    let settings = ViewSettings {
        antialiasing: Antialiasing::OFF,
        bloom: false,
        ..ViewSettings::default()
    };
    let mut view = View::new(&bed.device, 16);
    let size = bed.size;
    let mut frame = |bed: &mut Bed, view: &mut View| {
        let (_, rgba) = bed.render(view, &settings, &draws, &eye, true, |_| {});
        strongest(&rgba.unwrap(), size)
    };
    // The first frame asks for the type's pipeline; once made, it draws red.
    frame(&mut bed, &mut view);
    assert!(
        bed.frame
            .forward_mut()
            .wait_for_material_types(&bed.device, Duration::from_secs(60))
    );
    // The type compiles at run time: without Slang its fallback draws.
    if bed
        .frame
        .forward_mut()
        .material_type_error(ty)
        .is_some_and(|e| e.contains("no Slang compiler"))
        && !std::env::var("ENGINE_REQUIRE_SLANG").is_ok_and(|v| v == "1")
    {
        println!("skipping: no Slang library");
        bed.finish(&mut [&mut view]);
        return;
    }
    assert_eq!(frame(&mut bed, &mut view), 0, "red before the edit");

    // Frames until the center turns `channel`, at most 20 s: how long the edit took to
    // reach the screen.
    let until = |bed: &mut Bed,
                 view: &mut View,
                 channel: usize,
                 frame: &mut dyn FnMut(&mut Bed, &mut View) -> usize| {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if frame(bed, view) == channel {
                return Some(start.elapsed());
            }
        }
        None
    };
    save(&module, &glow([0.05, 0.9, 0.05]));
    let took = until(&mut bed, &mut view, 1, &mut frame).expect("green after the edit");
    println!("edit to screen: {:.0} ms", took.as_secs_f64() * 1e3);
    assert!(bed.frame.forward_mut().material_type_error(ty).is_none());

    // A module that does not compile: the error is kept, green stays on screen.
    save(
        &module,
        "struct GlowMaterial : IMaterial { this does not compile",
    );
    let start = Instant::now();
    while bed.frame.forward_mut().material_type_error(ty).is_none()
        && start.elapsed() < Duration::from_secs(20)
    {
        assert_eq!(frame(&mut bed, &mut view), 1, "the previous version draws");
    }
    let error = bed
        .frame
        .forward_mut()
        .material_type_error(ty)
        .map(str::to_owned)
        .expect("the error is reported");
    println!(
        "the broken module's error: {}",
        error.lines().next().unwrap_or("")
    );
    assert!(
        error.contains("glow.slang"),
        "the error names the module: {error}"
    );
    assert_eq!(frame(&mut bed, &mut view), 1);

    // Fixed: blue, and the error is gone.
    save(&module, &glow([0.05, 0.05, 0.9]));
    let took = until(&mut bed, &mut view, 2, &mut frame).expect("blue after the fix");
    println!("fix to screen: {:.0} ms", took.as_secs_f64() * 1e3);
    assert!(bed.frame.forward_mut().material_type_error(ty).is_none());

    bed.finish(&mut [&mut view]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unlit type of the color its parameters give.
const TINT: &str = r#"
struct TintParams { float4 color; };
struct TintMaterial : IMaterial
{
    typealias Params = TintParams;
    static uint model() { return MODEL_UNLIT; }
    static TintParams load(MaterialContext ctx) { return material_params<TintParams>(ctx); }
    static Surface surface(MaterialContext ctx, TintParams p, bool masked)
    {
        Surface s;
        s.base = p.color;
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = 0.0;
        s.roughness = 1.0;
        s.subsurface = 0;
        return surface_terms(s);
    }
    static float occlusion(MaterialContext ctx, TintParams p) { return 1.0; }
    static float3 emission(MaterialContext ctx, TintParams p) { return 0.0; }
    static float alpha(MaterialContext ctx, TintParams p) { return 1.0; }
    static float alpha_cutoff(TintParams p) { return 0.0; }
    static float thickness(MaterialContext ctx, TintParams p) { return 0.0; }
}
"#;

fn tint_file(rgb: [f32; 3]) -> String {
    format!(
        "(shader: \"tint.slang\", type: \"TintMaterial\", params: {{ \"color\": ({}, {}, {}, 1.0) }})",
        rgb[0], rgb[1], rgb[2]
    )
}

#[test]
fn a_material_file_reloads_when_it_changes() {
    let Some(mut bed) = Bed::new((64, 64), true) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("engine_frame_live_files_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tint.slang"), TINT).unwrap();
    let path = dir.join("wall.material.ron");
    std::fs::write(&path, tint_file([0.9, 0.05, 0.05])).unwrap();
    let mut live = LiveMaterialFiles::new(Path::new(ENGINE_SHADER_DIR));
    live.set_hot_reload(true);
    let mut no_textures = |_: &Path, _| None;
    let material = match live.load(
        &path,
        MaterialDesc::solid([0.5; 3], 0.0, 0.5),
        bed.frame.forward_mut(),
        &mut bed.materials,
        &mut no_textures,
    ) {
        Ok(m) => m,
        Err(e)
            if e.contains("Slang")
                && !std::env::var("ENGINE_REQUIRE_SLANG").is_ok_and(|v| v == "1") =>
        {
            println!("skipping: {e}");
            bed.finish(&mut []);
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let draws = vec![bed.cube_at(
        material,
        Vec3::new(30.0, 30.0, 0.2),
        Vec3::new(0.0, 0.0, -3.0),
    )];
    let eye = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::new(0.0, 0.0, -3.0), false);
    let settings = ViewSettings {
        antialiasing: Antialiasing::OFF,
        bloom: false,
        ..ViewSettings::default()
    };
    let mut view = View::new(&bed.device, 16);
    let size = bed.size;
    // A frame: the files read again first (once per frame), then drawn.
    let frame = |bed: &mut Bed, view: &mut View, live: &mut LiveMaterialFiles| {
        let read = live.update(bed.frame.forward_mut(), &mut bed.materials, &mut |_, _| {
            None
        });
        let (_, rgba) = bed.render(view, &settings, &draws, &eye, true, |_| {});
        (strongest(&rgba.unwrap(), size), read)
    };
    frame(&mut bed, &mut view, &mut live);
    assert!(
        bed.frame
            .forward_mut()
            .wait_for_material_types(&bed.device, Duration::from_secs(60))
    );
    assert_eq!(
        frame(&mut bed, &mut view, &mut live).0,
        0,
        "red before the edit"
    );

    // Edited (as editors save): green within a few frames, no compile.
    save_as(&path, &tint_file([0.05, 0.9, 0.05]));
    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(20) {
        if frame(&mut bed, &mut view, &mut live).0 == 1 {
            seen = true;
            break;
        }
    }
    assert!(seen, "green after the edit");
    println!(
        "file edit to screen: {:.0} ms",
        start.elapsed().as_secs_f64() * 1e3
    );

    // A file that does not parse: its error is kept, green stays.
    save_as(&path, "(shader: \"tint.slang\", type: ");
    let start = Instant::now();
    while live.error(&path).is_none() && start.elapsed() < Duration::from_secs(20) {
        assert_eq!(
            frame(&mut bed, &mut view, &mut live).0,
            1,
            "the previous parameters draw"
        );
    }
    let error = live
        .error(&path)
        .map(str::to_owned)
        .expect("the error is kept");
    println!("the broken file's error: {error}");
    assert!(error.contains("wall.material.ron"), "{error}");

    // Fixed: blue.
    save_as(&path, &tint_file([0.05, 0.05, 0.9]));
    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(20) {
        if frame(&mut bed, &mut view, &mut live).0 == 2 {
            seen = true;
            break;
        }
    }
    assert!(seen, "blue after the fix");
    assert!(live.error(&path).is_none());
    bed.finish(&mut [&mut view]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writes `text` to `path` through a temporary file renamed over it.
fn save_as(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).unwrap();
    std::fs::rename(&temporary, path).unwrap();
}
