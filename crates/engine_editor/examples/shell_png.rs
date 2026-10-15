//! Renders the editor shell (the team's Godot layout, `docs/research/ux/editor-layout.md`)
//! headless into a PNG: the UI through `engine_ui` + `engine_render2d` + `engine_text`, with a
//! 3D scene from the forward renderer in the viewport (an `engine_render` viewport drawn
//! under the UI). Works on any Vulkan 1.3 device, including lavapipe.
//!
//! `cargo run -p engine_editor --release --example shell_png -- [out.png] [--playing]`

#![allow(missing_docs, clippy::print_stdout, clippy::unwrap_used)]

use engine_core::math::{Mat4, Quat, Vec2, Vec3, look_at};
use engine_editor::icons;
use engine_editor::paint::{UiText, paint};
use engine_editor::shell::{
    Field, FileItem, FileKind, Palette, Section, ShellModel, TreeItem, build,
};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, ViewFeatures,
};
use engine_render::graph::{Access, BufferState, ImportedBuffer, RenderGraph, TextureDesc};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube, perspective};
use engine_render::png::{PngFormat, encode_png};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_render::viewport::{ViewportDesc, ViewportSize, Viewports};
use engine_render2d::{Canvas, GlyphTexture, Order, Quad, QuadMode, Renderer2d, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};
use engine_ui::{Theme, Ui, UiInput};

const WIDTH: u32 = 1600;
const HEIGHT: u32 = 900;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;

