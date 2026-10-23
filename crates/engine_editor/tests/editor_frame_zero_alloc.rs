//! ADR 0107 §8 and CLAUDE.md §5: the editor's frames make no heap allocation in a steady
//! state, its panels included: a scene of named entities in the tree, one selected in the
//! inspector, files in the dock, the pointer moving over the viewport and the panels so
//! every frame is built anew. The
//! measuring thread's allocations are counted; a failing measurement prints the stacks of
//! its first ones. One test per file, so nothing else allocates on that thread.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_core::math::Vec3;
use engine_ecs::{Transform, World};
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use engine_scene::{AssetId, Name, SaveOptions, SceneId, SceneRegistry, save_with};
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

#[test]
fn the_editors_frames_do_not_allocate() {
    let dir = std::env::temp_dir().join(format!("ofelia-editor-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("props")).unwrap();
    // A scene of named entities, nested; a few files beside it.
    let mut world = World::new();
    for i in 0..20 {
        let e = world.spawn((
            SceneId::new(),
            Name::new(format!("Crate {i}")),
            Transform::from_xyz(i as f32, 0.0, 0.0),
        ));
        if i % 4 == 0 {
            let child = world.spawn((
                SceneId::new(),
                Name::new(format!("Lid {i}")),
                Transform::from_translation(Vec3::Y),
            ));
            world.set_parent(child, Some(e)).unwrap();
        }
    }
    let mut text = String::new();
    let options = SaveOptions {
        id: Some(AssetId::new()),
        assets: None,
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
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join("props").join(name), "a note\n").unwrap();
    }
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("editor allocations"),
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
    // An entity selected: the inspector shows its components too.
    let crate_3 = engine_scene::find_named(player.world_mut(), "Crate 3").unwrap();
    game.select(Some(crate_3));
    // The pointer goes round a loop over the viewport, the tree and the dock: every frame
    // has something new under it, so every frame is built.
    let spots: [(i32, i32); 6] = [
        (640, 360),
        (700, 300),
        (120, 160),
        (120, 220),
        (120, 600),
        (1180, 200),
    ];
    let mut frame = |n: usize| {
        let (x, y) = spots[n % spots.len()];
        game.event(&WindowEvent {
            window: WindowId(0),
            time: std::time::Instant::now(),
            kind: WindowEventKind::CursorMoved { x, y },
        });
        player
            .frame_overlaid(&mut game, Some(&mut overlay))
            .unwrap();
        player.set_view_rect(game.view_rect());
        game.sync_assets(&mut player);
    };
    for n in 0..240 {
        frame(n);
    }
    COUNTING.with(|c| c.set(true));
    for n in 0..240 {
        frame(n);
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(game.selection(), [crate_3], "still selected");
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    println!("{allocations} allocations in 240 frames of the editor");
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
    assert_eq!(allocations, 0, "allocations in the editor's frames");
}
