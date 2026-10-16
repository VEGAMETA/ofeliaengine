#![allow(clippy::unwrap_used)]

use crate::*;
use std::time::{Duration, Instant};

struct Clock(Instant);

impl Clock {
    fn at(&self, ms: u64) -> Instant {
        self.0 + Duration::from_millis(ms)
    }
}

fn key(input: &mut Input, t: Instant, key: Key, pressed: bool) {
    input.handle(&InputEvent {
        time: t,
        kind: EventKind::Key { key, pressed },
    });
}

fn axis(input: &mut Input, t: Instant, axis: GamepadAxis, value: f32) {
    input.handle(&InputEvent {
        time: t,
        kind: EventKind::GamepadAxis {
            pad: 0,
            axis,
            value,
        },
    });
}

fn game() -> (InputMap, ActionId, ActionId, ActionId) {
    let mut map = InputMap::new();
    let jump = map.add_button("jump").unwrap();
    map.bind(jump, Binding::button(Key::Space)).unwrap();
    map.bind(jump, Binding::button(GamepadButton::South))
        .unwrap();
    let walk = map.add_axis2d("move").unwrap();
    map.bind(walk, Binding::wasd()).unwrap();
    map.bind(walk, Binding::stick(Stick::Left)).unwrap();
    let look = map.add_axis2d("look").unwrap();
    map.bind(look, Binding::mouse_motion(0.5)).unwrap();
    (map, jump, walk, look)
}

#[test]
fn scancodes_map_to_positions() {
    assert_eq!(Key::from_scancode(0x11, false), Some(Key::KeyW));
    assert_eq!(Key::from_scancode(0x1E, false), Some(Key::KeyA));
    assert_eq!(Key::from_scancode(0x39, false), Some(Key::Space));
    assert_eq!(Key::from_scancode(0x1D, false), Some(Key::ControlLeft));
    assert_eq!(Key::from_scancode(0x1D, true), Some(Key::ControlRight));
    assert_eq!(Key::from_scancode(0x48, false), Some(Key::Numpad8));
    assert_eq!(Key::from_scancode(0x48, true), Some(Key::ArrowUp));
    assert_eq!(Key::from_scancode(0x3B, false), Some(Key::F1));
    assert_eq!(Key::from_scancode(0x44, false), Some(Key::F10));
    assert_eq!(Key::from_scancode(0x58, false), Some(Key::F12));
    assert_eq!(Key::from_scancode(0x64, false), Some(Key::F13));
    assert_eq!(Key::from_scancode(0x76, false), Some(Key::F24));
    assert_eq!(Key::from_scancode(0x53, false), Some(Key::NumpadDecimal));
    assert_eq!(Key::from_scancode(0x53, true), Some(Key::Delete));
    assert_eq!(Key::from_scancode(0x7F, false), None);
    for &k in Key::ALL {
        assert_eq!(Key::from_name(k.name()), Some(k));
    }
}

#[test]
fn taps_shorter_than_a_tick_are_not_lost() {
    let (map, jump, ..) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    input.sample_tick(c.at(0));
    key(&mut input, c.at(4), Key::Space, true);
    key(&mut input, c.at(6), Key::Space, false);
    let s = input.sample_tick(c.at(16));
    assert!(s.pressed(jump) && s.just_pressed(jump));
    let fraction = s.press_fraction(jump).unwrap();
    assert!((fraction - 0.25).abs() < 0.01, "{fraction}");
    let s = input.sample_tick(c.at(32));
    assert!(!s.pressed(jump) && s.just_released(jump));
    assert!(s.press_fraction(jump).is_none());
}

#[test]
fn holds_and_releases_are_seen_in_their_tick() {
    let (map, jump, ..) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    input.sample_tick(c.at(0));
    key(&mut input, c.at(10), Key::Space, true);
    assert!(input.sample_tick(c.at(16)).just_pressed(jump));
    // Held: pressed, not "just"; key repeat does not re-trigger.
    key(&mut input, c.at(20), Key::Space, true);
    let s = input.sample_tick(c.at(32));
    assert!(s.pressed(jump) && !s.just_pressed(jump));
    assert!(s.press_fraction(jump).is_none());
    key(&mut input, c.at(40), Key::Space, false);
    let s = input.sample_tick(c.at(48));
    assert!(s.just_released(jump));
}

#[test]
fn frame_and_tick_views_are_independent() {
    let (map, jump, ..) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    input.sample_tick(c.at(0));
    input.sample_frame(c.at(0));
    key(&mut input, c.at(3), Key::Space, true);
    // Frames at 144 Hz see the press first; the tick (60 Hz) still sees it later.
    assert!(input.sample_frame(c.at(7)).just_pressed(jump));
    assert!(!input.sample_frame(c.at(14)).just_pressed(jump));
    assert!(input.sample_tick(c.at(16)).just_pressed(jump));
    key(&mut input, c.at(18), Key::Space, false);
    assert!(input.sample_frame(c.at(21)).just_released(jump));
    assert!(input.sample_tick(c.at(32)).just_released(jump));
}

