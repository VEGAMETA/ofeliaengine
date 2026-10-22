//! Layout, interaction, keyboard focus, scrolling, floating layers and draw order.

#![allow(clippy::unwrap_used, clippy::float_cmp)]

use engine_ui::*;

fn frame(ui: &mut Ui, input: &UiInput, build: impl FnOnce(&mut Ui)) -> Vec<DrawCmd> {
    ui.begin_frame(input, [400.0, 300.0]);
    build(ui);
    ui.end_frame(&mut MonospaceMeasure).commands.to_vec()
}

fn at(x: f32, y: f32) -> UiInput {
    UiInput {
        mouse: Some([x, y]),
        ..UiInput::default()
    }
}

#[test]
fn grow_fixed_fit_percent_padding_and_gap() {
    let mut ui = Ui::new(Theme::default());
    let (mut a, mut b, mut c, mut d, mut t) = (0, 0, 0, 0, 0);
    frame(&mut ui, &UiInput::default(), |ui| {
        ui.row("row", Style::fill().padding(10.0).gap(5.0), |ui| {
            a = ui.container(
                "a",
                Style::default().size(Size::Fixed(50.0), Size::Grow(1.0)),
                |_| {},
            );
            b = ui.container(
                "b",
                Style::default().size(Size::Grow(1.0), Size::Fixed(20.0)),
                |_| {},
            );
            c = ui.container(
                "c",
                Style::default().size(Size::Grow(2.0), Size::Percent(0.5)),
                |_| {},
            );
            d = ui.column("d", Style::default().padding(2.0), |ui| {
                t = ui.label("12345678"); // 8 chars × 7 px at 14 px
            });
        });
    });
    // Inner width 380, gaps 15, fixed 50, d = 56 + 4 → 255 left for b:c = 1:2.
    let r = |id| ui.rect_of(id).unwrap();
    assert_eq!(r(a).size, [50.0, 280.0]);
    assert_eq!(r(a).pos, [10.0, 10.0]);
    assert!((r(b).size[0] - 255.0 / 3.0).abs() < 1e-3);
    assert!((r(c).size[0] - 255.0 * 2.0 / 3.0).abs() < 1e-3);
    assert_eq!(r(c).size[1], 140.0);
    assert_eq!(r(d).size, [60.0, 14.0 * 1.25 + 4.0]);
    assert_eq!(r(t).pos, [r(d).pos[0] + 2.0, r(d).pos[1] + 2.0]);
    assert_eq!(
        r(d).pos[0] + r(d).size[0],
        390.0,
        "the row ends at the padding"
    );
}

#[test]
fn wrapped_text_height_follows_the_width() {
    let mut ui = Ui::new(Theme::default());
    let mut p = 0;
    frame(&mut ui, &UiInput::default(), |ui| {
        ui.column(
            "c",
            Style::default().size(Size::Fixed(70.0), Size::Fit),
            |ui| {
                // 30 chars at 7 px = 210 px, wrapped at 70 px → 3 lines.
                p = ui.paragraph("p", "abcdefghijklmnopqrstuvwxyz0123");
            },
        );
    });
    assert_eq!(ui.rect_of(p).unwrap().size, [70.0, 3.0 * 14.0 * 1.25]);
}

#[test]
fn centered_children() {
    let mut ui = Ui::new(Theme::default());
    let mut x = 0;
    frame(&mut ui, &UiInput::default(), |ui| {
        ui.row(
            "r",
            Style::fill().align(Align::Center, Align::Center),
            |ui| {
                x = ui.container(
                    "x",
                    Style::default().size(Size::Fixed(100.0), Size::Fixed(50.0)),
                    |_| {},
                );
            },
        );
    });
    assert_eq!(ui.rect_of(x).unwrap().pos, [150.0, 125.0]);
}

fn one_button(ui: &mut Ui) -> Response {
    let mut r = Response::default();
    ui.column("root", Style::fill().padding(10.0), |ui| {
        r = ui.button("OK")
    });
    r
}

