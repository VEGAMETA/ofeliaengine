//! The editor's text: a string is laid out once while it stays on screen, and a steady
//! frame of the shell (build, layout with the real fonts) makes no heap allocation
//! (CLAUDE.md §5).

#![allow(clippy::unwrap_used)]

use engine_editor::icons;
use engine_editor::paint::UiText;
use engine_editor::shell::{
    Field, FileItem, FileKind, Palette, Section, ShellModel, TreeItem, build,
};
use engine_ui::{TextMeasure, Theme, Ui, UiInput};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Count the measuring thread only: other tests run on their own threads.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn count() {
    if COUNTING.with(Cell::get) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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
fn a_string_is_laid_out_once() {
    let mut text = UiText::new().unwrap();
    let label = "\u{F040A} Играть →";
    let size = text.measure(label, 13.0, None);
    let made = text.layouts_made();
    assert_eq!(text.measure(label, 13.0, None), size);
    assert_eq!(
        text.layouts_made(),
        made,
        "the second measure reuses the layout"
    );
    // Another size or wrap width is another layout.
    text.measure(label, 14.0, None);
    text.measure(label, 13.0, Some(40.0));
    assert_eq!(text.layouts_made(), made + 2);
    assert_eq!(text.cached_layouts(), 3);
}

fn item<'a>(depth: u16, icon: &'a str, name: &'a str, kind: &'a str) -> TreeItem<'a> {
    TreeItem {
        depth,
        icon,
        name,
        kind,
        expanded: None,
        selected: false,
    }
}

#[test]
fn a_steady_shell_frame_does_not_allocate() {
    let tree = [
        item(0, icons::NODE_3D, "World", "Node3D"),
        item(1, icons::SUN, "Sun", "DirectionalLight"),
        item(1, icons::CHARACTER, "Player", "CharacterBody"),
        item(2, icons::CAMERA, "Camera", "Camera3D"),
        item(1, icons::CONTROL, "Интерфейс", "Control"),
    ];
    let fields = [
        Field {
            name: "Position",
            value: "0.50   0.90   2.00",
        },
        Field {
            name: "Rotation",
            value: "0°   45°   0°",
        },
    ];
    let sections = [Section {
        title: "Transform",
        fields: &fields,
    }];
    let folders = [
        item(0, icons::FOLDER_OPEN, "res://", ""),
        item(1, icons::FOLDER, "levels", ""),
    ];
    let files = [
        FileItem {
            name: "main.scene",
            kind: FileKind::Scene,
        },
        FileItem {
            name: "player.luau",
            kind: FileKind::Script,
        },
    ];
    let model = ShellModel {
        scenes: &["main.scene", "меню.scene"],
        active_scene: 0,
        tools: &["Select", "Move"],
        active_tool: 1,
        tree: &tree,
        selection: "Player  ·  CharacterBody",
        inspector: &sections,
        path: "res://levels",
        folders: &folders,
        files: &files,
        bottom_tabs: &["Output", "Debugger"],
        active_bottom: None,
        playing: false,
        status: "engine 0.0.0",
    };
    let palette = Palette::default();
    let mut ui = Ui::new(Theme::default());
    let mut text = UiText::new().unwrap();
    let input = UiInput::default();
    let frame = |ui: &mut Ui, text: &mut UiText| {
        ui.begin_frame(&input, [1600.0, 900.0]);
        build(ui, &model, &palette);
        std::hint::black_box(ui.end_frame(text).commands.len());
    };
    for _ in 0..5 {
        frame(&mut ui, &mut text);
    }
    let made = text.layouts_made();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for _ in 0..100 {
        frame(&mut ui, &mut text);
    }
    COUNTING.with(|c| c.set(false));
    let n = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(n, 0, "{n} allocations in 100 frames");
    assert_eq!(
        text.layouts_made(),
        made,
        "steady frames lay nothing out again"
    );
}
