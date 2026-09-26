//! The input map: named actions, contexts and bindings.

use crate::device::{GamepadAxis, GamepadButton, Key, MouseButton, Stick};
use std::collections::HashMap;

/// Index of an action in its [`InputMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionId(pub(crate) u16);

impl ActionId {
    /// The raw index (actions are numbered in creation order).
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index of a context in its [`InputMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextId(pub(crate) u16);

impl ContextId {
    /// The context every map starts with (priority 0, enabled).
    pub const DEFAULT: ContextId = ContextId(0);
}

/// What an action's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Pressed or not, with a strength in 0..=1 (analog triggers, stick directions).
    Button,
    /// A value in −1..=1 (a negative and a positive button, or an analog axis).
    Axis1d,
    /// A 2D vector (four buttons, a stick, or mouse motion).
    Axis2d,
}

/// A digital or directional input that can act as a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ButtonSource {
    /// A keyboard key.
    Key(Key),
    /// A mouse button.
    Mouse(MouseButton),
    /// A gamepad button.
    Gamepad(GamepadButton),
    /// A gamepad axis pushed past the action's deadzone in one direction (triggers,
    /// stick directions).
    AxisPositive(GamepadAxis),
    /// A gamepad axis pushed past the deadzone in the negative direction.
    AxisNegative(GamepadAxis),
    /// A wheel notch up (away from the user).
    WheelUp,
    /// A wheel notch down.
    WheelDown,
}

impl From<Key> for ButtonSource {
    fn from(k: Key) -> Self {
        Self::Key(k)
    }
}

impl From<MouseButton> for ButtonSource {
    fn from(b: MouseButton) -> Self {
        Self::Mouse(b)
    }
}

impl From<GamepadButton> for ButtonSource {
    fn from(b: GamepadButton) -> Self {
        Self::Gamepad(b)
    }
}

/// Up to three keys that must be held for a binding to fire (`Ctrl+Shift+S`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    keys: [Option<Key>; 3],
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers { keys: [None; 3] };

    /// The given keys (at most three are kept).
    pub fn of(keys: &[Key]) -> Self {
        let mut m = Self::NONE;
        for (slot, &k) in m.keys.iter_mut().zip(keys) {
            *slot = Some(k);
        }
        m
    }

    /// The modifier keys.
    pub fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.keys.iter().flatten().copied()
    }

    /// Number of modifier keys.
    pub fn len(&self) -> usize {
        self.keys.iter().flatten().count()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How an input drives an action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Binding {
    /// A button source, optionally with modifier keys held. Drives a `Button`, or an
    /// `Axis1d` in the positive direction.
    Button {
        /// The input.
        source: ButtonSource,
        /// Keys that must be held too.
        modifiers: Modifiers,
    },
    /// Two buttons for an `Axis1d` (Godot's `get_axis`).
    Buttons1d {
        /// Gives −1.
        negative: ButtonSource,
        /// Gives +1.
        positive: ButtonSource,
    },
    /// An analog gamepad axis for an `Axis1d`, with the action's deadzone.
    Axis {
        /// The axis.
        axis: GamepadAxis,
        /// Multiplier (negative inverts).
        scale: f32,
    },
    /// Four buttons for an `Axis2d` (WASD, arrows, D-pad), normalized so diagonals are
    /// not faster.
    Buttons2d {
        /// +Y.
        up: ButtonSource,
        /// −Y.
        down: ButtonSource,
        /// −X.
        left: ButtonSource,
        /// +X.
        right: ButtonSource,
    },
    /// A stick for an `Axis2d`, with a circular deadzone (Godot's `get_vector`).
    Stick {
        /// Which stick.
        stick: Stick,
        /// Multiplier on both axes.
        scale: f32,
    },
    /// Mouse motion for an `Axis2d`, in counts × `scale` per tick (not per second).
    MouseMotion {
        /// Multiplier (sensitivity); +Y is up.
        scale: f32,
    },
}

impl Binding {
    /// A button binding without modifiers.
    pub fn button(source: impl Into<ButtonSource>) -> Self {
        Self::Button {
            source: source.into(),
            modifiers: Modifiers::NONE,
        }
    }

    /// A chord: `source` while all `modifiers` are held.
    pub fn chord(modifiers: &[Key], source: impl Into<ButtonSource>) -> Self {
        Self::Button {
            source: source.into(),
            modifiers: Modifiers::of(modifiers),
        }
    }

    /// W, S, A, D as a 2D axis.
    pub fn wasd() -> Self {
        Self::Buttons2d {
            up: Key::KeyW.into(),
            down: Key::KeyS.into(),
            left: Key::KeyA.into(),
            right: Key::KeyD.into(),
        }
    }

    /// The arrow keys as a 2D axis.
    pub fn arrows() -> Self {
        Self::Buttons2d {
            up: Key::ArrowUp.into(),
            down: Key::ArrowDown.into(),
            left: Key::ArrowLeft.into(),
            right: Key::ArrowRight.into(),
        }
    }

    /// The D-pad as a 2D axis.
    pub fn dpad() -> Self {
        Self::Buttons2d {
            up: GamepadButton::DPadUp.into(),
            down: GamepadButton::DPadDown.into(),
            left: GamepadButton::DPadLeft.into(),
            right: GamepadButton::DPadRight.into(),
        }
    }

    /// A stick at scale 1.
    pub fn stick(stick: Stick) -> Self {
        Self::Stick { stick, scale: 1.0 }
    }

    /// Mouse motion with a sensitivity.
    pub fn mouse_motion(scale: f32) -> Self {
        Self::MouseMotion { scale }
    }

    fn fits(&self, kind: ActionKind) -> bool {
        match self {
            Binding::Button { .. } => matches!(kind, ActionKind::Button | ActionKind::Axis1d),
            Binding::Buttons1d { .. } | Binding::Axis { .. } => kind == ActionKind::Axis1d,
            Binding::Buttons2d { .. } | Binding::Stick { .. } | Binding::MouseMotion { .. } => {
                kind == ActionKind::Axis2d
            }
        }
    }

    /// The button sources this binding reads (for conflicts and context blocking).
    pub(crate) fn sources(&self, out: &mut Vec<ButtonSource>) {
        match *self {
            Binding::Button { source, .. } => out.push(source),
            Binding::Buttons1d { negative, positive } => out.extend([negative, positive]),
            Binding::Buttons2d {
                up,
                down,
                left,
                right,
            } => out.extend([up, down, left, right]),
            Binding::Axis { .. } | Binding::Stick { .. } | Binding::MouseMotion { .. } => {}
        }
    }
}

/// Errors from building or changing an input map.
#[derive(Debug, Clone, PartialEq)]
pub enum MapError {
    /// An action with this name exists.
    DuplicateAction(String),
    /// A context with this name exists.
    DuplicateContext(String),
    /// The binding cannot drive an action of this kind (for example a stick for a button).
    WrongKind {
        /// The action.
        action: String,
        /// Its kind.
        kind: ActionKind,
    },
    /// The same input (with the same modifiers) already drives another action in this
    /// context.
    Conflict {
        /// The action that already uses it.
        existing: String,
        /// The input.
        source: ButtonSource,
    },
    /// No such binding index.
    NoSuchBinding,
}

impl core::fmt::Display for MapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DuplicateAction(n) => write!(f, "action `{n}` already exists"),
            Self::DuplicateContext(n) => write!(f, "context `{n}` already exists"),
            Self::WrongKind { action, kind } => {
                write!(f, "this binding cannot drive `{action}` ({kind:?})")
            }
            Self::Conflict { existing, source } => {
                write!(f, "{source:?} already drives `{existing}` in this context")
            }
            Self::NoSuchBinding => write!(f, "no such binding"),
        }
    }
}

impl std::error::Error for MapError {}

#[derive(Debug, Clone)]
pub(crate) struct Action {
    pub(crate) name: String,
    pub(crate) kind: ActionKind,
    pub(crate) deadzone: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct Context {
    pub(crate) name: String,
    pub(crate) priority: i32,
    pub(crate) enabled: bool,
    /// Inputs bound here are hidden from lower-priority contexts.
    pub(crate) blocks_lower: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundInput {
    pub(crate) action: ActionId,
    pub(crate) context: ContextId,
    pub(crate) binding: Binding,
}

/// Actions, contexts and bindings. Build it once (or load it), then hand it to
/// [`Input`](crate::Input).
///
/// ```
/// use engine_input::{Binding, GamepadButton, InputMap, Key, Stick};
///
/// let mut map = InputMap::new();
/// let jump = map.add_button("jump").unwrap();
/// map.bind(jump, Binding::button(Key::Space)).unwrap();
/// map.bind(jump, Binding::button(GamepadButton::South)).unwrap();
/// let walk = map.add_axis2d("move").unwrap();
/// map.bind(walk, Binding::wasd()).unwrap();
/// map.bind(walk, Binding::stick(Stick::Left)).unwrap();
/// assert_eq!(map.action("jump"), Some(jump));
/// ```
#[derive(Debug, Clone)]
pub struct InputMap {
    pub(crate) actions: Vec<Action>,
    pub(crate) contexts: Vec<Context>,
    pub(crate) bindings: Vec<BoundInput>,
    by_name: HashMap<String, ActionId>,
    current: ContextId,
}

impl Default for InputMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Default deadzone for analog inputs, as Godot's.
pub const DEFAULT_DEADZONE: f32 = 0.2;

impl InputMap {
    /// An empty map with the default context (priority 0, enabled).
    pub fn new() -> Self {
        Self {
            actions: Vec::new(),
            contexts: vec![Context {
                name: "default".into(),
                priority: 0,
                enabled: true,
                blocks_lower: true,
            }],
            bindings: Vec::new(),
            by_name: HashMap::new(),
            current: ContextId::DEFAULT,
        }
    }

    fn add(&mut self, name: &str, kind: ActionKind) -> Result<ActionId, MapError> {
        if self.by_name.contains_key(name) {
            return Err(MapError::DuplicateAction(name.into()));
        }
        let id = ActionId(self.actions.len() as u16);
        self.actions.push(Action {
            name: name.into(),
            kind,
            deadzone: DEFAULT_DEADZONE,
        });
        self.by_name.insert(name.into(), id);
        Ok(id)
    }

    /// Adds a button action.
    pub fn add_button(&mut self, name: &str) -> Result<ActionId, MapError> {
        self.add(name, ActionKind::Button)
    }

    /// Adds a one-dimensional axis action.
    pub fn add_axis1d(&mut self, name: &str) -> Result<ActionId, MapError> {
        self.add(name, ActionKind::Axis1d)
    }

    /// Adds a two-dimensional axis action.
    pub fn add_axis2d(&mut self, name: &str) -> Result<ActionId, MapError> {
        self.add(name, ActionKind::Axis2d)
    }

    /// Adds a context. Higher priorities see inputs first; inputs a context binds are
    /// hidden from lower ones while it is enabled (unless `blocks_lower` is off).
    /// New contexts start disabled. Later [`InputMap::bind`] calls go to it.
    pub fn add_context(&mut self, name: &str, priority: i32) -> Result<ContextId, MapError> {
        if self.contexts.iter().any(|c| c.name == name) {
            return Err(MapError::DuplicateContext(name.into()));
        }
        let id = ContextId(self.contexts.len() as u16);
        self.contexts.push(Context {
            name: name.into(),
            priority,
            enabled: false,
            blocks_lower: true,
        });
        self.current = id;
        Ok(id)
    }

    /// Makes later [`InputMap::bind`] calls add to `context`.
    pub fn use_context(&mut self, context: ContextId) {
        self.current = context;
    }

    /// Whether inputs bound in `context` are hidden from lower-priority contexts
    /// (default `true`).
    pub fn set_blocks_lower(&mut self, context: ContextId, blocks: bool) {
        self.contexts[context.0 as usize].blocks_lower = blocks;
    }

    /// Whether `context` starts enabled.
    pub fn set_enabled(&mut self, context: ContextId, enabled: bool) {
        self.contexts[context.0 as usize].enabled = enabled;
    }

    /// Deadzone for analog inputs of `action` (default [`DEFAULT_DEADZONE`]).
    pub fn set_deadzone(&mut self, action: ActionId, deadzone: f32) {
        self.actions[action.index()].deadzone = deadzone.clamp(0.0, 0.99);
    }

    /// Binds an input to `action` in the current context.
    pub fn bind(&mut self, action: ActionId, binding: Binding) -> Result<(), MapError> {
        let context = self.current;
        self.bind_in(context, action, binding)
    }

    /// Binds an input to `action` in `context`. Fails if the binding does not fit the
    /// action's kind, or if the same input with the same modifiers already drives
    /// another action in that context.
    pub fn bind_in(
        &mut self,
        context: ContextId,
        action: ActionId,
        binding: Binding,
    ) -> Result<(), MapError> {
        self.check(context, action, &binding, None)?;
        self.bindings.push(BoundInput {
            action,
            context,
            binding,
        });
        Ok(())
    }

    fn check(
        &self,
        context: ContextId,
        action: ActionId,
        binding: &Binding,
        skip: Option<usize>,
    ) -> Result<(), MapError> {
        let info = &self.actions[action.index()];
        if !binding.fits(info.kind) {
            return Err(MapError::WrongKind {
                action: info.name.clone(),
                kind: info.kind,
            });
        }
        let modifiers = match binding {
            Binding::Button { modifiers, .. } => *modifiers,
            _ => Modifiers::NONE,
        };
        let mut new = Vec::new();
        binding.sources(&mut new);
        let mut old = Vec::new();
        for (i, b) in self.bindings.iter().enumerate() {
            if Some(i) == skip || b.context != context || b.action == action {
                continue;
            }
            let other_modifiers = match b.binding {
                Binding::Button { modifiers, .. } => modifiers,
                _ => Modifiers::NONE,
            };
            if other_modifiers != modifiers {
                continue;
            }
            old.clear();
            b.binding.sources(&mut old);
            if let Some(&source) = new.iter().find(|s| old.contains(s)) {
                return Err(MapError::Conflict {
                    existing: self.actions[b.action.index()].name.clone(),
                    source,
                });
            }
        }
        Ok(())
    }

    /// Bindings of `action`, in all contexts: `(context, binding)`.
    pub fn bindings(&self, action: ActionId) -> impl Iterator<Item = (ContextId, &Binding)> {
        self.bindings
            .iter()
            .filter(move |b| b.action == action)
            .map(|b| (b.context, &b.binding))
    }

    /// Replaces the `index`-th binding of `action` (in [`InputMap::bindings`] order),
    /// for rebinding menus. Conflicts are reported, not resolved.
    pub fn rebind(
        &mut self,
        action: ActionId,
        index: usize,
        binding: Binding,
    ) -> Result<(), MapError> {
        let (slot, context) = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.action == action)
            .nth(index)
            .map(|(i, b)| (i, b.context))
            .ok_or(MapError::NoSuchBinding)?;
        self.check(context, action, &binding, Some(slot))?;
        self.bindings[slot].binding = binding;
        Ok(())
    }

    /// Removes the `index`-th binding of `action`.
    pub fn unbind(&mut self, action: ActionId, index: usize) -> Result<(), MapError> {
        let slot = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.action == action)
            .nth(index)
            .map(|(i, _)| i)
            .ok_or(MapError::NoSuchBinding)?;
        self.bindings.remove(slot);
        Ok(())
    }

    /// The action called `name`.
    pub fn action(&self, name: &str) -> Option<ActionId> {
        self.by_name.get(name).copied()
    }

    /// Name of `action`.
    pub fn action_name(&self, action: ActionId) -> &str {
        &self.actions[action.index()].name
    }

    /// Kind of `action`.
    pub fn action_kind(&self, action: ActionId) -> ActionKind {
        self.actions[action.index()].kind
    }

    /// Number of actions.
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// The context called `name`.
    pub fn context(&self, name: &str) -> Option<ContextId> {
        self.contexts
            .iter()
            .position(|c| c.name == name)
            .map(|i| ContextId(i as u16))
    }
}