#[test]
fn click_is_press_then_release_over_the_button() {
    let mut ui = Ui::new(Theme::default());
    let mut r = Response::default();
    frame(&mut ui, &at(15.0, 15.0), |ui| r = one_button(ui));
    assert!(!r.hovered, "no layout yet on the first frame");
    let press = UiInput {
        mouse_down: true,
        mouse_pressed: true,
        ..at(15.0, 15.0)
    };
    frame(&mut ui, &press, |ui| r = one_button(ui));
    assert!(r.hovered && r.pressed && r.active && !r.clicked);
    let release = UiInput {
        mouse_released: true,
        ..at(16.0, 16.0)
    };
    frame(&mut ui, &release, |ui| r = one_button(ui));
    assert!(r.clicked);
    frame(&mut ui, &at(16.0, 16.0), |ui| r = one_button(ui));
    assert!(!r.clicked && r.hovered);

    // Pressing, moving off and releasing does not click.
    frame(&mut ui, &press, |ui| r = one_button(ui));
    let away = UiInput {
        mouse_released: true,
        ..at(300.0, 250.0)
    };
    frame(&mut ui, &away, |ui| r = one_button(ui));
    assert!(!r.clicked);
}

#[test]
fn tab_and_enter_work_without_a_mouse() {
    let mut ui = Ui::new(Theme::default());
    let build = |ui: &mut Ui, out: &mut [Response; 3]| {
        ui.column("root", Style::fill(), |ui| {
            for (i, name) in ["One", "Two", "Three"].into_iter().enumerate() {
                out[i] = ui.button(name);
            }
        });
    };
    let mut r = [Response::default(); 3];
    frame(&mut ui, &UiInput::default(), |ui| build(ui, &mut r));
    let keys = |k: Vec<UiKey>, shift| UiInput {
        keys: k,
        shift,
        ..UiInput::default()
    };
    frame(&mut ui, &keys(vec![UiKey::Tab, UiKey::Tab], false), |ui| {
        build(ui, &mut r)
    });
    assert!(r[1].focused && !r[0].focused);
    frame(&mut ui, &keys(vec![UiKey::Enter], false), |ui| {
        build(ui, &mut r)
    });
    assert!(r[1].clicked && !r[0].clicked && !r[2].clicked);
    frame(
        &mut ui,
        &keys(vec![UiKey::Tab, UiKey::Tab, UiKey::Tab], true),
        |ui| build(ui, &mut r),
    );
    assert!(r[1].focused, "shift+tab wraps around");
    let cmds = frame(&mut ui, &keys(vec![UiKey::Escape], false), |ui| {
        build(ui, &mut r)
    });
    assert!(!r.iter().any(|r| r.focused));
    let focus_rings = cmds
        .iter()
        .filter(|c| matches!(c, DrawCmd::Rect { border: Some((col, _)), .. } if *col == Theme::default().focus))
        .count();
    assert_eq!(focus_rings, 0);
}

#[test]
fn wheel_scrolls_the_container_under_the_mouse_and_clamps() {
    let mut ui = Ui::new(Theme::default());
    let mut list = 0;
    let mut first = 0;
    let build = |ui: &mut Ui, list: &mut u64, first: &mut u64| {
        ui.column(
            "root",
            Style::default().size(Size::Fixed(200.0), Size::Fixed(100.0)),
            |ui| {
                *list = ui.scroll_column("list", |ui| {
                    for i in 0..20 {
                        let id = ui.label_keyed(&format!("row{i}"), "row");
                        if i == 0 {
                            *first = id;
                        }
                    }
                });
            },
        );
    };
    frame(&mut ui, &at(50.0, 50.0), |ui| {
        build(ui, &mut list, &mut first)
    });
    // 20 rows of 17.5 px = 350 px in 100 px: 250 px of scroll.
    let wheel = |w| UiInput {
        wheel: w,
        ..at(50.0, 50.0)
    };
    frame(&mut ui, &wheel(-40.0), |ui| {
        build(ui, &mut list, &mut first)
    });
    assert_eq!(ui.scroll_offset(list), 40.0);
    let cmds = frame(&mut ui, &at(50.0, 50.0), |ui| {
        build(ui, &mut list, &mut first)
    });
    assert_eq!(ui.rect_of(first).unwrap().pos[1], -40.0);
    assert!(cmds.contains(&DrawCmd::Clip(Some(Rect::new(0.0, 0.0, 200.0, 100.0)))));
    assert_eq!(cmds.last(), Some(&DrawCmd::Clip(None)), "clip restored");
    frame(&mut ui, &wheel(-10_000.0), |ui| {
        build(ui, &mut list, &mut first)
    });
    assert_eq!(ui.scroll_offset(list), 250.0);
    frame(&mut ui, &wheel(10_000.0), |ui| {
        build(ui, &mut list, &mut first)
    });
    assert_eq!(ui.scroll_offset(list), 0.0);
}

