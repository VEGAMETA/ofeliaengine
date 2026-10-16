//! Turning input events into action states, per simulation tick and per rendered frame.

use crate::device::{
    EventKind, GAMEPAD_AXIS_COUNT, GAMEPAD_BUTTON_COUNT, GamepadAxis, InputEvent, KEY_COUNT,
    MAX_GAMEPADS, MouseButton,
};
use crate::map::{ActionId, ActionKind, Binding, ButtonSource, ContextId, InputMap};
use std::time::Instant;

/// Fixed-point scale of action values: 16 fractional bits. Values are quantized when
/// sampled, so a client predicting with them and a server decoding them from the network
/// compute with identical numbers.
pub const VALUE_ONE: i32 = 1 << 16;

/// Sources, as slots for context blocking and chord disambiguation.
const MOUSE_SLOT: usize = 128;
const PAD_BUTTON_SLOT: usize = MOUSE_SLOT + 8;
const AXIS_POS_SLOT: usize = PAD_BUTTON_SLOT + GAMEPAD_BUTTON_COUNT;
const AXIS_NEG_SLOT: usize = AXIS_POS_SLOT + GAMEPAD_AXIS_COUNT;
const WHEEL_UP_SLOT: usize = AXIS_NEG_SLOT + GAMEPAD_AXIS_COUNT;
const WHEEL_DOWN_SLOT: usize = WHEEL_UP_SLOT + 1;
const STICK_SLOT: usize = WHEEL_DOWN_SLOT + 1;
const MOUSE_MOTION_SLOT: usize = STICK_SLOT + 2;
const SLOTS: usize = MOUSE_MOTION_SLOT + 1;

/// Raw digital inputs, as bits: keys, mouse buttons, then each pad's buttons.
const RAW_MOUSE: usize = 128;
const RAW_PAD: usize = RAW_MOUSE + 8;
const RAW_WHEEL_UP: usize = RAW_PAD + MAX_GAMEPADS * 16;
const RAW_WHEEL_DOWN: usize = RAW_WHEEL_UP + 1;
const RAW_BITS: usize = RAW_WHEEL_DOWN + 1;

const _: () = assert!(KEY_COUNT <= 128 && GAMEPAD_BUTTON_COUNT <= 16);

#[derive(Clone, Copy)]
struct Bits<const W: usize>([u64; W]);

impl<const W: usize> Default for Bits<W> {
    fn default() -> Self {
        Self([0; W])
    }
}

impl<const W: usize> Bits<W> {
    #[inline]
    fn get(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }
    #[inline]
    fn set(&mut self, i: usize, v: bool) {
        if v {
            self.0[i / 64] |= 1 << (i % 64);
        } else {
            self.0[i / 64] &= !(1 << (i % 64));
        }
    }
    fn clear(&mut self) {
        self.0 = [0; W];
    }
    fn or(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
    }
}

type RawBits = Bits<{ RAW_BITS.div_ceil(64) }>;
type SlotBits = Bits<{ SLOTS.div_ceil(64) }>;

fn slot(source: ButtonSource) -> usize {
    match source {
        ButtonSource::Key(k) => k as usize,
        ButtonSource::Mouse(b) => MOUSE_SLOT + b as usize,
        ButtonSource::Gamepad(b) => PAD_BUTTON_SLOT + b as usize,
        ButtonSource::AxisPositive(a) => AXIS_POS_SLOT + a as usize,
        ButtonSource::AxisNegative(a) => AXIS_NEG_SLOT + a as usize,
        ButtonSource::WheelUp => WHEEL_UP_SLOT,
        ButtonSource::WheelDown => WHEEL_DOWN_SLOT,
    }
}

fn binding_slots(binding: &Binding, out: &mut SlotBits) {
    let mut sources = Vec::new();
    binding.sources(&mut sources);
    for s in sources {
        out.set(slot(s), true);
    }
    match *binding {
        Binding::Axis { axis, .. } => {
            out.set(AXIS_POS_SLOT + axis as usize, true);
            out.set(AXIS_NEG_SLOT + axis as usize, true);
        }
        Binding::Stick { stick, .. } => out.set(STICK_SLOT + stick as usize, true),
        Binding::MouseMotion { .. } => out.set(MOUSE_MOTION_SLOT, true),
        _ => {}
    }
}