fn tree_item<'a>(
    depth: u16,
    icon: &'a str,
    name: &'a str,
    kind: &'a str,
    expanded: Option<bool>,
    selected: bool,
) -> TreeItem<'a> {
    TreeItem {
        depth,
        icon,
        name,
        kind,
        expanded,
        selected,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let playing = args.iter().any(|a| a == "--playing");
    let out_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| String::from("editor-shell.png"));

    let instance = Instance::new(&InstanceDesc {
        app_name: String::from("editor shell"),
        validation: cfg!(debug_assertions),
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    })
    .unwrap();
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    let device = Device::new(&instance, &adapter).unwrap();
    let adapter_name = device.adapter().name().to_owned();
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();

    // The 3D scene in the viewport.
    let forward = ForwardRenderer::new(
        &device,
        &bindless,
        FORMAT,
        vk::Format::D32_SFLOAT,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut frame_number = 0u64;
    let mut textures = TexturePool::new(&device, 64 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let ground = materials.add(MaterialDesc::solid([0.42, 0.44, 0.40], 0.0, 0.9));
    let crate_material = materials.add(MaterialDesc::solid([0.62, 0.44, 0.26], 0.0, 0.7));
    let pillar = materials.add(MaterialDesc::solid([0.55, 0.57, 0.62], 0.0, 0.5));
    let figure = materials.add(MaterialDesc::solid([0.2, 0.45, 0.9], 0.3, 0.35));
    let mut draws = vec![DrawItem {
        mesh,
        material: ground,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(24.0, 0.2, 24.0),
            Quat::IDENTITY,
            Vec3::new(0.0, -0.1, 0.0),
        ),
    }];
    for i in 0..9 {
        let (x, z) = ((i % 3) as f32 * 1.6 - 5.0, (i / 3) as f32 * 1.6 - 3.0);
        draws.push(DrawItem {
            mesh,
            material: crate_material,
            world_from_object: Mat4::from_scale_rotation_translation(
                Vec3::splat(1.1),
                Quat::from_rotation_y(i as f32 * 0.4),
                Vec3::new(x, 0.55 + (i % 2) as f32 * 1.1, z),
            ),
        });
    }
    draws.push(DrawItem {
        mesh,
        material: pillar,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 6.0, 2.0),
            Quat::IDENTITY,
            Vec3::new(4.0, 3.0, -4.0),
        ),
    });
    draws.push(DrawItem {
        mesh,
        material: figure,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(0.7, 1.8, 0.7),
            Quat::from_rotation_y(0.8),
            Vec3::new(0.5, 0.9, 2.0),
        ),
    });
    let light = DirectionalLight {
        direction: Vec3::new(-0.5, -1.0, -0.35),
        color: Vec3::splat(2.6),
        ambient: Vec3::new(0.12, 0.13, 0.16),
    };
    let mut viewports = Viewports::new(&device, 1);

    // The UI.
    let palette = Palette::default();
    let mut ui = Ui::new(Theme {
        text: palette.text,
        panel: palette.dock,
        ..Theme::default()
    });
    let mut text = UiText::new().unwrap();
    let mut glyphs = GlyphTexture::new(&device, &mut allocator, &bindless, &text.atlas).unwrap();
    let mut renderer2d = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        FORMAT,
        QuadMode::for_device(&device),
    )
    .unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 16 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);

    let tree = [
        tree_item(0, icons::NODE_3D, "World", "Node3D", Some(true), false),
        tree_item(1, icons::SUN, "Sun", "DirectionalLight", None, false),
        tree_item(
            1,
            icons::ENVIRONMENT,
            "Environment",
            "WorldEnvironment",
            None,
            false,
        ),
        tree_item(1, icons::NODE_3D, "Level", "Node3D", Some(true), false),
        tree_item(
            2,
            icons::MESH_INSTANCE,
            "Ground",
            "MeshInstance",
            None,
            false,
        ),
        tree_item(2, icons::NODE_3D, "Crates", "Node3D", Some(false), false),
        tree_item(
            2,
            icons::MESH_INSTANCE,
            "Tower",
            "MeshInstance",
            None,
            false,
        ),
        tree_item(
            1,
            icons::CHARACTER,
            "Player",
            "CharacterBody",
            Some(true),
            true,
        ),
        tree_item(2, icons::CAMERA, "Camera", "Camera3D", None, false),
        tree_item(
            2,
            icons::MESH_INSTANCE,
            "Model",
            "MeshInstance",
            None,
            false,
        ),
        tree_item(
            2,
            icons::COLLISION,
            "Collider",
            "CollisionShape",
            None,
            false,
        ),
        tree_item(1, icons::NODE_3D, "Enemies", "Node3D", Some(false), false),
        tree_item(
            1,
            icons::CONTROL,
            "Интерфейс",
            "Control",
            Some(false),
            false,
        ),
    ];
    let transform = [
        Field {
            name: "Position",
            value: "0.50   0.90   2.00",
        },
        Field {
            name: "Rotation",
            value: "0°   45°   0°",
        },
        Field {
            name: "Scale",
            value: "1   1   1",
        },
    ];
    let body = [
        Field {
            name: "Speed",
            value: "6.5 m/s",
        },
        Field {
            name: "Jump height",
            value: "1.4 m",
        },
        Field {
            name: "Mass",
            value: "72 kg",
        },
        Field {
            name: "Gravity",
            value: "World",
        },
    ];
    let script = [
        Field {
            name: "Source",
            value: "player.luau",
        },
        Field {
            name: "Hot reload",
            value: "On",
        },
    ];
    let physics = [
        Field {
            name: "Layer",
            value: "Characters",
        },
        Field {
            name: "Collides with",
            value: "World, Enemies",
        },
    ];
    let inspector = [
        Section {
            title: "Transform",
            fields: &transform,
        },
        Section {
            title: "Character Body",
            fields: &body,
        },
        Section {
            title: "Script",
            fields: &script,
        },
        Section {
            title: "Physics",
            fields: &physics,
        },
    ];
    let folders = [
        tree_item(0, icons::FOLDER_OPEN, "res://", "", Some(true), false),
        tree_item(1, icons::FOLDER_OPEN, "levels", "", None, true),
        tree_item(1, icons::FOLDER, "models", "", Some(false), false),
        tree_item(1, icons::FOLDER, "scripts", "", None, false),
        tree_item(1, icons::FOLDER, "textures", "", None, false),
        tree_item(1, icons::FOLDER, "audio", "", None, false),
    ];
    let file = |name, kind| FileItem { name, kind };
    let files = [
        file("main.scene", FileKind::Scene),
        file("level_01.scene", FileKind::Scene),
        file("level_02.scene", FileKind::Scene),
        file("меню.scene", FileKind::Scene),
        file("player.luau", FileKind::Script),
        file("enemy.luau", FileKind::Script),
        file("crate.gltf", FileKind::Mesh),
        file("tower.gltf", FileKind::Mesh),
        file("stone.ktx2", FileKind::Texture),
        file("wind.ogg", FileKind::Audio),
    ];
    let tools = [
        (icons::TOOL_SELECT, "Select"),
        (icons::TOOL_MOVE, "Move"),
        (icons::TOOL_ROTATE, "Rotate"),
        (icons::TOOL_SCALE, "Scale"),
        (icons::SNAP, "Snap"),
    ]
    .map(|(icon, name)| format!("{icon} {name}"));
    let tools = tools.each_ref().map(String::as_str);
    let status = format!("engine 0.0.0  ·  Vulkan  ·  {adapter_name}");
    let model = ShellModel {
        scenes: &["main.scene", "level_01.scene*", "меню.scene"],
        active_scene: 1,
        tools: &tools,
        active_tool: 1,
        tree: &tree,
        selection: "Player  ·  CharacterBody",
        inspector: &inspector,
        path: "res://levels",
        folders: &folders,
        files: &files,
        bottom_tabs: &[
            "Output",
            "Debugger",
            "Audio",
            "Animation",
            "Shader",
            "Profiler",
        ],
        active_bottom: None,
        playing,
        status: &status,
    };

    // Lay the UI out: the viewport's rectangle sizes the 3D view.
    ui.begin_frame(
        &UiInput {
            mouse: None,
            ..UiInput::default()
        },
        [WIDTH as f32, HEIGHT as f32],
    );
    let response = build(&mut ui, &model, &palette);
    let started = std::time::Instant::now();
    let commands = ui.end_frame(&mut text).commands.len();
    let layout_time = started.elapsed();
    let view_rect = ui.rect_of(response.viewport).unwrap();
    let (vw, vh) = (
        view_rect.size[0].round().max(1.0) as u32,
        view_rect.size[1].round().max(1.0) as u32,
    );
    let view = viewports.create(ViewportDesc {
        name: String::from("scene viewport"),
        size: ViewportSize::Fixed {
            width: vw,
            height: vh,
        },
        format: FORMAT,
        tonemap: false,
        placement: None,
        clear: Some([0.16, 0.18, 0.22, 1.0]),
        ..ViewportDesc::default()
    });
    let eye = Vec3::new(9.0, 7.0, 11.0);
    let camera = Camera {
        clip_from_world: perspective(50f32.to_radians(), vw as f32 / vh as f32, 0.1, 200.0)
            * look_at(eye, Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        position: eye,
    };

    let readback_size = u64::from(WIDTH * HEIGHT * 4);
    let (readback, readback_mem) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(readback_size)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    // SAFETY: plain object creation.
    let (pool, cmd) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (pool, cmd)
    };
    // SAFETY: one-shot recording.
    unsafe {
        d.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
        .unwrap();
    }
    ring.begin_slot(0);
    let mut canvas = Canvas::new();
    let stats;
    {
        // SAFETY: nothing is in flight.
        let mut fg = unsafe { graph.begin_slot(0, cmd) };
        frame_number += 1;
        let geo = geometry
            .begin_frame(&mut fg, &device, &mut allocator, frame_number)
            .unwrap();
        textures
            .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
            .unwrap();
        let mats = materials
            .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
            .unwrap();
        {
            let (forward, bindless, draws, ring) = (&forward, &bindless, &draws, &mut ring);
            viewports
                .render(
                    &mut fg,
                    &device,
                    &mut allocator,
                    bindless,
                    (WIDTH, HEIGHT),
                    1,
                    |fg, vf| {
                        let depth = fg.create_texture(
                            "viewport depth",
                            TextureDesc::new_2d(vk::Format::D32_SFLOAT, vf.width, vf.height),
                        );
                        let clear = match vf.load {
                            engine_render::graph::LoadOp::Clear(c) => Some(c),
                            _ => None,
                        };
                        let view = ForwardView {
                            camera,
                            light,
                            color: vf.color,
                            depth,
                            clear,
                            occlusion: None,
                            temporal: None,
                            shadow_cache: None,
                            lights: &[],
                            decals: &[],
                            environment: None,
                            reflections: Default::default(),
                            gi: None,
                            features: ViewFeatures::ALL,
                        };
                        forward.add_passes(fg, ring, bindless, &geo, &mats, draws, &view);
                    },
                )
                .unwrap();
        }
        // The 3D view under the UI, then the UI.
        let index = viewports.texture_index(view).unwrap();
        let size = Vec2::new(view_rect.size[0], view_rect.size[1]);
        let center = Vec2::new(view_rect.pos[0], view_rect.pos[1]) + size * 0.5;
        canvas.draw(&Quad::sprite(index, center, size).order(Order::layer(-1)));
        let frame = ui.last_frame();
        stats = paint(&frame, &mut text, &glyphs, &mut canvas, 1.0);
        glyphs.sync(&mut fg, &mut ring, &mut text.atlas).unwrap();
        canvas.prepare();
        let target = fg.create_texture("window", TextureDesc::new_2d(FORMAT, WIDTH, HEIGHT));
        renderer2d
            .add_pass(
                &mut fg,
                &mut ring,
                &bindless,
                &canvas,
                &View2d::pixels(WIDTH, HEIGHT),
                target,
                Some(palette.window),
            )
            .unwrap();
        let out = fg.import_buffer(
            "readback",
            ImportedBuffer {
                buffer: readback,
                size: readback_size,
                address: 0,
                initial: BufferState::idle(),
                final_state: Some(BufferState::host_read()),
            },
        );
        fg.add_pass("read back")
            .read(target, Access::TransferRead)
            .write(out, Access::TransferWrite)
            .execute(move |ctx| {
                let region = vk::BufferImageCopy {
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_extent: vk::Extent3D {
                        width: WIDTH,
                        height: HEIGHT,
                        depth: 1,
                    },
                    ..Default::default()
                };
                // SAFETY: recording; layouts arranged by the graph.
                unsafe {
                    ctx.device.cmd_copy_image_to_buffer(
                        ctx.cmd,
                        ctx.image(target),
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        ctx.buffer(out),
                        &[region],
                    );
                }
            });
        fg.execute_with_bindless(&mut allocator, Some(&bindless))
            .unwrap();
    }
    // SAFETY: as above.
    unsafe {
        d.end_command_buffer(cmd).unwrap();
        let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        d.queue_submit2(
            device.graphics.raw,
            &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
            vk::Fence::null(),
        )
        .unwrap();
        device.wait_idle().unwrap();
    }

    let ptr = readback_mem.mapped_ptr().unwrap();
    // SAFETY: the GPU is idle; the mapping covers the image.
    let rgba = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), readback_size as usize) };
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    let png = encode_png(WIDTH, HEIGHT, PngFormat::Rgb, &rgb).unwrap();
    std::fs::write(&out_path, &png).unwrap();
    println!(
        "{out_path}: {WIDTH}×{HEIGHT}, {} KiB; {} UI nodes, {commands} draw commands (first-frame layout {:.0} µs), {} rect quads, {} glyphs, {} clips; viewport {vw}×{vh}; {adapter_name}",
        png.len() / 1024,
        ui.node_count(),
        layout_time.as_secs_f64() * 1e6,
        stats.rects,
        stats.glyphs,
        stats.clips,
    );

    // Steady state: the editor builds, lays out and paints the UI every frame. The first
    // frame above also shapes every string for the first time.
    let input = UiInput {
        mouse: None,
        ..UiInput::default()
    };
    let mut scratch = Canvas::new();
    let (mut ui_us, mut paint_us) = (Vec::with_capacity(200), Vec::with_capacity(200));
    for _ in 0..200 {
        let t = std::time::Instant::now();
        ui.begin_frame(&input, [WIDTH as f32, HEIGHT as f32]);
        build(&mut ui, &model, &palette);
        ui.end_frame(&mut text);
        ui_us.push(t.elapsed().as_secs_f64() * 1e6);
        let t = std::time::Instant::now();
        scratch.clear();
        paint(&ui.last_frame(), &mut text, &glyphs, &mut scratch, 1.0);
        paint_us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p).round() as usize]
    };
    println!(
        "steady frame (200 runs, CPU): build + layout {:.0} µs median / {:.0} p99; paint {:.0} / {:.0}",
        pct(&mut ui_us, 0.5),
        pct(&mut ui_us, 0.99),
        pct(&mut paint_us, 0.5),
        pct(&mut paint_us, 0.99),
    );

    // SAFETY: the device is idle.
    unsafe {
        viewports.destroy_all(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        renderer2d.destroy(&mut allocator);
        glyphs
            .texture_mut()
            .destroy(&device, &mut allocator, &bindless, 0);
        allocator.destroy_buffer(readback, readback_mem);
        d.destroy_command_pool(pool, None);
    }
    drop(forward);
    let report = instance.validation_report();
    if report.errors > 0 {
        println!(
            "validation errors: {} {:#?}",
            report.errors, report.first_errors
        );
        std::process::exit(1);
    }
}