#[test]
fn floating_layers_draw_last_and_win_hit_tests() {
    let mut ui = Ui::new(Theme::default());
    let mut under = Response::default();
    let mut menu = Response::default();
    let build = |ui: &mut Ui, under: &mut Response, menu: &mut Response| {
        ui.column("root", Style::fill(), |ui| {
            ui.container("anchor", Style::default(), |ui| {
                ui.container(
                    "popup",
                    Style {
                        floating: Some([0.0, 0.0]),
                        ..Style::default()
                    },
                    |ui| *menu = ui.button_keyed("menu", "Menu item"),
                );
            });
            *under = ui.button_keyed("under", "Underneath");
        });
    };
    frame(&mut ui, &at(5.0, 5.0), |ui| {
        build(ui, &mut under, &mut menu)
    });
    let cmds = frame(&mut ui, &at(5.0, 5.0), |ui| {
        build(ui, &mut under, &mut menu)
    });
    assert!(
        menu.hovered && !under.hovered,
        "the popup covers the button"
    );
    let texts: Vec<&str> = {
        ui.begin_frame(&at(5.0, 5.0), [400.0, 300.0]);
        build(&mut ui, &mut under, &mut menu);
        let f = ui.end_frame(&mut MonospaceMeasure);
        f.commands
            .iter()
            .filter_map(|c| match c {
                DrawCmd::Text { range, .. } => Some(&f.text[range.clone()]),
                _ => None,
            })
            .map(|s| if s == "Menu item" { "menu" } else { "under" })
            .collect()
    };
    assert_eq!(texts, ["under", "menu"], "floating content paints last");
    assert!(!cmds.is_empty());
}

#[test]
fn ids_depend_on_the_parent() {
    let mut ui = Ui::new(Theme::default());
    let (mut a, mut b) = (0, 0);
    frame(&mut ui, &UiInput::default(), |ui| {
        ui.column("left", Style::default(), |ui| a = ui.label("Name"));
        ui.column("right", Style::default(), |ui| b = ui.label("Name"));
    });
    assert_ne!(a, b);
}

#[test]
fn drags_report_travel_from_the_press_and_their_release() {
    let mut ui = Ui::new(Theme::default());
    let mut r = Response::default();
    frame(&mut ui, &at(15.0, 15.0), |ui| r = one_button(ui));
    let held = |x: f32, y: f32, pressed: bool| UiInput {
        mouse_down: true,
        mouse_pressed: pressed,
        ..at(x, y)
    };
    frame(&mut ui, &held(15.0, 15.0, true), |ui| r = one_button(ui));
    assert_eq!(ui.drag_delta(r.id), Some([0.0, 0.0]));
    // The pointer leaves the button: still its drag, measured from the press.
    frame(&mut ui, &held(140.0, 200.0, false), |ui| r = one_button(ui));
    assert_eq!(ui.drag_delta(r.id), Some([125.0, 185.0]));
    assert!(r.active && !r.released);
    assert_eq!(
        ui.drag_delta(r.id ^ 1),
        None,
        "only the pressed widget drags"
    );
    let up = UiInput {
        mouse_released: true,
        ..at(150.0, 200.0)
    };
    frame(&mut ui, &up, |ui| r = one_button(ui));
    assert!(r.released && !r.clicked, "released away from it: no click");
    assert_eq!(ui.drag_delta(r.id), None);
    frame(&mut ui, &at(150.0, 200.0), |ui| r = one_button(ui));
    assert!(!r.released);
    assert!(ui.hovers(r.id) == ui.rect_of(r.id).unwrap().contains([150.0, 200.0]));
}

fn field_frame(ui: &mut Ui, input: &UiInput, text: &str) -> TextFieldResponse {
    look_frame(ui, input, text, false).0
}

/// A frame with one text field (a search box, autofocused, when `search`) above a row
/// marked as a hover region: the field's response, and whether the mouse is in the row.
fn look_frame(ui: &mut Ui, input: &UiInput, text: &str, search: bool) -> (TextFieldResponse, bool) {
    let mut r = TextFieldResponse::default();
    let mut in_row = false;
    frame(ui, input, |ui| {
        ui.column("root", Style::fill().padding(10.0), |ui| {
            r = ui.text_field(
                "name",
                text,
                &TextField {
                    style: Style::default().size(Size::Fixed(200.0), Size::Fixed(20.0)),
                    size: 14.0,
                    color: [1.0; 4],
                    hint: "Name",
                    hint_color: [0.5; 4],
                    autofocus: search,
                    search,
                },
            );
            let row = ui.container(
                "row",
                Style::default().size(Size::Fixed(200.0), Size::Fixed(20.0)),
                |ui| {
                    ui.container(
                        "button",
                        Style::default().size(Size::Fixed(20.0), Size::Fixed(20.0)),
                        |_| {},
                    );
                    let button = ui.id("button");
                    ui.sense(button, false);
                },
            );
            ui.mark_region(row);
            in_row = ui.in_region(row);
        });
    });
    (r, in_row)
}