/// Which devices a player's [`Input`] listens to (local multiplayer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFilter {
    /// Keyboard and mouse.
    pub keyboard_mouse: bool,
    /// Bit `n`: gamepad `n`.
    pub gamepads: u8,
}

impl DeviceFilter {
    /// Every device (single player).
    pub const ALL: DeviceFilter = DeviceFilter {
        keyboard_mouse: true,
        gamepads: 0xFF,
    };

    /// One gamepad only.
    pub const fn gamepad(index: u8) -> Self {
        Self {
            keyboard_mouse: false,
            gamepads: 1 << index,
        }
    }
}

/// Changes accumulated between two samples of one view (tick or frame).
#[derive(Clone)]
struct Interval {
    /// Went down at some point since the last sample.
    latched: RawBits,
    /// First press time since the last sample, per raw bit.
    press_time: Box<[Option<Instant>]>,
    mouse: [f32; 2],
    last_sample: Option<Instant>,
}

impl Interval {
    fn new() -> Self {
        Self {
            latched: RawBits::default(),
            press_time: vec![None; RAW_BITS].into_boxed_slice(),
            mouse: [0.0; 2],
            last_sample: None,
        }
    }

    fn press(&mut self, bit: usize, time: Instant) {
        if !self.latched.get(bit) {
            self.latched.set(bit, true);
            self.press_time[bit] = Some(time);
        }
    }

    fn reset(&mut self, now: Instant) {
        self.latched.clear();
        self.press_time.fill(None);
        self.mouse = [0.0; 2];
        self.last_sample = Some(now);
    }
}

/// Every action's state at one sample: pressed flags, quantized values, and when in the
/// sampled interval each press happened.
#[derive(Debug, Clone, Default)]
pub struct ActionState {
    pressed: Vec<u64>,
    previous: Vec<u64>,
    /// Per action: `[x, y]` in 16.16 fixed point (buttons and 1D axes use `x`).
    values: Vec<[i32; 2]>,
    /// Per action: position of the first press within the sampled interval, 0..=65535;
    /// `u16::MAX` when the action was not pressed during the interval.
    fractions: Vec<u16>,
}

/// No press during the interval (see [`ActionState::press_fraction`]).
pub(crate) const NO_FRACTION: u16 = u16::MAX;

impl ActionState {
    pub(crate) fn resize(&mut self, actions: usize) {
        let words = actions.div_ceil(64);
        self.pressed.resize(words, 0);
        self.previous.resize(words, 0);
        self.values.resize(actions, [0; 2]);
        self.fractions.resize(actions, NO_FRACTION);
    }

    pub(crate) fn begin(&mut self) {
        core::mem::swap(&mut self.pressed, &mut self.previous);
        self.pressed.fill(0);
        self.values.fill([0; 2]);
        self.fractions.fill(NO_FRACTION);
    }

