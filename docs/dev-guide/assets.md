# Assets: cooking scenes into packs

Your sources (glTF scenes, PNG and JPEG textures) are for editing. A game loads
**packs**: one file per scene holding exactly what the GPU takes. That means compressed
textures with their mip levels, meshes with their detail levels, materials and
instances (ADR 0056). A pack needs neither the sources nor the derived data cache, and
it loads with a few large reads.

## Cooking

```text
ofeliaengine cook assets/level1/level1.gltf -o packs/level1.pack
```

```text
assets/level1/level1.gltf: 69 textures (69 from the derived data cache), 103 meshes, 25 materials, 103 instances; cooked in 0.1 s
packs/level1.pack: 361 entries, 71.5 MB (104.5 MB raw, 68%), written in 2.6 s
```

- **Textures** are filtered and block-compressed as for the renderer (BC7, BC5 for
  normal maps; see rendering.md §2). They go through the derived data cache, by default
  `.ddc` next to the scene's folder (`--ddc <dir>` for another), so a second cook takes
  seconds.
- **Meshes** get detail levels (`--no-lods` to skip them).
- **Every entry** is compressed with zstd in chunks of 1 MiB (each decompresses on its
  own thread) when that saves at least 5%. `--level 0` stores everything raw, which loads
  a little faster from NVMe but makes the file a third larger.
- **Textures are split for streaming:** the levels up to 128 texels form one entry, and
  each larger level is an entry of its own. The file starts with meshes and those small
  levels (the whole scene, blurry), then the large levels from the smallest.
- **The same texture** used by several materials is stored once.

## Loading

```rust
use engine_assets::load_pack;
use engine_core::jobs::ThreadPool;

let jobs = ThreadPool::builder().build()?;
let (scene, stats) = load_pack("packs/level1.pack".as_ref(), &jobs, 32)?;
// scene.textures: EncodedTexture (format, size, levels): add them to the TexturePool.
// scene.meshes: CookedMesh (vertices, indices, detail levels): add them to the GeometryPool.
// scene.materials: CookedMaterial, with textures as indices into scene.textures.
// scene.instances: mesh, material and transform of each placed mesh.
println!("{:.1} MB in {:.0} ms", stats.read_bytes as f64 / 1e6, stats.seconds * 1e3);
```

`examples/scene_bench.rs` in `engine_render` turns a `CookedScene` into textures,
materials and draws; copy that part. Its `--pack <file>` option loads a pack.

- **Reads go straight to the drive:**
  - the pack is read unbuffered through a Windows 11 I/O ring, 32 reads in flight;
  - entries decompress on the job pool as they arrive;
  - loading a pack a second time reads the drive again: keep what you need loaded.
- **Damage is found, never drawn:**
  - opening checks the header, the table and every entry's bounds;
  - loading checks each chunk's hash;
  - a damaged, truncated or foreign file gives `PackError::Invalid` with the reason.
- **Packs have a version.** An engine that changes the format refuses older packs with
  a message; cook them again.

## Streaming textures while the game runs

Load the scene and its meshes, create the textures empty, and let their levels arrive
while frames are drawn (ADR 0057):

```rust
use engine_assets::open_scene;
use engine_assets::pack::{PackStreamer, StreamThread};

let (pack, layout, _) = open_scene("packs/level1.pack".as_ref(), &jobs, 32)?;
// layout.scene: meshes, materials and instances, as from load_pack.
// layout.textures: format, size and levels of each texture, no data yet.
let ids: Vec<TextureId> = layout.textures.iter().map(|t| {
    textures.add_empty(&mut allocator, t.width, t.height, format_of(t.format), t.levels)
}).collect::<Result<_, _>>()?;
// Reads and decompression on a thread of their own.
let mut stream = StreamThread::spawn(pack, 32, 64, 4)?;
let mut queue: VecDeque<TextureEntry> = layout.texture_entries.into(); // tails first

// Every frame, before textures.begin_frame:
while stream.has_room() && let Some(&te) = queue.front() {
    let bytes = PackStreamer::target_bytes(&stream.entries()[te.entry as usize]) as u64;
    let Some(mut span) = textures.staging(&mut allocator, bytes)? else { break }; // ring full
    // SAFETY: the span is this request's until it comes back from poll.
    unsafe { stream.request(te.entry, span.as_mut_ptr(), span.len() as usize, tag)? };
    in_flight.insert(tag, (span, te));
    queue.pop_front();
}
stream.poll(&mut finished);
for done in finished.drain(..) {
    let (span, te) = in_flight.remove(&done.tag).unwrap();
    done.result?;
    textures.upload_levels(ids[te.texture as usize], te.first_level, te.level_count, span)?;
}
```

`examples/scene_bench.rs` in `engine_render` does exactly this (`--pack <file> --stream`).

- **The data goes straight to the GPU:**
  - raw entries are read from the drive into the texture pool's staging ring;
  - compressed ones are decompressed into it;
  - the GPU copies from there.
- **Every texture shows up blurry first:** its levels up to 128 texels (the tail) come
  first in the file. Then the larger levels arrive, smallest first, and the texture
  sharpens as each joins.
- **The frame's thread only queues and collects:** microseconds per frame. Reads,
  decompression and checks run on the streaming thread and its workers, below normal
  priority.
- **The staging ring** (64 MiB by default, `TexturePool::set_staging_size`) bounds how
  far reading runs ahead: `staging` returns `None` while it is full. It must hold the
  largest level: a 4K BC7 level is 16 MiB, an 8K one 64 MiB.

Sponza, counted from the first frame:

| | hard disk | NVMe |
|---|---:|---:|
| scene and meshes, before the first frame | 13–316 ms | 7 ms |
| every texture visible (blurry) | 26–61 ms | 14 ms |
| every level | 394–564 ms | 40–41 ms |

## What it costs

Sponza (104.5 MB once loaded, 71.5 MB in its pack), on the reference machine:

| | hard disk | NVMe |
|---|---:|---:|
| **from its pack** | **0.52–0.66 s** | **34–36 ms** |
| from glTF and the derived data cache, first time | 3.1 s | 45 ms |

- zstd costs 0.1–0.2 s of CPU in all, spread over the job pool.
- On a hard disk the pack loads at the disk's full speed (about 120 MB/s): smaller
  packs load faster there.

## Reading files yourself

`engine_platform::io` is the engine's streaming reader:

- `ReadFile::open` opens a file for unbuffered reads.
- A `ReadQueue` holds many reads at once. Each takes an `AlignedBuffer` and gives it back
  with its completion.
- Offsets are multiples of `READ_ALIGN` (4096 bytes).
- Keep 8–32 reads in flight for small reads: on NVMe that is 9× faster than one at a
  time.

## Not yet

- Loading only the levels the camera needs, and a memory budget that evicts the rest:
  today every level streams in.
- Streaming detail levels of meshes.
- Hot reload of packs: cook on save, swap while running.