fn typing(text: &str) -> UiInput {
    UiInput {
        text: String::from(text),
        ..UiInput::default()
    }
}

fn keys(keys: &[UiKey]) -> UiInput {
    UiInput {
        keys: keys.to_vec(),
        ..UiInput::default()
    }
}

#[test]
fn text_fields_edit_commit_and_cancel() {
    let mut ui = Ui::new(Theme::default());
    field_frame(&mut ui, &UiInput::default(), "Door");
    // A click starts the edit, the caret where the click is (7 px a character): after "Do".
    let press = UiInput {
        mouse_down: true,
        mouse_pressed: true,
        ..at(10.0 + 14.0, 15.0)
    };
    let r = field_frame(&mut ui, &press, "Door");
    assert!(r.started && r.editing);
    field_frame(
        &mut ui,
        &UiInput {
            mouse_released: true,
            ..at(24.0, 15.0)
        },
        "Door",
    );
    let r = field_frame(&mut ui, &typing("o"), "Door");
    assert!(r.changed);
    assert_eq!(ui.edited_text(), "Dooor");
    // Shift+End selects to the end, Backspace removes it, typing goes on.
    field_frame(
        &mut ui,
        &UiInput {
            shift: true,
            ..keys(&[UiKey::End])
        },
        "Door",
    );
    field_frame(&mut ui, &keys(&[UiKey::Backspace]), "Door");
    assert_eq!(ui.edited_text(), "Doo");
    field_frame(&mut ui, &typing("r 1"), "Door");
    // Ctrl+Backspace takes the word before the caret, Ctrl+A and Ctrl+C copy it all.
    field_frame(
        &mut ui,
        &UiInput {
            ctrl: true,
            ..keys(&[UiKey::Backspace])
        },
        "Door",
    );
    assert_eq!(ui.edited_text(), "Door ");
    field_frame(&mut ui, &keys(&[UiKey::SelectAll, UiKey::Copy]), "Door");
    assert_eq!(ui.take_copied().as_deref(), Some("Door "));
    // Paste replaces the selection; Enter commits.
    field_frame(
        &mut ui,
        &UiInput {
            paste: String::from(
                "Gate
2",
            ),
            ..keys(&[UiKey::Paste])
        },
        "Door",
    );
    let r = field_frame(&mut ui, &keys(&[UiKey::Enter]), "Door");
    assert!(r.committed && !r.editing);
    assert_eq!(ui.edited_text(), "Gate 2");
    let r = field_frame(&mut ui, &UiInput::default(), "Gate 2");
    assert!(!r.editing && !r.committed && ui.editing().is_none());

    // Escape cancels; a click elsewhere commits.
    ui.edit_field(r.id);
    let r = field_frame(&mut ui, &typing("x"), "Gate 2");
    assert!(r.started && r.editing);
    assert_eq!(ui.edited_text(), "x", "started with everything selected");
    let r = field_frame(&mut ui, &keys(&[UiKey::Escape]), "Gate 2");
    assert!(r.cancelled && !r.committed);
    ui.edit_field(r.id);
    field_frame(&mut ui, &typing("y"), "Gate 2");
    let away = UiInput {
        mouse_down: true,
        mouse_pressed: true,
        ..at(390.0, 290.0)
    };
    let r = field_frame(&mut ui, &away, "Gate 2");
    assert!(r.committed);
    assert_eq!(ui.edited_text(), "y");
}

#[test]
fn search_boxes_keep_their_prefix_and_leave_up_and_down_to_the_list() {
    let mut ui = Ui::new(Theme::default());
    // Autofocused with the caret at the end: typing goes after the prefix.
    let (r, _) = look_frame(&mut ui, &UiInput::default(), "@", true);
    assert!(r.started && r.editing);
    look_frame(&mut ui, &typing("cu"), "@", true);
    assert_eq!(ui.edited_text(), "@cu");
    // Up and Down do not move the caret (the list under the box takes them).
    look_frame(
        &mut ui,
        &keys(&[UiKey::Up, UiKey::Down, UiKey::Up]),
        "@",
        true,
    );
    look_frame(&mut ui, &typing("b"), "@", true);
    assert_eq!(ui.edited_text(), "@cub");
    // Home still goes to the start.
    look_frame(&mut ui, &keys(&[UiKey::Home]), "@", true);
    look_frame(&mut ui, &typing(">"), "@", true);
    assert_eq!(ui.edited_text(), ">@cub");
}

