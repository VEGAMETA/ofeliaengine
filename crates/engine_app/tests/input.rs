//! The game's input actions in the player: the world's `engine_input::Input` resource,
//! sampled before every tick and before presentation, and released when the window loses
//! the focus. Offscreen, so the test feeds the events itself (a window's player reads raw
//! input and XInput pads into it).

#![allow(clippy::unwrap_used)]

mod common;

use engine_app::{Game, Player, Window};
use engine_ecs::World;
use engine_input::{Binding, EventKind, Input, InputEvent, InputMap, Key};
use std::time::Instant;

/// What the game's hooks saw of its "jump" action.
#[derive(Debug, Default)]
struct Jumps {
    ticks: usize,
    just_pressed: usize,
    held: usize,
    presented_held: usize,
}

impl Game for Jumps {
    fn tick(&mut self, world: &mut World, _dt: f32) {
        let actions = world.resource::<Input>().unwrap().tick();
        self.ticks += 1;
        self.just_pressed += usize::from(actions.just_pressed("jump"));
        self.held += usize::from(actions.pressed("jump"));
    }

    fn present(&mut self, world: &mut World, _alpha: f32) {
        let actions = world.resource::<Input>().unwrap().frame();
        self.presented_held += usize::from(actions.pressed("jump"));
    }
}

fn space(player: &mut Player, pressed: bool) {
    player
        .world_mut()
        .resource_mut::<Input>()
        .unwrap()
        .handle(&InputEvent {
            time: Instant::now(),
            kind: EventKind::Key {
                key: Key::Space,
                pressed,
            },
        });
}

#[test]
fn the_games_ticks_and_frames_see_its_actions() {
    let mut map = InputMap::new();
    let jump = map.add_button("jump").unwrap();
    map.bind(jump, Binding::button(Key::Space)).unwrap();
    let Some((mut player, _)) = common::player_with(|settings| {
        settings.window = Window::Offscreen {
            width: 64,
            height: 64,
        };
        settings.input = Some(map);
    }) else {
        return;
    };
    // A tick per frame.
    player.set_frame_time(Some(1.0 / 64.0));
    let mut game = Jumps::default();
    player.frame(&mut game).unwrap();
    assert_eq!((game.ticks, game.just_pressed, game.held), (1, 0, 0));

    // Pressed and held over three frames: one press, three ticks held.
    space(&mut player, true);
    for _ in 0..3 {
        player.frame(&mut game).unwrap();
    }
    assert_eq!(game.just_pressed, 1);
    assert_eq!(game.held, 3);
    assert_eq!(game.presented_held, 3);

    // A tap between two ticks is not lost.
    space(&mut player, false);
    player.frame(&mut game).unwrap();
    space(&mut player, true);
    space(&mut player, false);
    player.frame(&mut game).unwrap();
    assert_eq!(game.just_pressed, 2, "{game:?}");

    // Held when the window loses the focus: released, nothing stays down.
    space(&mut player, true);
    player.frame(&mut game).unwrap();
    let held = game.held;
    player
        .world_mut()
        .resource_mut::<Input>()
        .unwrap()
        .release_all();
    player.frame(&mut game).unwrap();
    player.frame(&mut game).unwrap();
    assert_eq!(game.held, held, "{game:?}");
}
