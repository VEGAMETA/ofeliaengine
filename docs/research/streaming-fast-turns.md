# Streaming on fast camera turns: what is safe to build

Queue item 0b (`docs/status.md`): the owner saw models and textures arrive late in UE when the
camera turned quickly, and ADR 0076 has the same risk. Its plan listed feedback over 360°
(a face of a small cube around the camera per frame, besides the main view), mesh levels by
a sphere around the camera, and lower latency; "first check it stays clear of the Graphine
prediction patents". This note does that check and measures where the latency goes today.
Checked 2026-10-10. Not legal advice: the reading below is the engineering team's, for the
owner to decide on.

## The patents

Graphine's "Prediction system for texture streaming" family, now owned by Unity
Technologies ApS, priority 2014-11-03. Claims read from Google Patents (2026-10-10):

| Patent | Granted | Status (2026-10-10) |
|---|---|---|
| US10748241B2 | 2020-08-18 | maintained (fee paid 2024-01-09), anticipated expiration 2035-10-30 |
| US11195250B2 (continuation) | 2021-12-07 | maintained (fee paid 2025-05-13), anticipated expiration 2035-10-30 |
| US11893656B2 (continuation) | 2024-02-06 | adjusted expiration 2035-12-23 |
| EP3216011B1 | granted | Europe |

What the independent claims require, all together:

- **US10748241B2, claims 1, 17, 18:** a *plurality of predictors* receiving information on
  the virtual world, each identifying a predicted set of subregions (texture tiles or
  levels, mesh levels) with a *different prediction scheme of a different accuracy and
  computational complexity*; a *working set manager* that *combines the predicted sets*
  into the working set, determines *priorities of steering instructions* for loading it
  into a streaming cache, and outputs them in that order, the combining and the priorities
  *based on an analysis of each predictor's output*.
- **US11195250B2, claims 1, 19, 20:** the same without "different accuracy and
  complexity": a plurality of predictors with different prediction schemes, combining
  their sets into a working set based on an analysis of each one's output, priorities
  likewise, steering instructions in priority order. **The broadest of the three.**
- **US11893656B2, claims 1, 16, 19:** a plurality of predictors, combining, priorities,
  plus *modifying one predictor's output with another's* and adjusting the combination or
  the priorities from the modified output.

The dependent claims name the schemes: rendering the scene from a virtual camera and reading
back unique subregion identifiers (feedback: US10748241 claim 10); **a virtual camera
position other than the main one** (claim 11); subregions within a virtual distance
(claim 9); rendering without occluders (claim 12); weights counting how many predicted sets
hold a subregion (claims 13–14).

## Reading the plan against them

| Idea | Predictors | Reading |
|---|---|---|
| Feedback of the main view only (today, ADR 0076) | one | outside: no plurality, nothing combined |
| **360° feedback: extra views besides the main view** | two: main view + other camera positions, their wanted levels merged | **reads on US11195250 claim 1**, with the extra views as the scheme of its claim 11: avoid |
| Mesh or texture levels by a sphere around the camera, besides feedback | two schemes (distance, feedback) combined into one working set | close to claim 1 with claim 9: avoid combining them for the same objects |
| One feedback pass rendered with a wider field of view than the screen, *instead of* the main view's | one (the same camera position, one scheme) | outside the independent claims as read here; owner to confirm |
| Keeping recently seen levels longer (evict least recently seen first) | none: retention, not prediction | outside |
| Newly visible requests first, coarser levels before finer ones | ordering of one predictor's output | outside (no combining) |
| Larger always-resident tails | none | outside |
| Faster arrival (reads, decompression, upload) | none | outside |

ADR 0076 already noted that mesh levels come from the frustum and the instances' screen
size while textures come from feedback, under one budget. Each object kind gets its
levels from one source; the budget does not merge the sets of two schemes for the same
objects. That reading stays as ADR 0076 left it (its question to the team, whether counsel
should look before any prefetching, is still open).

## Where the latency goes today

`stream_bench` on the RTX, Sponza pack (`target/audit/sponza.pack`, in the OS file cache),
900 frames paced at 60 fps, ample budget (2026-10-10):

- 405 levels arrive (92.8 MiB read). Request → uploaded mean 197.8 ms, worst 666.7 ms
  (40 frames): the first view asks for almost everything at once, and the queue drains
  over about 50 frames (settled at frame 53).
- After that, levels arrive 1–2 frames after they are asked for (16.7–33.3 ms), and
  nothing is evicted: Sponza (90.7 MiB with every level) fits the budget, so a turn shows
  no pop-in at all.

ADR 0076's 337–503 ms means came from the same first burst, with the pack on the SATA disk.
On fast turns, pop-in therefore needs two things together: a budget too small for what is
around the camera (levels evicted behind it), and a slow refill (a disk, or many levels at
once). Next measurement: a fast-turn mode in `stream_bench` (180° in a fraction of a second
in a tight budget, the pack on the HDD and on the NVMe drive) counting the frames and the
share of visible texels below their wanted level until it is resident.

## The owner's answer (2026-10-10)

What the engine has does not fall under the family: one source per kind of object, a budget
that only divides bytes (ADR 0076's question answered). Combining several schemes' sets for
the same objects stays out, so 360° views besides the main view are not built. The wider
field of view (one feedback pass that sees past the screen's edges, instead of the main
view's) is a later step, decided by the fast-turn measurements.

## Proposal

Build only what is outside the claims as read above: retention by last use, newly visible
requests first with coarser levels first, larger tails, faster arrival, and the fast-turn
measurement. Ask the owner about the single wider-field-of-view feedback pass, and leave
360° views and distance prediction combined with feedback out unless counsel clears them.