#[test]
fn composites_normalize_and_sticks_use_a_circular_deadzone() {
    let (map, _, walk, _) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    key(&mut input, c.at(1), Key::KeyW, true);
    key(&mut input, c.at(1), Key::KeyD, true);
    let [x, y] = input.sample_tick(c.at(16)).vector(walk);
    let d = core::f32::consts::FRAC_1_SQRT_2;
    assert!((x - d).abs() < 1e-3 && (y - d).abs() < 1e-3, "{x} {y}");
    key(&mut input, c.at(17), Key::KeyW, false);
    key(&mut input, c.at(17), Key::KeyD, false);
    // Inside the deadzone (0.2): nothing.
    axis(&mut input, c.at(18), GamepadAxis::LeftX, 0.15);
    let s = input.sample_tick(c.at(32));
    assert_eq!(s.vector(walk), [0.0, 0.0]);
    assert!(!s.pressed(walk));
    // Godot's remap: 0.6 → (0.6 − 0.2) / 0.8 = 0.5.
    axis(&mut input, c.at(33), GamepadAxis::LeftX, 0.6);
    let [x, y] = input.sample_tick(c.at(48)).vector(walk);
    assert!((x - 0.5).abs() < 1e-3 && y == 0.0, "{x}");
}

#[test]
fn mouse_motion_sums_per_view() {
    let (map, _, _, look) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    for i in 0..4 {
        input.handle(&InputEvent {
            time: c.at(i),
            kind: EventKind::MouseMotion { dx: 3.0, dy: 2.0 },
        });
        if i == 1 {
            // A frame sample in between does not steal the tick's motion.
            assert_eq!(input.sample_frame(c.at(i)).vector(look), [3.0, -2.0]);
        }
    }
    assert_eq!(input.sample_tick(c.at(16)).vector(look), [6.0, -4.0]);
    assert_eq!(input.sample_tick(c.at(32)).vector(look), [0.0, 0.0]);
}

#[test]
fn chords_consume_their_key() {
    let mut map = InputMap::new();
    let back = map.add_button("back").unwrap();
    map.bind(back, Binding::button(Key::KeyS)).unwrap();
    let save = map.add_button("save").unwrap();
    map.bind(save, Binding::chord(&[Key::ControlLeft], Key::KeyS))
        .unwrap();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    key(&mut input, c.at(1), Key::KeyS, true);
    let s = input.sample_tick(c.at(16));
    assert!(s.pressed(back) && !s.pressed(save));
    key(&mut input, c.at(17), Key::KeyS, false);
    key(&mut input, c.at(18), Key::ControlLeft, true);
    key(&mut input, c.at(19), Key::KeyS, true);
    let s = input.sample_tick(c.at(32));
    assert!(
        s.pressed(save) && !s.pressed(back),
        "Ctrl+S must not also move back"
    );
}

#[test]
fn higher_contexts_hide_their_inputs_from_lower_ones() {
    let mut map = InputMap::new();
    let jump = map.add_button("jump").unwrap();
    map.bind(jump, Binding::button(Key::Space)).unwrap();
    let fire = map.add_button("fire").unwrap();
    map.bind(fire, Binding::button(MouseButton::Left)).unwrap();
    let vehicle = map.add_context("vehicle", 10).unwrap();
    let brake = map.add_button("brake").unwrap();
    map.bind(brake, Binding::button(Key::Space)).unwrap();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    key(&mut input, c.at(1), Key::Space, true);
    input.handle(&InputEvent {
        time: c.at(1),
        kind: EventKind::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        },
    });
    let s = input.sample_tick(c.at(16));
    assert!(s.pressed(jump) && !s.pressed(brake) && s.pressed(fire));
    input.set_context_enabled(vehicle, true);
    let s = input.sample_tick(c.at(32));
    assert!(
        s.pressed(brake) && !s.pressed(jump),
        "the vehicle context owns Space"
    );
    assert!(
        s.pressed(fire),
        "inputs the vehicle context does not bind still reach gameplay"
    );
}

#[test]
fn triggers_as_buttons_have_strength() {
    let mut map = InputMap::new();
    let accel = map.add_button("accelerate").unwrap();
    map.bind(
        accel,
        Binding::button(ButtonSource::AxisPositive(GamepadAxis::RightTrigger)),
    )
    .unwrap();
    map.set_deadzone(accel, 0.1);
    let steer = map.add_axis1d("steer").unwrap();
    map.bind(
        steer,
        Binding::Buttons1d {
            negative: Key::KeyA.into(),
            positive: Key::KeyD.into(),
        },
    )
    .unwrap();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    axis(&mut input, c.at(1), GamepadAxis::RightTrigger, 0.55);
    key(&mut input, c.at(1), Key::KeyA, true);
    let s = input.sample_tick(c.at(16));
    assert!(s.pressed(accel));
    assert!((s.strength(accel) - 0.5).abs() < 1e-3);
    assert_eq!(s.axis(steer), -1.0);
}

#[test]
fn names_resolve_like_godot() {
    let (map, ..) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    key(&mut input, c.at(1), Key::Space, true);
    input.sample_tick(c.at(16));
    assert!(input.tick().pressed("jump"));
    assert!(input.tick().just_pressed("jump"));
    assert!(!input.tick().pressed("no such action"));
    assert_eq!(input.tick().vector("move"), [0.0, 0.0]);
}

