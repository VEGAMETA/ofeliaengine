# PSO records: cooking only the variants a game draws

ADR 0092's last step: "Compile All Shaders" and the cook over the pairs (material type,
view features) a game draws, instead of every fragment entry point of `forward.slang`
for every type (128 since the bent-normal twins, ADR 0058). Checked 2026-10-14.

## Sources

| Source | Version | What we read |
|---|---|---|
| Unreal Engine, "Manually Creating Bundled PSO Caches" and "Optimizing Rendering with PSO Caches" (dev.epicgames.com) | UE 5.8 documentation | how PSOs are recorded, merged, cooked and warmed |
| Unity, "PSO tracing" (`GraphicsStateCollection`) | Unity 6.0–6.5 manual (experimental API) | the same, for a second engine |
| Godot, "Reducing stutter from shader (pipeline) compilations" | 4.4 and stable documentation | the alternative: no records, prediction at load and ubershaders |
| Our ADRs 0055 (pipeline cache), 0079 (pipelines on first use), 0092 (material types) | — | what we already have |

Products read through their documentation; no code (Unreal's and Unity's are not open,
Godot's approach differs).

## How each does it

- **Unreal (bundled PSO cache):**
  - Recording: playtests of a packaged build run with `-logPSO` and write
    `rec.pipelinecache` files under `Saved/CollectedPSOs`.
  - The cook writes stable shader keys (`.shk`). The `ShaderPipelineCacheTools expand`
    commandlet turns records plus keys into a stable cache (`.spc`), packaged with the game.
  - Startup: the cache compiles in batches (`r.ShaderPipelineCache.StartupMode`:
    paused, fast, background or precompile), filtered by game usage masks; a loading
    screen can wait on `NumPrecompilesRemaining`.
  - Misses: a PSO the cache lacks logs "Encountered a new graphics PSO". It goes into a
    user cache in `Saved`, merged on the next start.
  - Runtime PSO precaching (prediction from materials at load) complements it.
- **Unity 6 (PSO tracing):**
  - Recording: `GraphicsStateCollection.BeginTrace` / `EndTrace` in a development build on
    the target device, one collection per graphics API and platform.
  - The collection is saved (`SaveToFile`, or `SendToEditor`) as a `.graphicsstate` asset.
  - Startup: `WarmUp` / `WarmUpProgressively` before the content needs it. On APIs without
    PSO prewarming it falls back to `ShaderVariantCollection.WarmUp`.
- **Godot 4.4:**
  - No records: pipelines are precompiled when meshes load or nodes enter the scene, from
    what the renderer sees then.
  - Meanwhile ubershaders draw. Features turned on later (MSAA, VoxelGI) still compile
    mid-game.

## What we take

- **Records from playtests, as Unreal and Unity do:**
  - The renderer keeps every (type, entry point) pair frames asked for
    (`ForwardRenderer::material_type_entries_drawn`).
  - Tools merge that into a text record (`engine_assets::pso_record`, `*.pso.ron`): sorted,
    a type named by its module's file name and its own, so a team shares one record in
    version control.
  - `flight --record-pso`, and the golden harness for its shots (`tests/golden/pso.ron`).
- **The cook compiles only the record's entry points** for the types it names
  (`ofeliaengine cook --pso`, or `<scene>.pso.ron` next to the scene). A type the record
  does not name is cooked whole, as before.
- **Misses are loud, as Unreal's are:** a pack's type asked for an entry point the pack
  lacks logs a warning and is counted (`material_type_pack_misses`); the golden harness
  fails on any. Where Slang is there (the editor, development), the entry point compiles
  on first sight.
- **Degrade within the type:** when a type has no pipeline for a bent-normal twin (still
  compiling, or not in its pack), it draws its variant without bent normals, not its glTF
  fallback.

## What we do not take

- **A separate stable cache file of whole pipelines (Unreal's `.spc`):** our records name
  entry points. The pipelines' state comes from the engine's pipelines they derive from,
  and the driver's pipeline cache (ADR 0055) keeps the compiled pipelines.
- **Prediction without records (Godot, Unreal's precaching):** for now a type the record
  does not name is cooked whole. Predicting from the feature sets each device class turns
  on could replace that later.
- **Warming the recorded pipelines first at load:** ADR 0079's next step. The records make
  it possible; not done yet.