/// A frame with a list of 20 rows of 20 px in a scroll container 100 px tall; row
/// `reveal` asks to be shown. The list's id and the rows' ids.
fn list_frame(ui: &mut Ui, reveal: Option<usize>) -> (u64, Vec<u64>) {
    let mut ids = (0, Vec::new());
    frame(ui, &UiInput::default(), |ui| {
        let list = ui.container(
            "list",
            Style {
                direction: Direction::Column,
                scroll: true,
                ..Style::default().size(Size::Fixed(200.0), Size::Fixed(100.0))
            },
            |ui| {
                for i in 0..20 {
                    ui.push_id(i);
                    let row = ui.container(
                        "row",
                        Style::default().size(Size::Fixed(200.0), Size::Fixed(20.0)),
                        |_| {},
                    );
                    if reveal == Some(i as usize) {
                        ui.reveal(row);
                    }
                    ids.1.push(row);
                    ui.pop_id();
                }
            },
        );
        ids.0 = list;
    });
    ids
}

#[test]
fn reveal_scrolls_a_row_into_view_and_no_further() {
    let mut ui = Ui::new(Theme::default());
    let inside = |ui: &Ui, list: u64, row: u64| {
        let (l, r) = (ui.rect_of(list).unwrap(), ui.rect_of(row).unwrap());
        r.pos[1] >= l.pos[1] - 0.01 && r.pos[1] + r.size[1] <= l.pos[1] + l.size[1] + 0.01
    };
    let (list, rows) = list_frame(&mut ui, None);
    assert!(!inside(&ui, list, rows[15]));
    // Shown from the next frame on, at the bottom edge: the least scroll that shows it.
    list_frame(&mut ui, Some(15));
    let (list, rows) = list_frame(&mut ui, None);
    assert!(inside(&ui, list, rows[15]));
    assert_eq!(ui.scroll_offset(list), 220.0);
    // A row already in view moves nothing; one above scrolls back to its top.
    list_frame(&mut ui, Some(13));
    list_frame(&mut ui, None);
    assert_eq!(ui.scroll_offset(list), 220.0);
    list_frame(&mut ui, Some(2));
    let (list, rows) = list_frame(&mut ui, None);
    assert!(inside(&ui, list, rows[2]));
    assert_eq!(ui.scroll_offset(list), 40.0);
}

#[test]
fn hover_regions_see_the_mouse_over_any_child() {
    let mut ui = Ui::new(Theme::default());
    // The row lies under the field (10 px padding, then the field's 20 px): over its
    // button, which takes the hover, the row still has the mouse.
    look_frame(&mut ui, &at(15.0, 40.0), "Door", false);
    let (_, in_row) = look_frame(&mut ui, &at(15.0, 40.0), "Door", false);
    assert!(in_row);
    let (_, in_row) = look_frame(&mut ui, &at(15.0, 20.0), "Door", false);
    assert!(!in_row, "over the field, not the row");
}

#[test]
fn a_nodes_image_is_drawn_over_its_fill() {
    let mut ui = Ui::new(Theme::default());
    let image = UiImage {
        texture: 7,
        uv: [0.25, 0.0, 0.5, 0.25],
    };
    ui.begin_frame(&UiInput::default(), [200.0, 100.0]);
    ui.container(
        "tile",
        Style {
            size: [Size::Fixed(64.0), Size::Fixed(48.0)],
            background: Some([0.1, 0.1, 0.1, 1.0]),
            image: Some(image),
            ..Style::default()
        },
        |_| {},
    );
    let mut measure = MonospaceMeasure;
    ui.end_frame(&mut measure);
    let frame = ui.last_frame();
    let kinds: Vec<&str> = frame
        .commands
        .iter()
        .map(|c| match c {
            DrawCmd::Rect { .. } => "rect",
            DrawCmd::Image { .. } => "image",
            DrawCmd::Text { .. } => "text",
            DrawCmd::Clip(_) => "clip",
        })
        .collect();
    let at = kinds.iter().position(|&k| k == "image").expect("drawn");
    assert_eq!(kinds[at - 1], "rect", "over the fill: {kinds:?}");
    let DrawCmd::Image { rect, image: drawn } = frame.commands[at] else {
        unreachable!()
    };
    assert_eq!(drawn, image);
    assert_eq!(rect.size, [64.0, 48.0]);
}
