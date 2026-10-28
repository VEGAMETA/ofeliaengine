//! ADR 0107 §8, ADR 0123 E10-3d and CLAUDE.md §5: the editor's frames make no heap
//! allocation in a steady state with a material graph's tab shown: its canvas drawn, the
//! pointer going over its boxes, ports and the empty canvas so every frame lights something
//! else. The measuring thread's allocations are counted; a failing measurement prints the
//! stacks of its first ones. One test per file, so nothing else allocates on that thread.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_platform::window::{WindowEvent, WindowEventKind, WindowId};
use engine_scene::AssetId;
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
fn a_graph_tabs_frames_do_not_allocate() {
    let dir = std::env::temp_dir().join(format!("ofelia-graph-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
    std::fs::write(dir.join("main.scene"), "[scene version=4]\n").unwrap();
    std::fs::write(
        dir.join("materials/moss.mgraph"),
        format!(
            "[mgraph version=2 id={}]\n\nmodel = \"lit\"\n\
             nodes[\"c\"].kind = \"color\"\nnodes[\"c\"].value = [0.25, 0.55, 0.2, 1]\n\
             nodes[\"k\"].kind = \"number\"\nnodes[\"k\"].at = [0, 110]\n\
             nodes[\"k\"].value = [0.8]\n\
             nodes[\"m\"].kind = \"multiply\"\nnodes[\"m\"].at = [220, 30]\n\
             nodes[\"out\"].kind = \"output\"\nnodes[\"out\"].at = [460, 0]\n\
             nodes[\"rr\"].kind = \"reroute\"\nnodes[\"rr\"].at = [380, 60]\n\
             frames[\"frame1\"].at = [-20, -46]\nframes[\"frame1\"].size = [220, 220]\n\
             frames[\"frame1\"].title = \"Tint\"\n\
             links[\"m.a\"] = \"c.rgba\"\nlinks[\"m.b\"] = \"k.value\"\n\
             links[\"rr.in\"] = \"m.out\"\nlinks[\"out.base\"] = \"rr.out\"\n",
            AssetId::new()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join(".ofelia/session"),
        "[session version=1]\n\n[editor]\ngraph = 0\n\n[graph]\npath = \
         \"materials/moss.mgraph\"\n",
    )
    .unwrap();
    let (width, height) = (1280u32, 720u32);
    let settings = PlayerSettings {
        title: String::from("graph allocations"),
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
    game.begin_session(&mut player).unwrap();
    player.set_frame_time(Some(1.0 / 60.0));
    // The pointer goes round a loop over the canvas: the boxes, their ports, the links and
    // the empty canvas, every frame something else under it.
    let spots: [(i32, i32); 6] = [
        (100, 175),
        (170, 197),
        (300, 200),
        (280, 227),
        (450, 300),
        (560, 240),
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
        player.renderer().forward().material_types_pending()
    };
    // The steady state: the graph's preview made (its module's type compiled, the sphere
    // drawn and read back), then frames as they come.
    let started = std::time::Instant::now();
    let (mut n, mut pending) = (0, 1);
    while n < 240 || pending > 0 {
        pending = frame(n);
        n += 1;
        assert!(
            started.elapsed().as_secs() < 90,
            "material types still on their way"
        );
    }
    for n in 0..120 {
        frame(n);
    }
    COUNTING.with(|c| c.set(true));
    for n in 0..240 {
        frame(n);
    }
    COUNTING.with(|c| c.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    println!("{allocations} allocations in 240 frames of a graph's tab");
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
    assert_eq!(allocations, 0, "allocations in the graph tab's frames");
}