    #[inline]
    fn bit(words: &[u64], i: usize) -> bool {
        words.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    pub(crate) fn set_pressed(&mut self, action: usize, pressed: bool) {
        if pressed {
            self.pressed[action / 64] |= 1 << (action % 64);
        } else {
            self.pressed[action / 64] &= !(1 << (action % 64));
        }
    }

    pub(crate) fn set_value(&mut self, action: usize, value: [i32; 2]) {
        self.values[action] = value;
    }

    pub(crate) fn set_fraction(&mut self, action: usize, fraction: u16) {
        self.fractions[action] = fraction;
    }

    pub(crate) fn raw_value(&self, action: usize) -> [i32; 2] {
        self.values[action]
    }

    pub(crate) fn raw_fraction(&self, action: usize) -> u16 {
        self.fractions[action]
    }

    pub(crate) fn pressed_words(&self) -> &[u64] {
        &self.pressed
    }

    /// Whether the action is pressed (for axes: moved out of its deadzone). A press and
    /// release that both happened within the sampled interval count as pressed for that
    /// one sample.
    #[inline]
    pub fn pressed(&self, action: ActionId) -> bool {
        Self::bit(&self.pressed, action.index())
    }

    /// Pressed now, not at the previous sample.
    #[inline]
    pub fn just_pressed(&self, action: ActionId) -> bool {
        self.pressed(action) && !Self::bit(&self.previous, action.index())
    }

    /// Pressed at the previous sample, not now.
    #[inline]
    pub fn just_released(&self, action: ActionId) -> bool {
        !self.pressed(action) && Self::bit(&self.previous, action.index())
    }

    /// Strength of a button action (0..=1), or the value of a 1D axis (−1..=1).
    #[inline]
    pub fn strength(&self, action: ActionId) -> f32 {
        self.values
            .get(action.index())
            .map_or(0.0, |v| v[0] as f32 / VALUE_ONE as f32)
    }

    /// Value of a 1D axis action (−1..=1). Same as [`ActionState::strength`].
    #[inline]
    pub fn axis(&self, action: ActionId) -> f32 {
        self.strength(action)
    }

    /// Value of a 2D axis action: +X right, +Y up. Sticks and key composites are within
    /// the unit circle; mouse motion adds counts × sensitivity for the interval.
    #[inline]
    pub fn vector(&self, action: ActionId) -> [f32; 2] {
        self.values.get(action.index()).map_or([0.0; 2], |v| {
            [
                v[0] as f32 / VALUE_ONE as f32,
                v[1] as f32 / VALUE_ONE as f32,
            ]
        })
    }

    /// When within the sampled interval the action was first pressed, as a fraction
    /// 0..1 of the interval, if it was pressed during it. Networking uses it as the
    /// sub-tick time of a shot (ADR 0019).
    pub fn press_fraction(&self, action: ActionId) -> Option<f32> {
        match self.fractions.get(action.index()) {
            Some(&f) if f != NO_FRACTION => Some(f as f32 / 65535.0),
            _ => None,
        }
    }
}

fn quantize(v: f32) -> i32 {
    (v * VALUE_ONE as f32).round() as i32
}

/// Godot's radial deadzone for one axis: zero inside, remapped to 0..1 outside.
fn deadzone1(v: f32, deadzone: f32) -> f32 {
    let a = v.abs();
    if a <= deadzone {
        0.0
    } else {
        v.signum() * ((a - deadzone) / (1.0 - deadzone)).min(1.0)
    }
}

/// Godot's circular deadzone for a stick (`Input::get_vector`).
fn deadzone2(x: f32, y: f32, deadzone: f32) -> [f32; 2] {
    let len = (x * x + y * y).sqrt();
    if len <= deadzone {
        [0.0; 2]
    } else if len > 1.0 {
        [x / len, y / len]
    } else {
        let scale = (len - deadzone) / (1.0 - deadzone) / len;
        [x * scale, y * scale]
    }
}

/// The input state of one player: the map, raw device state, and action states per
/// simulation tick and per rendered frame.
///
/// ```
/// use engine_input::{Binding, EventKind, Input, InputEvent, InputMap, Key};
/// use std::time::{Duration, Instant};
///
/// let mut map = InputMap::new();
/// let jump = map.add_button("jump").unwrap();
/// map.bind(jump, Binding::button(Key::Space)).unwrap();
/// let mut input = Input::new(map);
///
/// let t0 = Instant::now();
/// input.sample_tick(t0);
/// // A tap shorter than a tick is not lost.
/// input.handle(&InputEvent { time: t0 + Duration::from_millis(2), kind: EventKind::Key { key: Key::Space, pressed: true } });
/// input.handle(&InputEvent { time: t0 + Duration::from_millis(5), kind: EventKind::Key { key: Key::Space, pressed: false } });
/// let tick = input.sample_tick(t0 + Duration::from_millis(16));
/// assert!(tick.just_pressed(jump));
/// assert!(input.tick().pressed("jump"));
/// let tick = input.sample_tick(t0 + Duration::from_millis(32));
/// assert!(tick.just_released(jump));
/// ```
pub struct Input {
    map: InputMap,
    /// Enabled contexts, highest priority first.
    order: Vec<u16>,
    /// Per context: binding indices, chords (most modifiers) first.
    context_bindings: Vec<Vec<u32>>,
    /// Per context: every slot its bindings read.
    context_slots: Vec<SlotBits>,
    /// Per binding: the slots it reads.
    binding_slots: Vec<SlotBits>,
    filter: DeviceFilter,
    down: RawBits,
    axes: [[f32; GAMEPAD_AXIS_COUNT]; MAX_GAMEPADS],
    tick: Interval,
    frame: Interval,
    tick_state: ActionState,
    frame_state: ActionState,
}

impl core::fmt::Debug for Input {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Input")
            .field("actions", &self.map.action_count())
            .field("filter", &self.filter)
            .finish()
    }
}

