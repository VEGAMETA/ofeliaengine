# Upscalers: which files they need

The engine has three upscalers (how to use them in code: [rendering.md
§7](rendering.md#7-temporal-antialiasing-and-upsampling-taa--taau)):

| Upscaler | GPUs | Files to supply |
|---|---|---|
| TAAU (the engine's own) | every GPU | none |
| AMD FSR 3.1 | every GPU | none: built into the engine from AMD's MIT source |
| NVIDIA DLSS Super Resolution / DLAA | NVIDIA RTX | NVIDIA's DLLs, below |

The engine does not ship or download NVIDIA's DLLs (CLAUDE.md §0; team decision, ADR
0063). When they are present, load Streamline at startup (`Streamline::load` before
creating the instance): it costs about 1.1 s once, and DLSS can then be switched on
without a restart. A game either ships
them itself, under NVIDIA's license, or lets players add them. Without them the DLSS
option simply is not there: nothing fails.

## Which one: measured on both reference GPUs (2026-10-11)

Sponza with everything on (`examples/showcase.rs`, `examples/aa_bench.rs`), GPU time of
the whole frame at 1920 × 1080 unless noted.

| | RTX 4060 Ti, 1080p | RTX 4060 Ti, 4K | two-CU Radeon iGPU, 1080p |
|---|---:|---:|---:|
| TAA (the engine's, 100 %) | 2.00 ms | 7.73 ms | 77 ms |
| TAAU 67 % | 1.44 ms¹ | 4.18 ms | 40.6 ms |
| FSR 3.1 Native AA | 2.69 ms | 10.64 ms | 101 ms |
| FSR 3.1 Quality (67 %) | 1.58 ms | 5.93 ms | 48.4 ms |
| DLAA | 2.87 ms | 10.74 ms | — |
| DLSS Quality (67 %) | 2.01 ms | 7.10 ms | — |

¹ `aa_bench`'s scene (no GI); the other cells are the day scene.

The picture, against a reference rendered at 4 × 4 the size (`aa_bench`, RTX, 1080p):

| | Still, FLIP | Moving, FLIP | Flicker, % of pixels | Flicker on the plain scene |
|---|---:|---:|---:|---:|
| TAA | 0.0266 | 0.0321 | 0.26 | 0.18 |
| TAAU 67 % | 0.0328 | 0.0385 | 0.27 | 0.15 |
| FSR 3.1 Native AA | 0.0269 | 0.0241 | 0.60 | 1.14 |
| FSR 3.1 Quality (67 %) | 0.0335 | 0.0301 | 0.50 | 0.72 |
| DLAA | 0.0190 | 0.0242 | 0.50 | 0.79 |
| DLSS Quality (67 %) | 0.0222 | 0.0263 | 0.50 | 0.74 |

Lower is better in every column.

- **FSR gives the same picture on both vendors.** At 640 × 360 its FLIP and flicker on
  the Radeon and the RTX agree within 0.001 and 0.03 %.
- **On the RTX, FSR costs between TAAU and DLSS:** at 4K, Quality 5.93 ms against 4.18
  and 7.10. It is the sharpest in motion after DLAA, and it flickers most on still
  edges and textures (its sharpening): 1.14 % of pixels against TAA's 0.18 %.
- **On a small integrated Radeon, FSR costs 6–8 ms more than TAAU** at the same render
  size: its passes take about 2.7 times TAAU's. TAAU stays the default there (`RenderDefaults`).
- **DLSS gives the best still picture.** In Performance mode NVIDIA picks network M by
  default, which costs more than Balanced's K (4.66 against 3.31 ms at 4K): Balanced is
  then faster than Performance.

## DLSS: the files

From NVIDIA's Streamline SDK release, `streamline-sdk-v2.14.1.zip` on
github.com/NVIDIAGameWorks/Streamline/releases, folder `bin/x64` (not
`bin/x64/development`: those DLLs are unsigned and the engine refuses them):

| File | Needed | What it is |
|---|---|---|
| `sl.interposer.dll` | yes | Streamline's entry point |
| `sl.common.dll` | yes | Streamline's common plugin |
| `sl.dlss.dll` | yes | Streamline's DLSS plugin |
| `nvngx_dlss.dll` | yes | DLSS itself (about 60 MB) |
| `NvLowLatencyVk.dll` | no | Reflex markers for Vulkan; without it Streamline logs one error at startup and DLSS works |

Copy only these: every other `sl.*.dll` in the folder is loaded, checked and ignored, which
adds about 0.5 s to startup.

## Where they go

The engine looks, in order:

1. in the folder the game passes (`StreamlineDesc::plugin_dir`);
2. in `ENGINE_STREAMLINE_DIR` (development and tests);
3. in `plugins/nvidia/` next to the game's executable;
4. next to the game's executable.

The first folder with `sl.interposer.dll` is used; if it lacks one of the other needed
files, DLSS is unavailable and the error names the missing files.

## How to check

`cargo run -p engine_dlss --example dlss_probe` prints:

- where the DLLs were found, or where they were looked for;
- whether NVIDIA signed them (the engine checks `sl.interposer.dll` with Windows'
  `WinVerifyTrust` and that the signer is NVIDIA Corporation; Streamline checks the
  others);
- which GPUs run DLSS, and why the others do not (for example
  `eErrorAdapterNotSupported` on an AMD GPU, `eErrorDriverOutOfDate` with an old driver);
- the versions (`sl.dlss` 2.14.1, `nvngx_dlss` 310.9.1) and each mode's render sizes.

A player's copy of a game shows the same through the game's own settings: DLSS appears
only when all of that succeeded.

## If your game ships the DLLs

You then accept NVIDIA's license for them (the NVIDIA RTX SDKs License,
`nvngx_dlss.license.txt` in the SDK's `bin/x64`). Among its terms:

- notify NVIDIA before the commercial release (developer.nvidia.com/sw-notification);
- show NVIDIA's marks on the splash screen, in the about box and in the credits, as
  NVIDIA requires and approves;
- DLSS may be used only on systems with NVIDIA GPUs;
- distribute the DLLs only as part of the game.

Read the license itself; this list is not legal advice. The engine carries no NVIDIA mark
and turns off Streamline's over-the-air updates, so the DLLs you ship are the ones that
run.
