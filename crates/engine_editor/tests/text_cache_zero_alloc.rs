//! The editor's text: a string is laid out once while it stays on screen, and a steady
//! frame of the shell (build, layout with the real fonts) makes no heap allocation
//! (CLAUDE.md §5).

#![allow(clippy::unwrap_used)]

use engine_editor::icons;
use engine_editor::paint::UiText;
use engine_editor::shell::{
    Field, FileItem, FileKind, MenuEntry, Palette, PaletteRow, PaletteView, Section, ShellModel,
    TreeItem, Widget, build,
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
        active: false,
        renaming: false,
        disabled: false,
        off: false,
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
            widget: Widget::Numbers {
                values: &["0.5", "0.9", "2"],
                labels: &["x", "y", "z"],
                typing: None,
            },
            changed: true,
        },
        Field {
            name: "Rotation",
            widget: Widget::Numbers {
                values: &["0", "45", "0"],
                labels: &["x", "y", "z"],
                typing: None,
            },
            changed: true,
        },
        Field {
            name: "Visible",
            widget: Widget::Check(Some(true)),
            changed: false,
        },
        Field {
            name: "Mode",
            widget: Widget::Choice {
                current: "Opaque",
                open: Some(&["Opaque", "Mask", "Blend"]),
            },
            changed: false,
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
            selected: false,
            thumbnail: None,
            renaming: false,
            name: "main.scene",
            kind: FileKind::Scene,
        },
        FileItem {
            selected: false,
            thumbnail: None,
            renaming: false,
            name: "player.luau",
            kind: FileKind::Script,
        },
    ];
    // The command palette open over it all, a query typed.
    let rows = [
        PaletteRow {
            icon: "",
            label: "Scene: Add Cube",
            matched: &[7, 11],
            category: 7,
            detail: "",
        },
        PaletteRow {
            icon: "",
            label: "Scene: Duplicate Selection",
            matched: &[],
            category: 7,
            detail: "Ctrl+D",
        },
    ];
    let model = ShellModel {
        scenes: &["main.scene", "меню.scene"],
        active_scene: 0,
        tools: &["Select", "Move"],
        active_tool: 1,
        tree: &tree,
        selection: "Player  ·  CharacterBody",
        prefab: false,
        inspector: &sections,
        path: "res://levels",
        folders: &folders,
        files: &files,
        tile: 64.0,
        columns: 4,
        bottom_tabs: &["Output", "Debugger"],
        active_bottom: None,
        playing: false,
        paused: false,
        status: "engine 0.0.0",
        snap: true,
        steps: ["0.1 m", "45°", "0.1"],
        step_menu: None,
        pivot: "Median",
        add_menu: Some((
            &[
                MenuEntry {
                    icon: icons::NODE_3D,
                    label: "Point",
                    kind: 0,
                    children: &[],
                },
                MenuEntry {
                    icon: icons::MODEL,
                    label: "Mesh",
                    kind: usize::MAX,
                    children: &[MenuEntry {
                        icon: icons::CUBE,
                        label: "Cube",
                        kind: 1,
                        children: &[],
                    }],
                },
            ],
            Some(1),
        )),
        reveal_row: Some(3),
        drop_row: None,
        tree_filter: "",
        inspector_filter: "",
        palette: Some(PaletteView {
            query: "ac",
            hint: "Type a command",
            rows: &rows,
            selected: 1,
        }),
        hint: None,
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