/// An action, by id or by name (Godot style).
pub trait ActionRef {
    /// The action's id in `map`.
    fn resolve(&self, map: &InputMap) -> Option<ActionId>;
}

impl ActionRef for ActionId {
    #[inline]
    fn resolve(&self, _: &InputMap) -> Option<ActionId> {
        Some(*self)
    }
}

impl ActionRef for &str {
    #[inline]
    fn resolve(&self, map: &InputMap) -> Option<ActionId> {
        map.action(self)
    }
}

/// Read access to one view's actions, by id or by name. Unknown names read as
/// released.
#[derive(Clone, Copy)]
pub struct Actions<'a> {
    map: &'a InputMap,
    state: &'a ActionState,
}

impl core::fmt::Debug for Actions<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Actions").finish_non_exhaustive()
    }
}

impl Actions<'_> {
    /// See [`ActionState::pressed`].
    pub fn pressed(&self, action: impl ActionRef) -> bool {
        action
            .resolve(self.map)
            .is_some_and(|a| self.state.pressed(a))
    }

    /// See [`ActionState::just_pressed`].
    pub fn just_pressed(&self, action: impl ActionRef) -> bool {
        action
            .resolve(self.map)
            .is_some_and(|a| self.state.just_pressed(a))
    }

    /// See [`ActionState::just_released`].
    pub fn just_released(&self, action: impl ActionRef) -> bool {
        action
            .resolve(self.map)
            .is_some_and(|a| self.state.just_released(a))
    }

    /// See [`ActionState::strength`].
    pub fn strength(&self, action: impl ActionRef) -> f32 {
        action
            .resolve(self.map)
            .map_or(0.0, |a| self.state.strength(a))
    }

    /// See [`ActionState::axis`].
    pub fn axis(&self, action: impl ActionRef) -> f32 {
        self.strength(action)
    }

    /// See [`ActionState::vector`].
    pub fn vector(&self, action: impl ActionRef) -> [f32; 2] {
        action
            .resolve(self.map)
            .map_or([0.0; 2], |a| self.state.vector(a))
    }

    /// The underlying state.
    pub fn state(&self) -> &ActionState {
        self.state
    }
}

impl Input {
    /// An input state for `map`, listening to every device.
    pub fn new(map: InputMap) -> Self {
        let mut input = Self {
            map,
            order: Vec::new(),
            context_bindings: Vec::new(),
            context_slots: Vec::new(),
            binding_slots: Vec::new(),
            filter: DeviceFilter::ALL,
            down: RawBits::default(),
            axes: [[0.0; GAMEPAD_AXIS_COUNT]; MAX_GAMEPADS],
            tick: Interval::new(),
            frame: Interval::new(),
            tick_state: ActionState::default(),
            frame_state: ActionState::default(),
        };
        input.rebuild();
        input
    }

    /// The input map.
    pub fn map(&self) -> &InputMap {
        &self.map
    }

    /// Changes the map (rebinding, new actions) and rebuilds the lookup tables.
    pub fn edit_map<R>(&mut self, f: impl FnOnce(&mut InputMap) -> R) -> R {
        let r = f(&mut self.map);
        self.rebuild();
        r
    }

    fn rebuild(&mut self) {
        let contexts = self.map.contexts.len();
        self.context_bindings = vec![Vec::new(); contexts];
        self.context_slots = vec![SlotBits::default(); contexts];
        self.binding_slots = vec![SlotBits::default(); self.map.bindings.len()];
        for (i, b) in self.map.bindings.iter().enumerate() {
            let c = b.context.0 as usize;
            self.context_bindings[c].push(i as u32);
            binding_slots(&b.binding, &mut self.binding_slots[i]);
            let slots = self.binding_slots[i];
            self.context_slots[c].or(&slots);
        }
        for list in &mut self.context_bindings {
            let bindings = &self.map.bindings;
            list.sort_by_key(|&i| match bindings[i as usize].binding {
                Binding::Button { modifiers, .. } => core::cmp::Reverse(modifiers.len()),
                _ => core::cmp::Reverse(0),
            });
        }
        self.order.reserve(contexts);
        self.update_order();
        let actions = self.map.action_count();
        self.tick_state.resize(actions);
        self.frame_state.resize(actions);
    }

