//! The conversion pass (ADR 0100) against its CPU reference, under the validation layer
//! with synchronization validation, on the GPU `ENGINE_ADAPTER` picks:
//!
//! - synthetic planes of each chroma layout, bit depth, range, matrix and siting,
//!   uploaded as the software decoder's pictures are, at the texture's size and scaled;
//! - the GPU decoder's pictures, read where they were decoded, against rav1d's planes
//!   (bit-exact with them, `tests/gpu.rs`) converted on the CPU.
//!
//! Every texel may differ from the reference by one in a channel at most (the GPU's
//! arithmetic order); without a GPU the tests print why and pass, unless
//! `ENGINE_REQUIRE_GPU=1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::undocumented_unsafe_blocks
)]

mod common;

use common::{Bed, codes_of, compare, gpu, reference};
use engine_video::av1::sequence::ColorConfig;
use engine_video::av1::{Parser, TemporalUnit};
use engine_video::convert::{Conversion, DecoderPlanes, PlaneUpload, Planes, VideoTexture};
use engine_video::gpu::{GpuDecoder, GpuDecoderDesc, GpuError};
use engine_video::software::{Settings, SoftwareDecoder};
use engine_video::webm::{TrackKind, Webm};

/// A synthetic picture.
struct Case {
    name: &'static str,
    depth: u8,
    /// Chroma subsampled across and down; `None`: luma only.
    chroma: Option<[bool; 2]>,
    matrix: u8,
    full: bool,
    siting: u8,
    size: [u32; 2],
    target: [u32; 2],
}

/// Rows of samples with padding past each (a stride longer than the row).
fn rows(values: &[u16], [w, h]: [u32; 2], depth: u8) -> (Vec<u8>, usize) {
    let sample = if depth > 8 { 2 } else { 1 };
    let stride = w as usize * sample + 7;
    let mut bytes = vec![0xA5; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = values[y * w as usize + x];
            if sample == 2 {
                bytes[y * stride + 2 * x..][..2].copy_from_slice(&v.to_le_bytes());
            } else {
                bytes[y * stride + x] = v as u8;
            }
        }
    }
    (bytes, stride)
}

/// Smooth gradients with noise over a plane, every code value of `depth` bits reachable.
fn plane(seed: u64, [w, h]: [u32; 2], depth: u8) -> Vec<u16> {
    let max = (1u32 << depth) - 1;
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..w * h)
        .map(|i| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let (x, y) = (i % w, i / w);
            let smooth = (x * max / w.max(2) + y * max / h.max(2)) / 2;
            let noise = (state >> 40) as u32 % (max / 4 + 1);
            (smooth + noise).min(max) as u16
        })
        .collect()
}

#[test]
fn synthetic_planes_match_the_cpu() {
    let Some((instance, device)) = gpu(false) else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let cases = [
        Case {
            name: "8-bit 4:2:0, BT.709 studio range, chroma left",
            depth: 8,
            chroma: Some([true, true]),
            matrix: 1,
            full: false,
            siting: 0,
            size: [64, 48],
            target: [64, 48],
        },
        Case {
            name: "10-bit 4:2:0, BT.2020 full range, colocated, odd size",
            depth: 10,
            chroma: Some([true, true]),
            matrix: 9,
            full: true,
            siting: 2,
            size: [61, 37],
            target: [61, 37],
        },
        Case {
            name: "8-bit 4:4:4 GBR",
            depth: 8,
            chroma: Some([false, false]),
            matrix: 0,
            full: true,
            siting: 0,
            size: [40, 30],
            target: [40, 30],
        },
        Case {
            name: "8-bit 4:2:2, BT.601 studio range",
            depth: 8,
            chroma: Some([true, false]),
            matrix: 6,
            full: false,
            siting: 0,
            size: [48, 32],
            target: [48, 32],
        },
        Case {
            name: "8-bit luma only",
            depth: 8,
            chroma: None,
            matrix: 1,
            full: false,
            siting: 0,
            size: [32, 24],
            target: [32, 24],
        },
        Case {
            name: "12-bit 4:2:0 scaled down",
            depth: 12,
            chroma: Some([true, true]),
            matrix: 1,
            full: false,
            siting: 1,
            size: [80, 60],
            target: [64, 40],
        },
        Case {
            name: "8-bit 4:2:0 scaled up",
            depth: 8,
            chroma: Some([true, true]),
            matrix: 2,
            full: false,
            siting: 0,
            size: [40, 30],
            target: [100, 70],
        },
    ];
    let mut bed = Bed::new(device);
    for (index, case) in cases.iter().enumerate() {
        let color = ColorConfig {
            bit_depth: case.depth,
            high_bitdepth: case.depth > 8,
            twelve_bit: case.depth == 12,
            mono_chrome: case.chroma.is_none(),
            color_description_present_flag: true,
            color_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: case.matrix,
            color_range: case.full,
            subsampling_x: case.chroma.is_none_or(|c| c[0]),
            subsampling_y: case.chroma.is_none_or(|c| c[1]),
            chroma_sample_position: case.siting,
            separate_uv_delta_q: false,
        };
        let [w, h] = case.size;
        let chroma_size = case.chroma.map(|[x, y]| {
            [
                if x { w.div_ceil(2) } else { w },
                if y { h.div_ceil(2) } else { h },
            ]
        });
        let seed = index as u64 * 3;
        let luma = rows(&plane(seed, case.size, case.depth), case.size, case.depth);
        let chroma = chroma_size.map(|size| {
            (
                rows(&plane(seed + 1, size, case.depth), size, case.depth),
                rows(&plane(seed + 2, size, case.depth), size, case.depth),
                size,
            )
        });
        let planes = Planes {
            size: case.size,
            bit_depth: case.depth,
            luma: (&luma.0, luma.1),
            chroma: chroma
                .as_ref()
                .map(|(cb, cr, size)| ([(&cb.0[..], cb.1), (&cr.0[..], cr.1)], *size)),
        };
        let conversion = Conversion::new(&color, case.target[0], case.target[1]);
        let mut upload = PlaneUpload::new(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            &color,
            case.size,
            1,
        )
        .unwrap();
        let mut texture = VideoTexture::new(
            &bed.device,
            &mut bed.allocator,
            &bed.bindless,
            case.target[0],
            case.target[1],
        )
        .unwrap();
        let source = upload.stage(0, &planes).unwrap();
        let ours = bed.convert(&mut texture, &source, &conversion, None);
        let theirs = reference(&codes_of(&planes), &conversion, case.target);
        match compare(&ours, &theirs, case.target[0]) {
            Ok(off) => println!("{}: {} texels, {off} off by one", case.name, ours.len()),
            Err(e) => panic!("{}: {e}", case.name),
        }
        unsafe {
            texture.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0);
            upload.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0);
        }
    }
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}

