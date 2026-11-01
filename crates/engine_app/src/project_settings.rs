//! Project settings (ADR 0127): a file per area in the project's `settings/` folder, each
//! one value in the scenes' grammar. S1: the input map, `settings/input.settings`:
//!
//! ```text
//! [input version=1 id=…]
//!
//! contexts["menu"].priority = 10
//! actions["jump"].kind = Button
//! actions["jump"].bindings["default"] = ["Space", "Pad.South"]
//! actions["move"].kind = Axis2d
//! actions["move"].bindings["default"] = ["KeyW, KeyS, KeyA, KeyD", "Stick.Left"]
//! actions["pause"].bindings["menu"] = ["Escape"]
//! ```
//!
//! A binding is written as `engine_input::Binding::to_text` writes it. The context
//! `default` always exists (priority 0, enabled); others start disabled unless they say.
//! A game that sets `PlayerSettings::input` in code keeps its own map: the file sets only
//! what code left unset.

use std::collections::BTreeMap;
use std::path::Path;

use engine_reflect::Reflect;
use engine_scene::{AssetId, LoadError, ValueReport};

/// The project's settings folder.
pub const SETTINGS_FOLDER: &str = "settings";

/// What an action's value is (`engine_input::ActionKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum ActionKindSetting {
    /// Pressed or not.
    #[default]
    Button = 0,
    /// A value in −1..=1.
    Axis1d = 1,
    /// A 2D vector.
    Axis2d = 2,
}

/// A context of the input map: a set of bindings switched on and off as one (a menu over
/// the game).
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct ContextSettings {
    /// Higher priorities see inputs first.
    pub priority: i32,
    /// Whether it starts switched on.
    pub enabled: bool,
    /// Whether inputs it binds are hidden from lower priorities while it is on.
    pub blocks_lower: bool,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            priority: 0,
            enabled: false,
            blocks_lower: true,
        }
    }
}

/// An action: its kind, deadzone, and its bindings by context.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct ActionSettings {
    /// What its value is.
    pub kind: ActionKindSetting,
    /// Analog inputs below this count as nothing.
    #[reflect(min = 0.0, max = 1.0)]
    pub deadzone: f32,
    /// Its bindings as text, by the context they are in.
    pub bindings: BTreeMap<String, Vec<String>>,
}

impl Default for ActionSettings {
    fn default() -> Self {
        Self {
            kind: ActionKindSetting::Button,
            deadzone: engine_input::DEFAULT_DEADZONE,
            bindings: BTreeMap::new(),
        }
    }
}

/// The input map's file: `settings/input.settings`.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct InputSettings {
    /// Its contexts besides `default`, by name.
    pub contexts: BTreeMap<String, ContextSettings>,
    /// Its actions, by name.
    pub actions: BTreeMap<String, ActionSettings>,
}