    /// Enabled contexts, highest priority first (ties in creation order). Allocation-free:
    /// `order` has room for every context.
    fn update_order(&mut self) {
        let contexts = &self.map.contexts;
        self.order.clear();
        self.order
            .extend((0..contexts.len() as u16).filter(|&c| contexts[c as usize].enabled));
        self.order
            .sort_unstable_by_key(|&c| (core::cmp::Reverse(contexts[c as usize].priority), c));
    }

    /// Enables or disables a context (a menu, a vehicle). Cheap enough to call every
    /// frame.
    pub fn set_context_enabled(&mut self, context: ContextId, enabled: bool) {
        if self.map.contexts[context.0 as usize].enabled != enabled {
            self.map.contexts[context.0 as usize].enabled = enabled;
            self.update_order();
        }
    }

    /// Whether `context` is enabled.
    pub fn context_enabled(&self, context: ContextId) -> bool {
        self.map.contexts[context.0 as usize].enabled
    }

    /// Which devices this input listens to.
    pub fn set_devices(&mut self, filter: DeviceFilter) {
        self.filter = filter;
    }

    /// Releases every key, button and stick: what the devices can no longer report once
    /// the window loses the focus (the player calls it then), so nothing stays held.
    /// Presses latched since the last sample are still seen by it.
    pub fn release_all(&mut self) {
        for bit in 0..RAW_BITS {
            self.down.set(bit, false);
        }
        self.axes = [[0.0; GAMEPAD_AXIS_COUNT]; MAX_GAMEPADS];
    }

    fn press(&mut self, bit: usize, pressed: bool, time: Instant) {
        if pressed && !self.down.get(bit) {
            self.tick.press(bit, time);
            self.frame.press(bit, time);
        }
        self.down.set(bit, pressed);
    }

    /// Applies one event. Call it for every event, in order, before sampling.
    pub fn handle(&mut self, event: &InputEvent) {
        match event.kind {
            EventKind::Key { key, pressed } if self.filter.keyboard_mouse => {
                self.press(key as usize, pressed, event.time);
            }
            EventKind::MouseButton { button, pressed } if self.filter.keyboard_mouse => {
                self.press(RAW_MOUSE + button as usize, pressed, event.time);
            }
            EventKind::MouseMotion { dx, dy } if self.filter.keyboard_mouse => {
                for view in [&mut self.tick, &mut self.frame] {
                    view.mouse[0] += dx;
                    view.mouse[1] += dy;
                }
            }
            EventKind::Wheel { y, .. } if self.filter.keyboard_mouse => {
                if y > 0.0 {
                    self.tick.press(RAW_WHEEL_UP, event.time);
                    self.frame.press(RAW_WHEEL_UP, event.time);
                } else if y < 0.0 {
                    self.tick.press(RAW_WHEEL_DOWN, event.time);
                    self.frame.press(RAW_WHEEL_DOWN, event.time);
                }
            }
            EventKind::GamepadButton {
                pad,
                button,
                pressed,
            } if self.pad_allowed(pad) => {
                self.press(
                    RAW_PAD + pad as usize * 16 + button as usize,
                    pressed,
                    event.time,
                );
            }
            EventKind::GamepadAxis { pad, axis, value } if self.pad_allowed(pad) => {
                self.axes[pad as usize][axis as usize] = value;
            }
            EventKind::GamepadConnection {
                pad,
                connected: false,
            } if (pad as usize) < MAX_GAMEPADS => {
                for b in 0..16 {
                    self.down.set(RAW_PAD + pad as usize * 16 + b, false);
                }
                self.axes[pad as usize] = [0.0; GAMEPAD_AXIS_COUNT];
            }
            _ => {}
        }
    }

