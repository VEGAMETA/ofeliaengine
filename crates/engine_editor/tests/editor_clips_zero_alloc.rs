//! ADR 0124's E12-1b and CLAUDE.md §5: the editor's frames make no heap allocation while a
//! character plays in place: CesiumMan selected, his Clips in the inspector, P pressed, so
//! every frame poses him anew and writes the Clips' time again. The measuring thread's
//! allocations are counted; a failing measurement prints the stacks of its first ones. One
//! test per file, so nothing else allocates on that thread.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Model, Player, PlayerSettings, Window};
use engine_ecs::{Transform, World};
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use engine_scene::{
    AssetId, AssetMeta, AssetPaths, AssetRef, Name, SaveOptions, SceneId, SceneRegistry, save_with,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoGame;

impl Game for NoGame {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
}

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// The stacks of the measurement's first allocations.
static TRACES: Mutex<Vec<Backtrace>> = Mutex::new(Vec::new());
const TRACED: usize = 64;

thread_local! {
    /// Set on the measuring thread while it measures.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Set while a stack is captured: what the capture allocates is not counted.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
}

fn count() {
    let counting = COUNTING.try_with(Cell::get).unwrap_or(false);
    let capturing = CAPTURING.try_with(Cell::get).unwrap_or(true);
    if !counting || capturing {
        return;
    }
    if ALLOCATIONS.fetch_add(1, Ordering::Relaxed) < TRACED {
        CAPTURING.with(|c| c.set(true));
        let trace = Backtrace::force_capture();
        if let Ok(mut traces) = TRACES.lock() {
            traces.push(trace);
        }
        CAPTURING.with(|c| c.set(false));
    }
}

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// One editor frame on the player.
fn frame(
    player: &mut Player,
    game: &mut engine_editor::EditorGame<NoGame>,
    overlay: &mut engine_editor::EditorOverlay,
) {
    player.frame_overlaid(game, Some(overlay)).unwrap();
    player.set_view_rect(game.view_rect());
    game.sync_assets(player);
}

#[test]
fn the_editors_frames_do_not_allocate_while_a_character_plays_in_place() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/CesiumMan");
    if !source.join("CesiumMan.gltf").exists() {
        println!(
            "skipped: {} missing (tools/fetch-test-assets.ps1)",
            source.display()
        );
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("ofelia-editor-clips-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    for file in ["CesiumMan.gltf", "CesiumMan_data.bin", "CesiumMan_img0.jpg"] {
        std::fs::copy(source.join(file), dir.join("models").join(file)).unwrap();
    }
    let id = AssetMeta::ensure(&dir.join("models/CesiumMan.gltf")).unwrap();
    let (mesh, material) = (
        AssetRef::part(id, "Cesium_Man"),
        AssetRef::part(id, "Cesium_Man-effect"),
    );
    let mut paths = AssetPaths::new();
    paths.insert(mesh, "models/CesiumMan.gltf#Cesium_Man");
    paths.insert(material, "models/CesiumMan.gltf#Cesium_Man-effect");
    let mut world = World::new();
    world.spawn((
        SceneId::new(),
        Name::new("Man"),
        Transform::IDENTITY,
        Model::new(mesh, material),
    ));
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: Some(&paths),
        prefabs: None,
    };
    save_with(
        &mut world,
        &SceneRegistry::from_program(),
        &options,
        &mut text,
    )
    .unwrap();
    std::fs::write(dir.join("main.scene"), &text).unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor allocations in place"),
        window: Window::Offscreen { width, height },
        scene: Some("main.scene".into()),
        project: Some(dir.clone()),
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => {
            println!("skipping (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let (mut game, mut overlay) =
        engine_editor::editor(&mut player, &settings, |_| Ok(NoGame)).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    let man = engine_scene::find_named(player.world_mut(), "Man").unwrap();
    game.select(Some(man));
    // His skin made, his Clips shown.
    for _ in 0..600 {
        frame(&mut player, &mut game, &mut overlay);
        if !game.clips().0.is_empty() {
            break;
        }
    }
    assert!(!game.clips().0.is_empty(), "his clips");
    // P: he plays in place.
    for pressed in [true, false] {
        game.event(&WindowEvent {
            window: WindowId(0),
            time: std::time::Instant::now(),
            kind: WindowEventKind::Key {
                virtual_key: 0x50,
                scancode: 0x19,
                extended: false,
                pressed,
                repeat: false,
            },
        });
    }
    for _ in 0..240 {
        frame(&mut player, &mut game, &mut overlay);
    }
    assert_eq!(game.playing_in_place(), 1, "playing in place");
    let shown = game.clips().1;
    COUNTING.with(|c| c.set(true));
    for _ in 0..240 {
        frame(&mut player, &mut game, &mut overlay);
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(game.playing_in_place(), 1, "still playing");
    assert_ne!(game.clips().1, shown, "the time moved on");
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    println!("{allocations} allocations in 240 frames of a character playing in place");
    if allocations > 0
        && let Ok(traces) = TRACES.lock()
    {
        // Each one by its first frames in the engine's crates, with their lines.
        for (k, trace) in traces.iter().enumerate() {
            let text = trace.to_string();
            let lines: Vec<&str> = text.lines().map(str::trim).collect();
            let mut ours = Vec::new();
            for (i, l) in lines.iter().enumerate() {
                if l.contains(": engine_") && !l.contains("tests::") {
                    let symbol = l.split(": ").nth(1).unwrap_or(l);
                    let at = lines
                        .get(i + 1)
                        .filter(|n| n.starts_with("at "))
                        .and_then(|n| n.rsplit(['\\', '/']).next())
                        .unwrap_or("");
                    ours.push(format!("{symbol} @ {at}"));
                    if ours.len() == 3 {
                        break;
                    }
                }
            }
            println!("allocation {}: {}", k + 1, ours.join(" <- "));
        }
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        allocations, 0,
        "allocations while a character plays in place"
    );
}
