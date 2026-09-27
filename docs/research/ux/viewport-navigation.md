# Viewport navigation: how editors move the 3D camera

Checked 2026-09-27 against the current documentation and default keymaps of each editor.
Products studied as users; no code carried over.

## What each editor does

| | Orbit | Pan | Zoom | Look / fly |
|---|---|---|---|---|
| **Godot 4** | middle drag | Shift + middle drag | wheel; Ctrl + middle drag | right drag looks; hold right + WASD/QE flies (freelook), wheel changes fly speed |
| **Blender** | middle drag | Shift + middle drag | wheel; Ctrl + middle drag | Shift + \` walk/fly mode (a mode, entered and left explicitly) |
| **Unity** | Alt + left drag | middle drag (or Q hand tool + left drag) | wheel; Alt + right drag | hold right + WASD/QE flies, wheel while flying changes speed |
| **Unreal** | Alt + left drag | middle drag | wheel; Alt + right drag | hold right + WASD/QE flies; right drag alone looks |

Common to all four:

- The wheel zooms toward what is under or in front of the camera.
- The middle button (or a modifier with it) is the camera button; the left button stays
  for selection and gizmos.
- Flying is "while the right button is held", never a mode you can forget you are in
  (except Blender's walk mode).

## What annoys

- **Alt + left drag** (Unity, Unreal) collides with window-manager shortcuts on some
  desktops and with Alt-based UI shortcuts; users of laptops without a middle button like
  it, though.
- **Blender's walk mode** is a mode (Raskin): the viewport stops reacting as usual until
  you leave it.
- Orbiting around the scene origin instead of what you look at: Godot and Blender orbit a
  pivot that "Focus selection" (F / numpad .) moves, and new users lose the pivot.
- Zoom that slows to a crawl near the pivot (a pure distance multiply) and cannot pass
  through it; Unreal's dolly moves the camera instead.

## What we take (first version, `engine_editor::camera`)

- **Godot's bindings**, since the team works from Godot's layout: middle drag orbits,
  Shift + middle drag pans, the wheel zooms toward the pivot, right drag looks around
  (the pivot moves with the view). The left button stays for the UI, selection and
  gizmos.
- Pan speed follows the pivot's distance and the field of view, so the scene under the
  cursor moves with it.
- Pitch stops at ±89°: no flip over the pole.

## Not yet (next steps)

- Fly with right button + WASD/QE and wheel speed while flying (needs held-key state in
  the input collector).
- Focus selection (F): move the pivot to the selection's bounds.
- Zoom that passes through the pivot (dolly when close), orbit around the point under
  the cursor.
- Rebindable bindings through the keymap (the command palette's `Keymap`), and an
  Alt + left drag orbit option for laptops without a middle button.
