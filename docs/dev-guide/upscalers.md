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