    fn pad_allowed(&self, pad: u8) -> bool {
        (pad as usize) < MAX_GAMEPADS && self.filter.gamepads & (1 << pad) != 0
    }

    /// The strongest value of `axis` over the allowed pads.
    fn axis_value(&self, axis: GamepadAxis) -> f32 {
        let mut best = 0.0f32;
        for pad in 0..MAX_GAMEPADS {
            if self.filter.gamepads & (1 << pad) != 0 {
                let v = self.axes[pad][axis as usize];
                if v.abs() > best.abs() {
                    best = v;
                }
            }
        }
        best
    }

    /// Whether `source` was down during the interval, its strength, and its first press
    /// time in the interval.
    fn source(
        &self,
        view: &Interval,
        source: ButtonSource,
        deadzone: f32,
    ) -> (bool, f32, Option<Instant>) {
        let raw = |bit: usize| -> (bool, f32, Option<Instant>) {
            let down = self.down.get(bit) || view.latched.get(bit);
            (down, if down { 1.0 } else { 0.0 }, view.press_time[bit])
        };
        match source {
            ButtonSource::Key(k) => raw(k as usize),
            ButtonSource::Mouse(b) => raw(RAW_MOUSE + b as usize),
            ButtonSource::Gamepad(b) => {
                let mut result = (false, 0.0, None);
                for pad in 0..MAX_GAMEPADS {
                    if self.filter.gamepads & (1 << pad) != 0 {
                        let (d, s, t) = raw(RAW_PAD + pad * 16 + b as usize);
                        if d {
                            result = (true, s, earliest(result.2, t));
                        }
                    }
                }
                result
            }
            ButtonSource::AxisPositive(a) => {
                let v = deadzone1(self.axis_value(a).max(0.0), deadzone);
                (v > 0.0, v, None)
            }
            ButtonSource::AxisNegative(a) => {
                let v = deadzone1((-self.axis_value(a)).max(0.0), deadzone);
                (v > 0.0, v, None)
            }
            ButtonSource::WheelUp => raw(RAW_WHEEL_UP),
            ButtonSource::WheelDown => raw(RAW_WHEEL_DOWN),
        }
    }

