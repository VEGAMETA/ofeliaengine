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
use std::backtrace::Backtrace;
use std::cell::{Cell, RefCell};

struct CountingAllocator;

thread_local! {
    // Each measuring thread counts its own: the tests run at once on their own threads.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static COUNTED: Cell<usize> = const { Cell::new(0) };
    // The stack of the first allocation counted, which a failure prints; what capturing it
    // allocates is not counted.
    static FIRST: RefCell<Option<Backtrace>> = const { RefCell::new(None) };
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
}

fn count() {
    let counting = COUNTING.try_with(Cell::get).unwrap_or(false);
    if !counting || CAPTURING.try_with(Cell::get).unwrap_or(true) {
        return;
    }
    COUNTED.set(COUNTED.get() + 1);
    if COUNTED.get() == 1 {
        CAPTURING.set(true);
        let stack = Backtrace::force_capture();
        FIRST.with_borrow_mut(|first| *first = Some(stack));
        CAPTURING.set(false);
    }
}

/// Runs `f` counting this thread's allocations: how many, and the first one's stack.
fn counted(f: impl FnOnce()) -> (usize, String) {
    COUNTED.set(0);
    COUNTING.set(true);
    f();
    COUNTING.set(false);
    let first = FIRST.with_borrow_mut(Option::take);
    (
        COUNTED.get(),
        first.map(|s| s.to_string()).unwrap_or_default(),
    )
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

#[test]
fn a_changing_text_is_laid_out_into_layouts_long_out_of_use() {
    use std::fmt::Write as _;
    let mut text = UiText::new().unwrap();
    let mut label = String::with_capacity(64);
    // A frame rate, as the status line shows it: a new text every frame.
    let mut frame = |text: &mut UiText, n: u32| {
        label.clear();
        let _ = write!(label, "{} fps · {}.{} ms", 1000 + n, n % 97, n % 89);
        std::hint::black_box(text.measure(&label, 13.0, None));
        text.end_frame();
    };
    // Warm-up: the glyphs, and layouts enough to cycle through.
    for n in 0..400 {
        frame(&mut text, n);
    }
    let (made, kept) = (text.layouts_made(), text.cached_layouts());
    let (n, first) = counted(|| {
        for n in 400..1000 {
            frame(&mut text, n);
        }
    });
    assert_eq!(
        n, 0,
        "{n} allocations laying out 600 new texts, the first:\n{first}"
    );
    assert_eq!(text.layouts_made(), made + 600, "each laid out once");
    assert!(
        text.cached_layouts() <= kept + 1,
        "{} kept",
        text.cached_layouts()
    );
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
        script: false,
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
                lock: None,
            },
            changed: true,
        },
        Field {
            name: "Rotation",
            widget: Widget::Numbers {
                values: &["0", "45", "0"],
                labels: &["x", "y", "z"],
                typing: None,
                lock: None,
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
                missing: false,
            },
            changed: false,
        },
    ];
    let sections = [Section {
        title: "Transform",
        fields: &fields,
        script: "",
        removable: false,
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
            playing: false,
            progress: None,
            name: "main.scene",
            kind: FileKind::Scene,
        },
        FileItem {
            selected: false,
            thumbnail: None,
            renaming: false,
            playing: true,
            progress: Some(0.4),
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
        scripts: &[],
        active_script: None,
        code: None,
        graphs: &[],
        active_graph: None,
        graph: None,
        tools: &["Select", "Move"],
        active_tool: 1,
        tree: &tree,
        selection: "Player  ·  CharacterBody",
        prefab: false,
        inspector: &sections,
        movable: [0, 0],
        add_component: true,
        add_at: None,
        view_mode: "Lit",
        view_modes: None,
        path: "res://levels",
        folders: &folders,
        files: &files,
        auto_play: true,
        sound: None,
        material_preview: None,
        clips: None,
        falloff: None,
        tile: 64.0,
        columns: 4,
        bottom_tabs: &["Output", "Debugger"],
        active_bottom: None,
        output: None,
        tasks: None,
        profiler: None,
        mixer: None,
        debugger: None,
        history: None,
        playing: false,
        paused: false,
        stopped: false,
        stop_error: false,
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
        drop_line: None,
        code_side: None,
        docks_beside_code: false,
        tree_filter: "",
        inspector_filter: "",
        palette: Some(PaletteView {
            query: "ac",
            hint: "Type a command",
            rows: &rows,
            selected: 1,
            at: None,
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
    let (n, first) = counted(|| {
        for _ in 0..100 {
            frame(&mut ui, &mut text);
        }
    });
    assert_eq!(n, 0, "{n} allocations in 100 frames, the first:\n{first}");
    assert_eq!(
        text.layouts_made(),
        made,
        "steady frames lay nothing out again"
    );
}