#[test]
fn decoded_pictures_match_the_cpu() {
    let Some((instance, device)) = gpu(true) else {
        return;
    };
    if device.video_decode.is_none() {
        println!(
            "{}: no AV1 decode queue, nothing to convert",
            device.adapter().name()
        );
        return;
    }
    let mut bed = Bed::new(device);
    let clips = [
        // 8 bits, coded 184 lines high under a 180-line picture: scaled to 180.
        "svt_8bit_320x180.webm",
        // 10 bits: P010's words.
        "aom_10bit_tiles_320x180.webm",
        "svt_grain_tiles_640x360.webm",
    ];
    let mut converted = 0;
    for clip in clips {
        let path = format!("{}/tests/clips/{clip}", env!("CARGO_MANIFEST_DIR"));
        let data = std::fs::read(&path).unwrap();
        let webm = Webm::read(&data).unwrap();
        let video = webm.track(TrackKind::Video).unwrap();
        let mut parser = Parser::with_codec_private(&video.private).unwrap();
        let mut unit = TemporalUnit::default();
        let mut software = SoftwareDecoder::new(&Settings {
            threads: 1,
            max_frame_delay: 1,
            apply_grain: true,
            output_invisible_frames: false,
        })
        .unwrap();
        let mut decoder: Option<(GpuDecoder, DecoderPlanes)> = None;
        let mut texture: Option<VideoTexture> = None;
        let mut pictures = Vec::new();
        let mut count = 0;
        for block in webm.blocks().map(Result::unwrap) {
            if block.track != video.number {
                continue;
            }
            parser.parse(block.data(), &mut unit).unwrap();
            let sequence = *parser.sequence().unwrap();
            if decoder.is_none() {
                let gpu = match GpuDecoder::new(
                    &bed.device,
                    &mut bed.allocator,
                    &sequence,
                    parser.sequence_serial(),
                    &GpuDecoderDesc::default(),
                ) {
                    Ok(d) => d,
                    Err(GpuError::Unsupported(why)) => {
                        println!("{clip}: not on this GPU ({why})");
                        break;
                    }
                    Err(e) => panic!("{clip}: {e}"),
                };
                let planes = DecoderPlanes::new(&gpu, &bed.bindless).unwrap();
                decoder = Some((gpu, planes));
            }
            let (gpu, planes) = decoder.as_mut().unwrap();
            pictures.clear();
            gpu.decode(
                &bed.device,
                &mut bed.allocator,
                block.data(),
                &unit,
                &sequence,
                block.time_ns,
                &mut pictures,
            )
            .unwrap();
            let mut theirs = Vec::new();
            software
                .decode(block.data(), block.time_ns, |p| theirs.push(p))
                .unwrap();
            assert_eq!(pictures.len(), theirs.len(), "{clip}: pictures");
            for (ours, theirs) in pictures.iter().zip(&theirs) {
                let target = [ours.render_width, ours.render_height];
                let texture = texture.get_or_insert_with(|| {
                    VideoTexture::new(
                        &bed.device,
                        &mut bed.allocator,
                        &bed.bindless,
                        target[0],
                        target[1],
                    )
                    .unwrap()
                });
                let conversion = Conversion::new(&sequence.color, target[0], target[1]);
                let source = planes.source(ours).unwrap();
                let semaphore = gpu.semaphore();
                let texels =
                    bed.convert(texture, &source, &conversion, Some((semaphore, ours.ready)));
                let reference = reference(
                    &codes_of(&Planes::from(theirs)),
                    &conversion,
                    texture.size(),
                );
                if let Err(e) = compare(&texels, &reference, texture.size()[0]) {
                    panic!("{clip}, picture {count}: {e}");
                }
                gpu.release(ours);
                count += 1;
            }
        }
        if let Some(texture) = texture {
            unsafe { texture.destroy(&bed.device, &mut bed.allocator, &bed.bindless, 0) };
        }
        if let Some((gpu, planes)) = decoder {
            planes.release(&bed.bindless, 0);
            unsafe { gpu.destroy(&bed.device, &mut bed.allocator) };
        }
        if count > 0 {
            println!("{clip}: {count} pictures converted as on the CPU");
            converted += 1;
        }
    }
    assert!(converted > 0, "no clip converted");
    assert_eq!(bed.finish(&instance), 0, "validation errors");
}