    fn evaluate(&mut self, frame: bool, now: Instant) {
        let (view, mut state) = if frame {
            (&self.frame, core::mem::take(&mut self.frame_state))
        } else {
            (&self.tick, core::mem::take(&mut self.tick_state))
        };
        state.begin();
        let start = view.last_sample.unwrap_or(now);
        let span = now.saturating_duration_since(start).as_secs_f64();
        let mut claimed = SlotBits::default();
        let mut chorded = SlotBits::default();
        let actions = self.map.action_count();
        for &context in &self.order {
            let c = context as usize;
            for &index in &self.context_bindings[c] {
                let bound = &self.map.bindings[index as usize];
                let slots = &self.binding_slots[index as usize];
                if slots.0.iter().zip(claimed.0).any(|(a, b)| a & b != 0) {
                    continue;
                }
                let a = bound.action.index();
                let info = &self.map.actions[a];
                let deadzone = info.deadzone;
                let (active, x, y, time) = match bound.binding {
                    Binding::Button { source, modifiers } => {
                        if modifiers.is_empty() && chorded.get(slot(source)) {
                            continue;
                        }
                        let held = modifiers
                            .keys()
                            .all(|k| self.source(view, k.into(), deadzone).0);
                        let (down, strength, time) = self.source(view, source, deadzone);
                        let active = held && down;
                        if active && !modifiers.is_empty() {
                            chorded.set(slot(source), true);
                        }
                        (active, if active { strength } else { 0.0 }, 0.0, time)
                    }
                    Binding::Buttons1d { negative, positive } => {
                        let (_, n, tn) = self.source(view, negative, deadzone);
                        let (_, p, tp) = self.source(view, positive, deadzone);
                        (p != n, p - n, 0.0, earliest(tn, tp))
                    }
                    Binding::Axis { axis, scale } => {
                        let v = deadzone1(self.axis_value(axis), deadzone) * scale;
                        (v != 0.0, v, 0.0, None)
                    }
                    Binding::Buttons2d {
                        up,
                        down,
                        left,
                        right,
                    } => {
                        let (_, u, tu) = self.source(view, up, deadzone);
                        let (_, d, td) = self.source(view, down, deadzone);
                        let (_, l, tl) = self.source(view, left, deadzone);
                        let (_, r, tr) = self.source(view, right, deadzone);
                        let (x, y) = (r - l, u - d);
                        let len = (x * x + y * y).sqrt();
                        let (x, y) = if len > 1.0 {
                            (x / len, y / len)
                        } else {
                            (x, y)
                        };
                        let time = earliest(earliest(tu, td), earliest(tl, tr));
                        (len > 0.0, x, y, time)
                    }
                    Binding::Stick { stick, scale } => {
                        let (ax, ay) = stick.axes();
                        let [x, y] = deadzone2(self.axis_value(ax), self.axis_value(ay), deadzone);
                        (x != 0.0 || y != 0.0, x * scale, y * scale, None)
                    }
                    Binding::MouseMotion { scale } => {
                        let (x, y) = (view.mouse[0] * scale, -view.mouse[1] * scale);
                        (x != 0.0 || y != 0.0, x, y, None)
                    }
                };
                if !active {
                    continue;
                }
                let [vx, vy] = state.raw_value(a);
                let (vx, vy) = (vx as f32 / VALUE_ONE as f32, vy as f32 / VALUE_ONE as f32);
                let value = match info.kind {
                    ActionKind::Button => [vx.max(x), 0.0],
                    ActionKind::Axis1d => [vx + x, 0.0],
                    ActionKind::Axis2d => [vx + x, vy + y],
                };
                state.set_value(a, [quantize(value[0]), quantize(value[1])]);
                state.set_pressed(a, true);
                if let Some(t) = time
                    && span > 0.0
                {
                    let f =
                        (t.saturating_duration_since(start).as_secs_f64() / span).clamp(0.0, 1.0);
                    let f = (f * 65534.0).round() as u16;
                    let old = state.raw_fraction(a);
                    state.set_fraction(a, if old == NO_FRACTION { f } else { old.min(f) });
                }
            }
            if self.map.contexts[c].blocks_lower {
                claimed.or(&self.context_slots[c]);
            }
        }
        // Clamp and requantize the combined values; recompute "pressed" for axes that
        // cancelled out.
        for a in 0..actions {
            let kind = self.map.actions[a].kind;
            let [x, y] = state.raw_value(a);
            let (x, y) = match kind {
                ActionKind::Button => (x.clamp(0, VALUE_ONE), 0),
                ActionKind::Axis1d => (x.clamp(-VALUE_ONE, VALUE_ONE), 0),
                ActionKind::Axis2d => (x, y),
            };
            state.set_value(a, [x, y]);
            if kind != ActionKind::Button && x == 0 && y == 0 {
                state.set_pressed(a, false);
            }
        }
        if frame {
            self.frame_state = state;
            self.frame.reset(now);
        } else {
            self.tick_state = state;
            self.tick.reset(now);
        }
    }

    /// Samples the simulation view at the end of a tick: everything that happened since
    /// the previous tick sample. Call once per fixed tick.
    pub fn sample_tick(&mut self, now: Instant) -> &ActionState {
        self.evaluate(false, now);
        &self.tick_state
    }

    /// Samples the per-frame view (UI, camera smoothing): everything since the previous
    /// frame sample. Independent of [`Input::sample_tick`].
    pub fn sample_frame(&mut self, now: Instant) -> &ActionState {
        self.evaluate(true, now);
        &self.frame_state
    }

    /// The last tick sample.
    pub fn tick(&self) -> Actions<'_> {
        Actions {
            map: &self.map,
            state: &self.tick_state,
        }
    }

    /// The last frame sample.
    pub fn frame(&self) -> Actions<'_> {
        Actions {
            map: &self.map,
            state: &self.frame_state,
        }
    }

    /// Whether a key is held right now (for text fields and debug keys; game code should
    /// use actions).
    pub fn key_down(&self, key: crate::Key) -> bool {
        self.down.get(key as usize)
    }

    /// Whether a mouse button is held right now.
    pub fn mouse_down(&self, button: MouseButton) -> bool {
        self.down.get(RAW_MOUSE + button as usize)
    }
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}
