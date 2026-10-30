//! ADR 0107 §8, ADR 0123 E10-3d and CLAUDE.md §5: the editor's frames make no heap
//! allocation in a steady state with a material graph's tab shown: its canvas drawn, the
//! pointer going over its boxes, ports and the empty canvas so every frame lights something
//! else; then likewise with an animation graph's tab shown (ADR 0124 E12-5d-1), the pointer
//! over its states, their edges, its arrows (their conditions shown) and Any State. The
//! measuring thread's allocations are counted; a failing measurement prints the stacks of
//! its first ones. One test per file, so nothing else allocates on that thread.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_app::{Game, Player, PlayerSettings, Window};
use engine_ecs::World;
use engine_platform::window::{MouseButton, WindowEvent, WindowEventKind, WindowId};
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

/// The measurement's allocations so far, their first ones' stacks printed (by their first
/// frames in the engine's crates, with their lines).
fn report(what: &str) -> usize {
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    println!("{allocations} allocations in 240 frames of {what}");
    if allocations > 0
        && let Ok(traces) = TRACES.lock()
    {
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
    allocations
}

/// An animation graph: Idle (the entry), Walk and Run, transitions both ways between Idle
/// and Walk, one from Walk into Run, one from Any State whose condition does not compile;
/// and Move, playing a 2D blend of five clips (ADR 0124's E12-5d-2a), inside the others'
/// frame so the canvas frames them as before.
fn hero() -> String {
    format!(
        "[agraph version=1 id={}]\n\n\
         params[\"speed\"].kind = Float\nparams[\"dir\"].kind = Float\n\
         layers[\"base\"].name = \"Base\"\nlayers[\"base\"].root = \"m\"\n\
         nodes[\"m\"].kind = Machine\nnodes[\"m\"].entry = \"idle\"\n\
         nodes[\"m\"].at = [-220, -120]\n\
         nodes[\"ci\"].clip = \"Idle\"\nnodes[\"cw\"].clip = \"Walk\"\n\
         nodes[\"cr\"].clip = \"Run\"\n\
         states[\"idle\"].machine = \"m\"\nstates[\"idle\"].name = \"Idle\"\n\
         states[\"idle\"].pose = \"ci\"\n\
         states[\"walk\"].at = [320, 0]\nstates[\"walk\"].machine = \"m\"\n\
         states[\"walk\"].name = \"Walk\"\nstates[\"walk\"].pose = \"cw\"\n\
         states[\"run\"].at = [320, 200]\nstates[\"run\"].machine = \"m\"\n\
         states[\"run\"].name = \"Run\"\nstates[\"run\"].pose = \"cr\"\n\
         nodes[\"b\"].kind = Blend2d\nnodes[\"b\"].param = \"dir\"\n\
         nodes[\"b\"].param_y = \"speed\"\n\
         nodes[\"b\"].points.0.clip = \"Idle\"\n\
         nodes[\"b\"].points.1.clip = \"Walk\"\nnodes[\"b\"].points.1.at = [0, 1.5]\n\
         nodes[\"b\"].points.2.clip = \"Run\"\nnodes[\"b\"].points.2.at = [0, 4]\n\
         nodes[\"b\"].points.3.clip = \"Left\"\nnodes[\"b\"].points.3.at = [-1, 1.5]\n\
         nodes[\"b\"].points.4.clip = \"Right\"\nnodes[\"b\"].points.4.at = [1, 1.5]\n\
         states[\"move\"].at = [0, 200]\nstates[\"move\"].machine = \"m\"\n\
         states[\"move\"].name = \"Move\"\nstates[\"move\"].pose = \"b\"\n\
         transitions[\"t1\"].from = \"idle\"\ntransitions[\"t1\"].to = \"walk\"\n\
         transitions[\"t1\"].when = \"speed > 0.1\"\n\
         transitions[\"t2\"].from = \"walk\"\ntransitions[\"t2\"].to = \"idle\"\n\
         transitions[\"t2\"].when = \"speed <= 0.1\"\n\
         transitions[\"t3\"].from = \"walk\"\ntransitions[\"t3\"].to = \"run\"\n\
         transitions[\"t3\"].when = \"speed > 3\"\n\
         transitions[\"t4\"].to = \"run\"\ntransitions[\"t4\"].when = \"sprint\"\n",
        AssetId::new()
    )
}

#[test]
fn a_graph_tabs_frames_do_not_allocate() {
    let dir = std::env::temp_dir().join(format!("ofelia-graph-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("materials")).unwrap();
    std::fs::create_dir_all(dir.join("anim")).unwrap();
    std::fs::write(dir.join("anim/hero.agraph"), hero()).unwrap();
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
         \"materials/moss.mgraph\"\n\n[graph]\npath = \"anim/hero.agraph\"\n",
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
    // The animation graph's, at its framing (80 %) in the same layout: Any State and its
    // edge, Idle and its edge, the arrows between Idle and Walk, Walk into Run, the red one
    // from Any State, the empty canvas, Run.
    let anim_spots: [(i32, i32); 9] = [
        (116, 145),
        (73, 145),
        (300, 258),
        (359, 250),
        (430, 244),
        (560, 330),
        (430, 331),
        (600, 150),
        (560, 415),
    ];
    // The blend panel's (E12-5d-2a), as fractions of it: off its clips' dots, round the
    // plane. A state to select before a frame, the button pressed or let go after its move.
    const BLEND_SPOTS: [[f32; 2]; 6] = [
        [0.3, 0.3],
        [0.7, 0.3],
        [0.95, 0.5],
        [0.7, 0.8],
        [0.3, 0.8],
        [0.05, 0.5],
    ];
    let on_panel = std::cell::Cell::new(false);
    let select = std::cell::Cell::new(None::<&str>);
    let button = std::cell::Cell::new(None::<bool>);
    let mut frame = |n: usize, anim: bool, keys: &[(u16, u16, bool)]| {
        if let Some(state) = select.take() {
            game.select_graph_state(state);
        }
        for &(vk, scancode, pressed) in keys {
            game.event(&WindowEvent {
                window: WindowId(0),
                time: std::time::Instant::now(),
                kind: WindowEventKind::Key {
                    virtual_key: vk,
                    scancode,
                    extended: false,
                    pressed,
                    repeat: false,
                },
            });
        }
        let (x, y) = if on_panel.get()
            && let Some([bx, by, bw, bh]) = game.blend_rect()
        {
            let [fx, fy] = BLEND_SPOTS[n % BLEND_SPOTS.len()];
            ((bx + fx * bw) as i32, (by + fy * bh) as i32)
        } else if anim {
            anim_spots[n % anim_spots.len()]
        } else {
            spots[n % spots.len()]
        };
        game.event(&WindowEvent {
            window: WindowId(0),
            time: std::time::Instant::now(),
            kind: WindowEventKind::CursorMoved { x, y },
        });
        if let Some(pressed) = button.take() {
            game.event(&WindowEvent {
                window: WindowId(0),
                time: std::time::Instant::now(),
                kind: WindowEventKind::MouseButton {
                    button: MouseButton::Left,
                    pressed,
                },
            });
        }
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
        pending = frame(n, false, &[]);
        n += 1;
        assert!(
            started.elapsed().as_secs() < 90,
            "material types still on their way"
        );
    }
    // Its nodes' previews (E10-4g) ask for their own pipeline once the type is made: rounds
    // of frames until one has none on its way (their atlas drawn, read back and shown).
    loop {
        let most = (0..120).map(|n| frame(n, false, &[])).max().unwrap_or(0);
        if most == 0 {
            break;
        }
        assert!(
            started.elapsed().as_secs() < 90,
            "the nodes' previews' pipeline still on its way"
        );
    }
    COUNTING.with(|c| c.set(true));
    for n in 0..240 {
        frame(n, false, &[]);
    }
    COUNTING.with(|c| c.set(false));
    let material = report("a material graph's tab");
    // Ctrl+Tab: the animation graph's tab; its frames as they come with the pointer on its
    // round, then measured alike.
    frame(0, true, &[(0x11, 0x1D, true), (0x09, 0x0F, true)]);
    frame(1, true, &[(0x09, 0x0F, false), (0x11, 0x1D, false)]);
    for n in 0..240 {
        frame(n, true, &[]);
    }
    if let Ok(mut traces) = TRACES.lock() {
        traces.clear();
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for n in 0..240 {
        frame(n, true, &[]);
    }
    COUNTING.with(|c| c.set(false));
    let anim = report("an animation graph's tab");
    // Move selected: its blend's panel in the inspector (E12-5d-2a), its mark dragged round
    // the plane (the weights and their texts each frame), measured alike.
    select.set(Some("move"));
    on_panel.set(true);
    for n in 0..60 {
        frame(n, true, &[]);
    }
    button.set(Some(true));
    for n in 0..240 {
        frame(n, true, &[]);
    }
    if let Ok(mut traces) = TRACES.lock() {
        traces.clear();
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for n in 0..240 {
        frame(n, true, &[]);
    }
    COUNTING.with(|c| c.set(false));
    let blend = report("a blend panel's mark dragged");
    button.set(Some(false));
    frame(0, true, &[]);
    let (now, weights, _) = game.blend_panel().expect("the blend panel shown");
    println!("the blend panel after the drag: {now}, {weights:?}");
    assert!(
        !now.starts_with("dir 0.00 \u{b7} speed 0.00"),
        "the mark moved: {now}"
    );
    // It was the animation graph's canvas that was measured: its entry's amber title bar.
    let shot = player
        .capture_overlaid(&mut game, Some(&mut overlay))
        .unwrap()
        .unwrap();
    let amber = shot
        .chunks_exact(4)
        .filter(|p| {
            let (r, g, b) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
            r > 1.4 * g && g > 2.0 * b && (95.0..150.0).contains(&r)
        })
        .count();
    assert!(amber > 1_000, "the animation graph's tab shown: {amber}");
    // `ENGINE_EDITOR_AGRAPH_ALLOC_PNG=<file>` writes it: the pointer on Walk's arrow into Run.
    if let Ok(path) = std::env::var("ENGINE_EDITOR_AGRAPH_ALLOC_PNG") {
        let png = engine_render::png::encode_png(
            width,
            height,
            engine_render::png::PngFormat::Rgba,
            &shot,
        )
        .unwrap();
        std::fs::write(path, png).unwrap();
    }
    overlay.destroy(&mut player).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        material, 0,
        "allocations in the material graph tab's frames"
    );
    assert_eq!(anim, 0, "allocations in the animation graph tab's frames");
    assert_eq!(blend, 0, "allocations in the blend panel's frames");
}
