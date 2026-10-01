# 0067. No camera motion blur in gameplay: cinematic views only, by license and by API

- Status: Accepted (team decision, 2026-10-01)
- Date: 2026-10-01
- Related: [ADR 0020](0020-engine-license.md) (engine license),
  [ADR 0043](0043-motion-vectors-and-taau.md) (motion vectors)

## Context

Motion blur is on the post-processing list (core audit item 15) and is not implemented
yet. Blur from the camera's own motion smears the whole picture whenever the player turns
or moves the camera: it hides detail exactly when the player needs to read the scene, adds
perceived latency and makes some players feel unwell. The team does not want games made
with the engine to use it in play; in non-interactive sequences, where the camera is not
the player's, it is a legitimate film look.

## Decision (team, 2026-10-01)

1. **License.** The engine's final license (ADR 0020: MIT is a placeholder until then)
   must forbid motion blur caused by the camera's motion while the player controls the
   camera or the game is otherwise interactive. It is allowed in non-interactive
   sequences: cutscenes and other cinematics. The wording is the lawyer's; this ADR is
   the requirement. Under the MIT placeholder the restriction cannot be stated (MIT does
   not restrict use), which is one more reason the final license must be in place before
   the engine leaves the team.
2. **API, from the first line of code.** The restriction is built into the renderer, so
   games comply without reading the license:
   - motion blur is a property of a **cinematic view** (a view marked non-interactive,
     e.g. `ViewKind::Cinematic` driven by a sequence/timeline), not of a gameplay view;
   - a gameplay view has no camera motion blur to turn on: the blur pass, if any runs for
     it, uses only object motion relative to the camera with the camera's own motion
     removed (motion vectors minus the camera term), or does not run at all;
   - switching a view to cinematic is explicit and visible in tools (the editor shows it),
     so a game cannot do it by accident.
3. **Code.** The full implementation (camera and object motion, quality settings) lives
   in the engine and is tested like any other effect; only its use is limited as above.

## Consequences

- When motion blur is built (post-processing, item 15 of the work queue), its API follows
  point 2; its ADR refers here, and a golden image covers a cinematic view.
- Object motion blur in gameplay (a spinning wheel, a fast projectile) is not covered by
  this decision; it stays a separate question if a game needs it.
- The final license text must carry point 1 (recorded in ADR 0020's terms).