#[test]
fn map_rejects_conflicts_and_wrong_kinds() {
    let (mut map, jump, walk, _) = game();
    let crouch = map.add_button("crouch").unwrap();
    assert!(matches!(
        map.bind(crouch, Binding::button(Key::Space)),
        Err(MapError::Conflict { .. })
    ));
    assert!(matches!(
        map.bind(jump, Binding::stick(Stick::Left)),
        Err(MapError::WrongKind { .. })
    ));
    assert!(matches!(
        map.bind(crouch, Binding::button(Key::KeyW)),
        Err(MapError::Conflict { .. })
    ));
    // The same key with a modifier is a different input.
    map.bind(crouch, Binding::chord(&[Key::ShiftLeft], Key::Space))
        .unwrap();
    assert!(matches!(
        map.add_button("jump"),
        Err(MapError::DuplicateAction(_))
    ));
    // Rebinding: jump's first binding moves to C.
    map.rebind(jump, 0, Binding::button(Key::KeyC)).unwrap();
    let keys: Vec<_> = map.bindings(jump).map(|(_, b)| *b).collect();
    assert_eq!(keys[0], Binding::button(Key::KeyC));
    assert!(matches!(
        map.rebind(crouch, 0, Binding::button(Key::KeyC)),
        Err(MapError::Conflict { .. })
    ));
    map.unbind(walk, 1).unwrap();
    assert_eq!(map.bindings(walk).count(), 1);
}

#[test]
fn encoded_ticks_decode_to_the_same_state() {
    let (map, jump, walk, look) = game();
    let mut input = Input::new(map.clone());
    let c = Clock(Instant::now());
    let mut server = ActionState::default();
    let mut bytes = Vec::new();
    for tick in 0..6u64 {
        let t = c.at(tick * 16);
        match tick {
            1 => key(&mut input, t + Duration::from_millis(3), Key::Space, true),
            2 => axis(&mut input, t, GamepadAxis::LeftY, -0.9),
            3 => input.handle(&InputEvent {
                time: t,
                kind: EventKind::MouseMotion { dx: -7.0, dy: 1.0 },
            }),
            4 => key(&mut input, t, Key::Space, false),
            _ => {}
        }
        let client = input.sample_tick(t + Duration::from_millis(15)).clone();
        bytes.clear();
        client.encode(&map, &mut bytes);
        assert_eq!(server.decode(&map, &bytes).unwrap(), bytes.len());
        for a in [jump, walk, look] {
            assert_eq!(server.pressed(a), client.pressed(a), "tick {tick}");
            assert_eq!(
                server.just_pressed(a),
                client.just_pressed(a),
                "tick {tick}"
            );
            assert_eq!(
                server.just_released(a),
                client.just_released(a),
                "tick {tick}"
            );
            assert_eq!(server.vector(a), client.vector(a), "tick {tick}");
            assert_eq!(
                server.press_fraction(a),
                client.press_fraction(a),
                "tick {tick}"
            );
        }
    }
    // Nothing pressed: one byte of flags for three actions.
    axis(&mut input, c.at(199), GamepadAxis::LeftY, 0.0);
    bytes.clear();
    input.sample_tick(c.at(200)).encode(&map, &mut bytes);
    assert_eq!(bytes.len(), 1);
    assert_eq!(server.decode(&map, &[]), Err(DecodeError::Truncated));
}

#[test]
fn local_players_listen_to_their_own_pads() {
    let (map, jump, ..) = game();
    let mut p1 = Input::new(map.clone());
    let mut p2 = Input::new(map);
    p1.set_devices(DeviceFilter::gamepad(0));
    p2.set_devices(DeviceFilter::gamepad(1));
    let c = Clock(Instant::now());
    let press = InputEvent {
        time: c.at(1),
        kind: EventKind::GamepadButton {
            pad: 1,
            button: GamepadButton::South,
            pressed: true,
        },
    };
    p1.handle(&press);
    p2.handle(&press);
    key(&mut p1, c.at(1), Key::Space, true);
    assert!(!p1.sample_tick(c.at(16)).pressed(jump));
    assert!(p2.sample_tick(c.at(16)).pressed(jump));
}

#[test]
fn released_all_when_the_window_loses_the_focus() {
    let (map, jump, walk, _) = game();
    let mut input = Input::new(map);
    let c = Clock(Instant::now());
    input.sample_tick(c.at(0));
    key(&mut input, c.at(5), Key::Space, true);
    key(&mut input, c.at(6), Key::KeyW, true);
    axis(&mut input, c.at(7), GamepadAxis::LeftX, 1.0);
    let s = input.sample_tick(c.at(16));
    assert!(s.pressed(jump) && s.vector(walk) != [0.0, 0.0]);
    // The key-ups go to another window.
    input.release_all();
    let s = input.sample_tick(c.at(32));
    assert!(s.just_released(jump));
    assert_eq!(s.vector(walk), [0.0, 0.0]);
}