/// The lines a default `T` writes (without the header), to leave out of a file.
fn default_lines<T: Reflect + Default>() -> Vec<String> {
    let mut all = String::new();
    engine_scene::write_value("x", 1, None, &T::default(), None, &mut all);
    all.lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

impl InputSettings {
    /// The kind in the file's header.
    pub const KIND: &'static str = "input";
    /// The format's version.
    pub const VERSION: u32 = 1;
    /// The file's path under the project.
    pub const PATH: &'static str = "settings/input.settings";

    /// The file's text with its id; lines at their defaults left out (an action's kind
    /// always written).
    pub fn to_text(&self, id: AssetId) -> String {
        let mut all = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut all);
        let action = default_lines::<ActionSettings>();
        let context = default_lines::<ContextSettings>();
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let field = |prefix: &str| {
                line.strip_prefix(prefix)
                    .and_then(|rest| rest.split_once("\"]."))
                    .map(|(_, field)| field)
            };
            let default = matches!(line, "contexts = {}" | "actions = {}")
                || field("actions[\"")
                    .is_some_and(|f| !f.starts_with("kind ") && action.iter().any(|d| d == f))
                || field("contexts[\"").is_some_and(|f| context.iter().any(|d| d == f));
            if !default {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// The settings from their text, and what reading it reported.
    ///
    /// # Errors
    ///
    /// Not an input file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// The project at `root`'s input settings, if it has the file.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or parsed (with its path).
    pub fn load(root: &Path) -> Result<Option<Self>, String> {
        let path = root.join(Self::PATH);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text)
                .map(|(file, _)| Some(file))
                .map_err(|e| format!("{}:{e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// The input map these settings make, and what could not go into it (a binding that
    /// names no input or does not fit its action, a context that does not exist), each
    /// left out.
    pub fn to_map(&self) -> (engine_input::InputMap, Vec<String>) {
        use engine_input::{Binding, ContextId, InputMap};
        let mut map = InputMap::new();
        let mut problems = Vec::new();
        let mut contexts: Vec<(&String, &ContextSettings)> = self
            .contexts
            .iter()
            .filter(|(name, _)| name.as_str() != "default")
            .collect();
        contexts.sort_by_key(|(name, c)| (c.priority, name.as_str()));
        for (name, c) in contexts {
            match map.add_context(name, c.priority) {
                Ok(id) => {
                    map.set_enabled(id, c.enabled);
                    map.set_blocks_lower(id, c.blocks_lower);
                }
                Err(e) => problems.push(format!("context {name}: {e}")),
            }
        }
        map.use_context(ContextId::DEFAULT);
        for (name, a) in &self.actions {
            let added = match a.kind {
                ActionKindSetting::Button => map.add_button(name),
                ActionKindSetting::Axis1d => map.add_axis1d(name),
                ActionKindSetting::Axis2d => map.add_axis2d(name),
            };
            let action = match added {
                Ok(id) => id,
                Err(e) => {
                    problems.push(format!("action {name}: {e}"));
                    continue;
                }
            };
            map.set_deadzone(action, a.deadzone);
            for (context, list) in &a.bindings {
                let id = if context == "default" {
                    Some(ContextId::DEFAULT)
                } else {
                    map.context(context)
                };
                let Some(id) = id else {
                    problems.push(format!("action {name}: no context {context}"));
                    continue;
                };
                for text in list {
                    match Binding::parse(text) {
                        Ok(b) => {
                            if let Err(e) = map.bind_in(id, action, b) {
                                problems.push(format!("action {name}: {text}: {e}"));
                            }
                        }
                        Err(e) => problems.push(format!("action {name}: {e}")),
                    }
                }
            }
        }
        (map, problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InputSettings {
        let mut s = InputSettings::default();
        s.contexts.insert(
            String::from("menu"),
            ContextSettings {
                priority: 10,
                ..ContextSettings::default()
            },
        );
        let mut jump = ActionSettings::default();
        jump.bindings.insert(
            String::from("default"),
            vec![String::from("Space"), String::from("Pad.South")],
        );
        s.actions.insert(String::from("jump"), jump);
        let mut movement = ActionSettings {
            kind: ActionKindSetting::Axis2d,
            ..ActionSettings::default()
        };
        movement.bindings.insert(
            String::from("default"),
            vec![
                String::from("KeyW, KeyS, KeyA, KeyD"),
                String::from("Stick.Left"),
            ],
        );
        s.actions.insert(String::from("move"), movement);
        let mut pause = ActionSettings::default();
        pause
            .bindings
            .insert(String::from("menu"), vec![String::from("Escape")]);
        s.actions.insert(String::from("pause"), pause);
        s
    }

    /// The file's text: defaults left out, kinds kept, read back the same.
    #[test]
    fn input_settings_round_trip() {
        let s = sample();
        let id = AssetId::new();
        let text = s.to_text(id);
        assert!(text.contains("actions[\"jump\"].kind = Button"), "{text}");
        assert!(!text.contains("deadzone"), "a default: {text}");
        assert!(!text.contains("blocks_lower"), "a default: {text}");
        assert!(text.contains("contexts[\"menu\"].priority = 10"), "{text}");
        let (read, report) = InputSettings::parse(&text).unwrap();
        assert_eq!(report.id, Some(id));
        assert_eq!(read, s);
    }

    /// The map they make: actions of their kinds with their bindings in their contexts;
    /// a bad binding left out and said.
    #[test]
    fn input_settings_make_the_map() {
        let mut s = sample();
        s.actions
            .get_mut("jump")
            .unwrap()
            .bindings
            .get_mut("default")
            .unwrap()
            .push(String::from("KeyNope"));
        let (map, problems) = s.to_map();
        let jump = map.action("jump").unwrap();
        let movement = map.action("move").unwrap();
        assert_eq!(map.action_kind(movement), engine_input::ActionKind::Axis2d);
        assert_eq!(map.bindings(jump).count(), 2);
        assert_eq!(map.bindings(movement).count(), 2);
        let menu = map.context("menu").unwrap();
        let pause = map.action("pause").unwrap();
        assert_eq!(map.bindings(pause).next().map(|(c, _)| c), Some(menu));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("KeyNope"), "{problems:?}");
    }
}
