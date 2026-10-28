//! Hot reload of a material type (ADR 0092): its module edited while frames are drawn. The
//! new version is on screen without a restart, the one before drawing until it is made (and
//! destroyed under the validation layer only once the GPU is done with it); a module that
//! does not compile keeps the previous version and says why; a parameter added is in the
//! reflected layout, its revision moved (the editor's inspector follows it, E10-2c); a type
//! whose module never compiled, so never drawn, reflected again once fixed (E10-3g-1). And
//! a material file edited: its parameters replaced in place
//! (`engine_streaming::material_files`).

use crate::common::{Bed, camera};
use engine_core::math::Vec3;
use engine_frame::{Antialiasing, View, ViewSettings};
use engine_render::forward::{ENGINE_SHADER_DIR, MaterialTypeDesc};
use engine_render::material::{MaterialDesc, MaterialParams};
use engine_streaming::material_files::LiveMaterialFiles;
use std::path::Path;
use std::time::{Duration, Instant};

/// An unlit type of one color, its parameters `unused` and then `more`'s fields.
fn glow(rgb: [f32; 3], more: &str) -> String {
    format!(
        r#"
struct GlowParams {{ float4 unused;{more} }};
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
    save(&module, &glow([0.9, 0.05, 0.05], ""));
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

    // The type's parameters' layout, reflected off the frame once asked for: its fields'
    // names and its revision, when it is not `after` (frames for at most 20 s).
    let reflected = |bed: &mut Bed,
                     view: &mut View,
                     after: Option<u32>,
                     frame: &mut dyn FnMut(&mut Bed, &mut View) -> usize| {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if let Some((Ok(layout), revision)) = bed.frame.forward().material_params(ty)
                && Some(revision) != after
            {
                let names: Vec<String> = layout.fields.iter().map(|f| f.name.clone()).collect();
                return Some((names, revision));
            }
            frame(bed, view);
        }
        None
    };
    let (names, revision) =
        reflected(&mut bed, &mut view, None, &mut frame).expect("the parameters' layout");
    assert_eq!(names, ["unused"]);

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
    // The edit adds a parameter too.
    save(&module, &glow([0.05, 0.9, 0.05], " float extra;"));
    let took = until(&mut bed, &mut view, 1, &mut frame).expect("green after the edit");
    println!("edit to screen: {:.0} ms", took.as_secs_f64() * 1e3);
    assert!(bed.frame.forward_mut().material_type_error(ty).is_none());
    // Reflected again with it: the layout has the new field, its revision moved.
    let (names, _) = reflected(&mut bed, &mut view, Some(revision), &mut frame)
        .expect("the layout reflected again");
    assert_eq!(names, ["unused", "extra"]);

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
    save(&module, &glow([0.05, 0.05, 0.9], " float extra;"));
    let took = until(&mut bed, &mut view, 2, &mut frame).expect("blue after the fix");
    println!("fix to screen: {:.0} ms", took.as_secs_f64() * 1e3);
    assert!(bed.frame.forward_mut().material_type_error(ty).is_none());

    bed.finish(&mut [&mut view]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// E10-3g-1: a type whose module does not compile from the start (its parameters'
/// reflection failed, so no material of it is made and it is never drawn) is reflected
/// again when its module is fixed, its revision moved: the editor's graph preview, an
/// Expression node's code typed wrong, then right.
#[test]
fn a_type_broken_from_the_start_is_reflected_again_when_fixed() {
    let Some(mut bed) = Bed::new((64, 64), true) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("engine_frame_broken_type_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("glow.slang");
    save(
        &module,
        "struct GlowMaterial : IMaterial { this does not compile",
    );
    let forward = bed.frame.forward_mut();
    forward.set_material_hot_reload(true);
    let ty = forward.register_material_type(MaterialTypeDesc {
        name: String::from("glow"),
        module: module.clone(),
        type_name: String::from("GlowMaterial"),
    });
    let settings = ViewSettings {
        antialiasing: Antialiasing::OFF,
        bloom: false,
        ..ViewSettings::default()
    };
    let eye = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::new(0.0, 0.0, -3.0), false);
    let mut view = View::new(&bed.device, 16);
    // Frames that draw nothing of the type until its reflection is `ok` (or not) at a
    // revision other than `after`, at most 20 s: why, if it failed, and the revision.
    let reflected = |bed: &mut Bed, view: &mut View, ok: bool, after: Option<u32>| {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if let Some((result, revision)) = bed.frame.forward().material_params(ty)
                && result.is_ok() == ok
                && Some(revision) != after
            {
                return Some((result.err().map(str::to_owned), revision));
            }
            let _ = bed.render(view, &settings, &[], &eye, false, |_| {});
        }
        None
    };
    let (why, revision) =
        reflected(&mut bed, &mut view, false, None).expect("the broken module's reflection");
    let why = why.unwrap_or_default();
    if why.contains("no Slang compiler")
        && !std::env::var("ENGINE_REQUIRE_SLANG").is_ok_and(|v| v == "1")
    {
        println!("skipping: no Slang library");
        bed.finish(&mut [&mut view]);
        return;
    }
    assert!(
        why.contains("glow.slang"),
        "the error names the module: {why}"
    );
    // Fixed, the type not drawn: its layout reflected, the revision moved.
    let start = Instant::now();
    save(&module, &glow([0.05, 0.9, 0.05], ""));
    let (_, fixed) = reflected(&mut bed, &mut view, true, Some(revision))
        .expect("the fixed module reflected again");
    println!(
        "fixed: reflected again after {:.0} ms",
        start.elapsed().as_secs_f64() * 1e3
    );
    assert_ne!(fixed, revision);
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

/// A `.material` file of the tint type (ADR 0120), its module beside it in the project.
fn tint_file(rgb: [f32; 3]) -> String {
    format!(
        "[material version=2 id=4f8e1d2c3b4a59687706f5e4d3c2b1a0]
shader = asset(\"tint.slang\", 7d1c0e9a2b3f4c5d8e6f7a8b9c0d1e2f)
type = \"TintMaterial\"
vectors[\"color\"] = [{}, {}, {}, 1]
",
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
    let path = dir.join("wall.material");
    std::fs::write(&path, tint_file([0.9, 0.05, 0.05])).unwrap();
    // The folder is the project the file's paths are read from.
    let mut live = LiveMaterialFiles::new(Path::new(ENGINE_SHADER_DIR), &dir);
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
    save_as(
        &path,
        "[material version=2]
type = ",
    );
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
    assert!(error.contains("wall.material"), "{error}");

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
