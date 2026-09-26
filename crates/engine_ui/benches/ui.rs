//! UI frame cost: building, laying out, hit testing and emitting draw commands for an
//! inspector-like tree. Median of repeated frames, single thread. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_ui --bench ui
//! ```

#![allow(clippy::print_stdout)]

use engine_ui::*;
use std::time::Instant;

const ROWS: usize = 2_000;

fn build(ui: &mut Ui, keys: &[String]) {
    ui.row("root", Style::fill(), |ui| {
        ui.panel(
            "side",
            Style::default()
                .size(Size::Fixed(300.0), Size::Grow(1.0))
                .padding(4.0),
            |ui| {
                ui.scroll_column("list", |ui| {
                    for k in keys {
                        ui.row(k, Style::default().gap(4.0).padding(2.0), |ui| {
                            ui.label("Transform.translation");
                            ui.spacer("s");
                            ui.button("Edit");
                            ui.label("(1, 2, 3)");
                        });
                    }
                });
            },
        );
        ui.column("main", Style::fill().padding(8.0), |ui| {
            ui.paragraph(
                "help",
                "Select an entity to edit its components. Everything is undoable.",
            );
        });
    });
}

fn main() {
    let keys: Vec<String> = (0..ROWS).map(|i| format!("row{i}")).collect();
    let mut ui = Ui::new(Theme::default());
    let mut measure = MonospaceMeasure;
    let input = UiInput {
        mouse: Some([50.0, 50.0]),
        ..UiInput::default()
    };
    let mut times = Vec::new();
    let mut builds = Vec::new();
    let mut commands = 0;
    for i in 0..220 {
        let t = Instant::now();
        ui.begin_frame(&input, [1920.0, 1080.0]);
        build(&mut ui, &keys);
        let built = t.elapsed().as_secs_f64() * 1e6;
        commands = ui.end_frame(&mut measure).commands.len();
        if i >= 20 {
            times.push(t.elapsed().as_secs_f64() * 1e6);
            builds.push(built);
        }
    }
    times.sort_by(f64::total_cmp);
    builds.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    let build_median = builds[builds.len() / 2];
    let nodes = ui.node_count();
    println!("### engine_ui frame ({nodes} nodes, {commands} draw commands)\n");
    println!("| stage | µs | per node |");
    println!("|---|---:|---:|");
    println!(
        "| build + layout + hit test + draw list | {median:.0} | {:.1} ns |",
        median * 1e3 / nodes as f64
    );
    println!(
        "| of which build (ids, nodes, text) | {build_median:.0} | {:.1} ns |",
        build_median * 1e3 / nodes as f64
    );
    println!();
}
