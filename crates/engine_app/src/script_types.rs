//! The engine's scripting API as Luau definitions (ADR 0114 §6, E7-2): what the script host
//! installs ([`crate::scripts`]), every reflected component of the program and every script
//! component, written as `types/ofelia.d.luau` for luau-lsp and loaded by the editor's
//! checker.
//!
//! - **The syntax** is Luau 0.736's, the VM's release (`declare extern type … with … end`).
//!   luau-lsp calls its definition syntax unstable, so this follows each Luau update.
//! - **A hook's `self`** takes its type from `component`'s declaration: `component<P>`
//!   returns a table whose `on_tick` and the other hooks take `self: P & ScriptSelf`. Luau's
//!   old solver types an un-annotated `self` of `function Door.on_tick(self, dt)` from that;
//!   the new solver of 0.736 does not, so the editor checks scripts with the old one, as
//!   luau-lsp does by default.
//! - **Components** are types of their reflected fields, as views read them: numbers,
//!   booleans, `vector` (a `Vec3`, `Vec3A` or `Vec2`), `Quat`, `Entity?`, strings, an enum's
//!   variants by name, and structs of those. Fields of other kinds (lists, maps, options,
//!   asset references) are left out: scripts cannot read them yet.
//! - **The project's animation graphs** (ADR 0124's E12-5c) name what `Animator:set` and
//!   `Animator:trigger` take: their parameters' names as string choices ([`GraphNames`],
//!   [`definitions_with`]), so the checker completes them inside the quotes and underlines
//!   a misspelt one.

use engine_reflect::{Primitive, TypeInfo, TypeKind};
use engine_scene::{AnimGraphFile, ParamKind};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Names the definitions give their own types; a component of one of these names is left
/// out (it would shadow them).
const RESERVED: [&str; 12] = [
    "Entity",
    "Quat",
    "ScriptSelf",
    "ScriptComponent",
    "PropOptions",
    "PhysicsFilter",
    "ComponentType",
    "Query",
    "Script",
    "Asset",
    "AnimatorParam",
    "AnimatorTrigger",
];

/// The parameters' names of the project's animation graphs (`.agraph` files, ADR 0124's
/// E12-5c), each once and sorted: what the definitions' `AnimatorParam` (numbers, flags and
/// whole numbers: `Animator:set`) and `AnimatorTrigger` (`Animator:trigger`) choose from. A
/// name that is a trigger in one graph and a number in another is in both.
///
/// ```ignore
/// let mut names = GraphNames::default();
/// names.add_text(&std::fs::read_to_string("hero.agraph")?)?;
/// let types = engine_app::script_types::definitions_with(&names);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphNames {
    /// Numbers', flags' and whole numbers' names.
    pub params: BTreeSet<String>,
    /// Triggers' names.
    pub triggers: BTreeSet<String>,
}

impl GraphNames {
    /// Graph `graph`'s parameters' names added.
    pub fn add(&mut self, graph: &AnimGraphFile) {
        for (name, param) in &graph.params {
            let names = if param.kind == ParamKind::Trigger {
                &mut self.triggers
            } else {
                &mut self.params
            };
            if !names.contains(name) {
                names.insert(name.clone());
            }
        }
    }

    /// The parameters' names of the graph file whose text is `text` added: its header and
    /// its `params[…]` lines are read, not its nodes, states or transitions.
    ///
    /// # Errors
    ///
    /// Not a graph's file, a newer one, or a bad parameter line (with its number).
    pub fn add_text(&mut self, text: &str) -> Result<(), String> {
        // The other lines left empty, so a bad line keeps its number.
        let mut kept = String::with_capacity(4096);
        let mut header = true;
        for line in text.lines() {
            let s = line.trim();
            let content = !s.is_empty() && !s.starts_with('#');
            if content && (header || s.starts_with("params[")) {
                kept.push_str(s);
            }
            header &= !content;
            kept.push('\n');
        }
        let (graph, _) = AnimGraphFile::parse(&kept).map_err(|e| e.to_string())?;
        self.add(&graph);
        Ok(())
    }
}

/// The definitions' first part: what does not depend on the program's components.
const HEADER: &str = r#"-- types/ofelia.d.luau: the engine's scripting API (ADR 0114), made by the editor from
-- the program's components, the project's scripts and its animation graphs' parameters.
-- Do not edit: it is written again.

-- A rotation: `q * v` rotates a vector, `a * b` composes.
declare extern type Quat with
    x: number
    y: number
    z: number
    w: number
    __mul: ((self: Quat, other: Quat) -> Quat) & ((self: Quat, other: vector) -> vector)
end

-- What a message (`send`) or a timer (`after`) carries.
type Value = nil | boolean | number | string | vector | Entity

-- What `has`, `add` and `remove` take: `Transform` (each component of the program is a
-- global of its name), a script's `component` table, or a component's name.
export type ComponentType = { name: string }

-- An asset of the project (a prefab scene, a model): what `prop.asset` holds. `tostring`
-- gives its path once it is known.
declare extern type Asset with
end

-- What `prop.number` and the other helpers take after the default.
type PropOptions = {
    min: number?,
    max: number?,
    step: number?,
    tooltip: string?,
    hidden: boolean?,
    skip: boolean?,
    -- Once the game's network schema names the component: not sent, or sent in steps
    -- of this size.
    net_skip: boolean?,
    net_quantum: number?,
}
"#;

/// An entity's fields and methods (ADR 0114 §3).
const ENTITY_MEMBERS: &str = r#"    -- Its `Name`, if it has one.
    name: string?
    -- Whether it still exists.
    alive: boolean
    -- Its parent, if it has one.
    parent: Entity?
    -- Whether it has `component`.
    function has(self, component: ComponentType | string): boolean
    -- Gives it `component` (the default, then `values` written into it) unless it has one,
    -- at once.
    function add(self, component: ComponentType | string, values: { [string]: any }?): ()
    -- Takes `component` away, at once.
    function remove(self, component: ComponentType | string): ()
    -- Despawns it, with its children, at the end of the stage.
    function despawn(self): ()
    -- Its children in order, for a `for` loop.
    function children(self): ((Entity, Entity?) -> Entity?, Entity, nil)
    -- The nearest of its descendants named `name` (its children first), or nil.
    function find_child(self, name: string): Entity?
    -- Calls `hook` with these values (at most four) on each of its scripts that has it,
    -- after the hook that sends returns.
    function send(self, hook: string, ...: Value): ()
"#;

/// An entity's methods on `self` (`self:despawn()`).
const SELF_METHODS: &str = r#"    has: (self: any, component: ComponentType | string) -> boolean,
    add: (self: any, component: ComponentType | string, values: { [string]: any }?) -> (),
    remove: (self: any, component: ComponentType | string) -> (),
    despawn: (self: any) -> (),
    children: (self: any) -> ((Entity, Entity?) -> Entity?, Entity, nil),
    find_child: (self: any, name: string) -> Entity?,
    send: (self: any, hook: string, ...Value) -> (),
    -- Calls `hook` on this script with these values (at most four) once `seconds` of game
    -- time passed.
    after: (self: any, seconds: number, hook: string, ...Value) -> (),
"#;

/// `Transform`'s methods on its views.
const TRANSFORM_METHODS: &str = r#"    -- Turns it by `angle` radians about `axis`, in its parent's space.
    function rotate(self, axis: vector, angle: number): ()
    -- Turns it so that its forward (-z) points at `point`, in its parent's space, `up` (y by
    -- default) up.
    function look_at(self, point: vector, up: vector?): ()
    -- Where it faces: its -z, in its parent's space.
    function forward(self): vector
    -- Its x, in its parent's space.
    function right(self): vector
    -- Its y, in its parent's space.
    function up(self): vector
"#;

/// `Animator`'s methods on its views (ADR 0124's E12-2b), its graph's (E12-5c), and its
/// graph's state, a field the inspector hides.
const ANIMATOR_METHODS: &str = r#"    -- Plays clip `clip` (its name in the model's file) from its start, fading from what
    -- plays now over `fade` seconds (at once without one).
    function play(self, clip: string, fade: number?): ()
    -- Sets parameter `name` of its graph to `value` (a number; a flag: true or false), for
    -- its transitions and blends. An error when it plays no graph or the graph lacks it.
    function set(self, name: AnimatorParam, value: number | boolean): ()
    -- Fires trigger `name` of its graph: it holds until a transition takes it.
    function trigger(self, name: AnimatorTrigger): ()
    -- The state its graph's first layer is in, by name (empty without a graph): the player
    -- writes it each tick, after the scripts' tick. Read it; writing it is an error.
    state: string
"#;

/// The project's graphs' parameters' names as `AnimatorParam` and `AnimatorTrigger`:
/// unions of string choices, `string` without any.
fn declare_graph_names(names: &GraphNames, out: &mut String) {
    out.push_str(
        "\n-- The parameters of the project's animation graphs (`.agraph` files): `Animator:set`\n\
         -- takes their names, `Animator:trigger` their triggers'. Written again when a graph\n\
         -- changes. A name kept in a variable says so: `local name: AnimatorParam = \"speed\"`.\n",
    );
    for (alias, names) in [
        ("AnimatorParam", &names.params),
        ("AnimatorTrigger", &names.triggers),
    ] {
        let choices: Vec<String> = names.iter().filter_map(|n| luau_string(n)).collect();
        let one_line = choices.iter().map(|c| c.len() + 3).sum::<usize>() <= 80;
        if choices.is_empty() {
            let _ = writeln!(out, "export type {alias} = string");
        } else if one_line {
            let _ = writeln!(out, "export type {alias} = {}", choices.join(" | "));
        } else {
            let _ = writeln!(
                out,
                "export type {alias} =\n    {}",
                choices.join("\n    | ")
            );
        }
    }
}

/// `name` as a Luau string (`"speed"`, its quotes and backslashes escaped); none for a name
/// with a control character, which no script writes.
fn luau_string(name: &str) -> Option<String> {
    if name.chars().any(char::is_control) {
        return None;
    }
    let mut s = String::with_capacity(name.len() + 2);
    s.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('"');
    Some(s)
}

/// The definitions' last part: the libraries.
const LIBRARIES: &str = r#"
-- `self` of a component of props `P`, for an annotation:
-- `type Self = Script<typeof(Door.props)>`, then `function Door.on_tick(self: Self, dt)`.
export type Script<P> = P & ScriptSelf

-- A script component's table, which `component` returns: its hooks, defined on it.
type ScriptComponent<P> = {
    name: string,
    props: P,
    on_start: ((self: P & ScriptSelf) -> ())?,
    on_tick: ((self: P & ScriptSelf, dt: number) -> ())?,
    on_frame: ((self: P & ScriptSelf, dt: number, alpha: number) -> ())?,
    on_stop: ((self: P & ScriptSelf) -> ())?,
    on_touch: ((self: P & ScriptSelf, other: Entity?, point: vector, normal: vector) -> ())?,
    on_untouch: ((self: P & ScriptSelf, other: Entity?) -> ())?,
    on_load: ((self: P & ScriptSelf) -> ())?,
    on_animation_event: ((self: P & ScriptSelf, name: string, data: string?) -> ())?,
    [string]: any,
}

-- Declares component `name` with `props`, each a default (a number, a boolean, a vector)
-- or a `prop.*` value.
declare function component<P>(name: string, props: P): ScriptComponent<P>

declare prop: {
    number: (default: number, options: PropOptions?) -> number,
    integer: (default: number, options: PropOptions?) -> number,
    boolean: (default: boolean, options: PropOptions?) -> boolean,
    vector: (default: vector, options: PropOptions?) -> vector,
    -- Text the component keeps (a sound's path, a line of dialogue's key).
    string: (default: string?, options: PropOptions?) -> string,
    entity: (default: nil, options: PropOptions?) -> Entity?,
    -- An asset reference of `kind` ("scene", "model"), set in the editor or a scene file.
    asset: (kind: string?, options: PropOptions?) -> Asset?,
}

-- What `world.query` makes: `for e, a, b in q do` gives each entity that has the
-- components, and views of them (moved to each entity in turn: keep the entity, not the
-- view). Made once, at a module's top.
declare extern type Query with
    -- Only the entities that have `component` too.
    function with(self, component: ComponentType | string): Query
    -- Only the entities without `component`.
    function without(self, component: ComponentType | string): Query
    __iter: (self: Query) -> ((Query, Entity?) -> (Entity, ...any), Query, nil)
end

declare world: {
    -- The entity with that `Name` (the first made, of several), or nil.
    find: (name: string) -> Entity?,
    -- An entity with these components (`{ Transform = { translation = p }, Name = "Bullet" }`),
    -- at once.
    spawn: (components: { [string]: any }?) -> Entity,
    -- The entities that have these components, for `for e, a, b in q do`.
    query: (...ComponentType | string) -> Query,
    -- A prefab scene (an asset, or its path) placed at once, moved by `translation`: its
    -- root's entity.
    instantiate: (prefab: Asset | string, translation: vector?) -> Entity,
}

-- Declares system `name`: hooks called once a tick (`on_tick(dt)`) or a frame
-- (`on_frame(dt, alpha)`), after the components' instances, in the order declared.
declare function system(name: string, hooks: {
    on_tick: ((dt: number) -> ())?,
    on_frame: ((dt: number, alpha: number) -> ())?,
}): ()

-- The collision layers a query sees, by name: made once, at a module's top.
declare extern type PhysicsFilter with
end

declare physics: {
    -- The first body along the ray, at most `max_distance` away: its entity (nil for a body
    -- without one), the hit's point and normal, and its distance. Nils when it hits
    -- nothing.
    raycast: (origin: vector, direction: vector, max_distance: number, filter: PhysicsFilter?) -> (Entity?, vector?, vector?, number?),
    -- The entities whose bodies overlap the sphere, for a `for` loop.
    overlap_sphere: (center: vector, radius: number, filter: PhysicsFilter?) -> () -> Entity?,
    -- The layers named so (`PhysicsSettings::layer_names`).
    filter: (layers: { string }) -> PhysicsFilter,
}

-- The game's input, by its actions' names (the game's input map): in tick hooks the
-- tick's view (a tap shorter than a tick still counts), in frame hooks the frame's.
declare input: {
    -- Whether the action is held.
    pressed: (action: string) -> boolean,
    -- Whether it went down in this view.
    just_pressed: (action: string) -> boolean,
    -- Whether it went up in this view.
    just_released: (action: string) -> boolean,
    -- How far it is held, 0 to 1.
    strength: (action: string) -> number,
    -- A 1D axis, -1 to 1.
    axis: (action: string) -> number,
    -- A 2D axis as a vector (x, y, 0), at most 1 long.
    vector: (action: string) -> vector,
}

-- The world's random numbers: seeded, the same on every machine, kept by saves and
-- rollback (`math.random` is not).
declare random: {
    -- A number in [0, 1).
    number: () -> number,
    -- A whole number from `a` to `b`, both included.
    integer: (a: number, b: number) -> number,
    -- A point inside the sphere of radius 1.
    vector_in_sphere: () -> vector,
}

-- Sounds that play once and go: their entities (nil while a client's tick runs again: it
-- played the first time).
declare audio: {
    -- As it is, no place (the UI, a stinger), on `bus` (`ui` by default).
    play: (sound: string, bus: string?) -> Entity?,
    -- At `position` in the world, on `bus` (`effects` by default).
    play_at: (sound: string, position: vector, bus: string?) -> Entity?,
    -- Moves snapshot `name` of the mixer to `weight` (0 not heard, 1 all of it) over
    -- `fade` seconds (the snapshot's own fade when none): whether the mixer has it.
    blend: (name: string, weight: number, fade: number?) -> boolean,
}

-- The session the game plays in.
declare net: {
    -- Whether this side decides the world: alone, or hosting.
    is_server: () -> boolean,
    -- Whether this side plays on a host (its own entities predicted).
    is_client: () -> boolean,
    -- Whether this tick runs again (a client correcting its prediction): what it starts
    -- (sounds, messages, timers) started the first time.
    resimulating: () -> boolean,
}

declare time: {
    -- The game's time, in seconds.
    now: () -> number,
    -- The tick's number.
    tick: () -> number,
    -- The tick's length, in seconds.
    dt: () -> number,
}

declare quat: {
    identity: () -> Quat,
    -- About `axis`, by `angle` radians.
    axis_angle: (axis: vector, angle: number) -> Quat,
    -- Yaw `y`, then pitch `x`, then roll `z`, in radians.
    euler: (x: number, y: number, z: number) -> Quat,
}

-- To the log as a warning, with the script's file and line.
declare function warn(...: any): ()
"#;

/// The definitions of the engine's scripting API for the program as it runs: its
/// reflected components and the script components declared so far
/// ([`engine_ecs::runtime_types`]), sorted by name. An animator's parameters are any
/// string here: [`definitions_with`] names a project's.
pub fn definitions() -> String {
    definitions_with(&GraphNames::default())
}

/// [`definitions`], with the project's animation graphs' parameters' names as what
/// `Animator:set` and `Animator:trigger` take (ADR 0124's E12-5c).
pub fn definitions_with(graphs: &GraphNames) -> String {
    let mut components: BTreeMap<&'static str, &'static TypeInfo> = BTreeMap::new();
    // The program's components are globals too (`Transform`), scripts' are not.
    let mut program: Vec<&'static str> = Vec::new();
    for t in engine_ecs::reflected_types() {
        if matches!(t.kind, engine_ecs::ReflectedKind::Component { .. })
            && !t.info.attrs.hidden
            && !t.info.attrs.skip
        {
            components.entry(t.info.name).or_insert(t.info);
            program.push(t.info.name);
        }
    }
    for t in engine_ecs::runtime_types() {
        if !t.info.attrs.hidden && !t.info.attrs.skip {
            components.entry(t.info.name).or_insert(t.info);
        }
    }
    components.retain(|name, info| {
        is_name(name) && !RESERVED.contains(name) && matches!(info.kind, TypeKind::Struct(_))
    });
    let mut out = String::with_capacity(16 * 1024);
    out.push_str(HEADER);
    declare_graph_names(graphs, &mut out);
    let mut declared: Vec<&'static str> = Vec::new();
    for info in components.values() {
        declare_struct(info, &mut declared, &mut out);
    }
    // An entity, and `self`: their components by name, an entity's methods.
    out.push_str(
        "\n-- An entity: its components by name. Reading one it lacks is an error.\n\
         declare extern type Entity with\n",
    );
    out.push_str(ENTITY_MEMBERS);
    for name in components.keys() {
        let _ = writeln!(out, "    {name}: {name}");
    }
    out.push_str(
        "end\n\n-- `self` in a hook, beside its component's props.\ntype ScriptSelf = {\n",
    );
    out.push_str("    entity: Entity,\n    locals: { [string]: any },\n");
    out.push_str(SELF_METHODS);
    for name in components.keys() {
        let _ = writeln!(out, "    {name}: {name},");
    }
    out.push_str("}\n");
    out.push_str(LIBRARIES);
    out.push_str("\n-- The program's components, as `has`, `add` and `remove` take them.\n");
    program.sort_unstable();
    program.dedup();
    for name in program {
        if components.contains_key(name) {
            let _ = writeln!(out, "declare {name}: ComponentType");
        }
    }
    out
}

/// Declares struct `info` as an extern type (its struct fields' types first), once.
fn declare_struct(info: &'static TypeInfo, declared: &mut Vec<&'static str>, out: &mut String) {
    let TypeKind::Struct(fields) = &info.kind else {
        return;
    };
    if declared.contains(&info.name) {
        return;
    }
    declared.push(info.name);
    let mut lines = String::new();
    for f in *fields {
        if !is_name(f.name) || f.attrs.hidden {
            continue;
        }
        let Some(ty) = luau_type(f.ty, declared, out) else {
            continue;
        };
        if let Some(tip) = f.attrs.tooltip {
            let _ = writeln!(lines, "    -- {}", tip.replace('\n', " "));
        }
        let _ = writeln!(lines, "    {}: {ty}", f.name);
    }
    if info.name == "Transform" {
        lines.push_str(TRANSFORM_METHODS);
    }
    if info.name == "Animator" {
        lines.push_str(ANIMATOR_METHODS);
    }
    let _ = write!(
        out,
        "\ndeclare extern type {} with\n{lines}end\n",
        info.name
    );
}

/// The Luau type a view gives for a value of `ty` (its struct declared first), if scripts
/// can read it.
fn luau_type(
    ty: &'static TypeInfo,
    declared: &mut Vec<&'static str>,
    out: &mut String,
) -> Option<String> {
    match ty.name {
        "Vec3" | "Vec3A" | "Vec2" => return Some(String::from("vector")),
        "Quat" => return Some(String::from("Quat")),
        "Entity" => return Some(String::from("Entity?")),
        "AssetRef" => return Some(String::from("Asset?")),
        _ => {}
    }
    match &ty.kind {
        TypeKind::Primitive(Primitive::Bool) => Some(String::from("boolean")),
        TypeKind::Primitive(_) => Some(String::from("number")),
        TypeKind::String => Some(String::from("string")),
        TypeKind::Enum { variants, .. } if !variants.is_empty() => Some(
            variants
                .iter()
                .map(|v| format!("\"{}\"", v.name))
                .collect::<Vec<_>>()
                .join(" | "),
        ),
        TypeKind::Struct(_) if is_name(ty.name) && !RESERVED.contains(&ty.name) => {
            declare_struct(ty, declared, out);
            Some(ty.name.to_owned())
        }
        _ => None,
    }
}

/// Luau's reserved words: no name, so a field named so (`Timer.repeat`) is left out of the
/// definitions (a script reads it as `timer["repeat"]`).
const KEYWORDS: [&str; 21] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Whether `s` is a Luau name: a letter or `_`, then letters, digits and `_`, and no
/// reserved word.
fn is_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_types_of_their_readable_fields() {
        let text = definitions();
        let transform = text
            .split("declare extern type Transform with\n")
            .nth(1)
            .and_then(|rest| rest.split("end\n").next())
            .unwrap_or_default();
        for field in ["rotation: Quat", "translation: vector", "scale: vector"] {
            assert!(transform.contains(field), "{field} in {transform:?}");
        }
        // A field named by a reserved word is no name: left out.
        let timer = text
            .split("declare extern type Timer with\n")
            .nth(1)
            .and_then(|rest| rest.split("end\n").next())
            .unwrap_or_default();
        assert!(
            timer.contains("duration: number") && !timer.contains("repeat"),
            "{timer:?}"
        );
        assert!(text.contains("declare function component<P>(name: string, props: P)"));
        // The animator's own method, and its saved fade hidden (E12-2b).
        let animator = text
            .split(
                "declare extern type Animator with
",
            )
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "end
",
                )
                .next()
            })
            .unwrap_or_default();
        assert!(
            animator.contains("clip: string")
                && animator.contains("function play(self, clip: string, fade: number?): ()")
                && !animator.contains("from_clip"),
            "{animator:?}"
        );
    }

    /// The text of an `.agraph` with parameters `params` (a name and its kind), and a
    /// machine, a state and a transition that are not read for them.
    fn graph_text(params: &[(&str, ParamKind)]) -> String {
        use engine_scene::{NodeFile, NodeKind, ParamFile, StateFile, TransitionFile};
        let mut g = AnimGraphFile::default();
        for &(name, kind) in params {
            g.params
                .insert(name.to_owned(), ParamFile { kind, value: 0.0 });
        }
        g.nodes.insert(
            String::from("m0"),
            NodeFile {
                kind: NodeKind::Machine,
                entry: String::from("idle"),
                ..NodeFile::default()
            },
        );
        g.states.insert(
            String::from("idle"),
            StateFile {
                name: String::from("Idle"),
                machine: String::from("m0"),
                ..StateFile::default()
            },
        );
        g.transitions.insert(
            String::from("t1"),
            TransitionFile {
                to: String::from("idle"),
                when: String::from("speed > 1"),
                ..TransitionFile::default()
            },
        );
        g.to_text(engine_scene::AssetId::new(), None)
    }

    /// ADR 0124's E12-5c: an animator's `set` and `trigger` take the project's graphs'
    /// parameters' names as string choices, sorted and each once; any string without
    /// graphs. Its graph's `state` is declared though hidden.
    #[test]
    fn an_animators_graph_calls_take_the_projects_parameter_names() {
        let declared = |text: &str| -> String {
            text.split("declare extern type Animator with\n")
                .nth(1)
                .and_then(|rest| rest.split("end\n").next())
                .unwrap_or_default()
                .to_owned()
        };
        let text = definitions();
        assert!(
            text.contains("export type AnimatorParam = string\n")
                && text.contains("export type AnimatorTrigger = string\n"),
            "{text}"
        );
        let animator = declared(&text);
        for line in [
            "function set(self, name: AnimatorParam, value: number | boolean): ()",
            "function trigger(self, name: AnimatorTrigger): ()",
            "    state: string\n",
        ] {
            assert!(animator.contains(line), "{line} in {animator:?}");
        }
        assert!(!animator.contains("params"), "{animator:?}");

        // Two graphs: `speed` in both, `jump` a trigger in one and a flag in the other.
        let mut names = GraphNames::default();
        names
            .add_text(&graph_text(&[
                ("speed", ParamKind::Float),
                ("jump", ParamKind::Trigger),
                ("armed", ParamKind::Bool),
            ]))
            .unwrap();
        names
            .add_text(&graph_text(&[
                ("speed", ParamKind::Float),
                ("weapon", ParamKind::Int),
                ("jump", ParamKind::Bool),
            ]))
            .unwrap();
        // Quotes in a name are escaped; a control character leaves it out.
        names.triggers.insert(String::from("say \"hi\""));
        names.triggers.insert(String::from("two\nlines"));
        let text = definitions_with(&names);
        assert!(
            text.contains(
                "export type AnimatorParam = \"armed\" | \"jump\" | \"speed\" | \"weapon\"\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("export type AnimatorTrigger = \"jump\" | \"say \\\"hi\\\"\"\n"),
            "{text}"
        );
        // Many names: one a line.
        let mut many = GraphNames::default();
        for i in 0..20 {
            many.params.insert(format!("parameter_{i:02}"));
        }
        let text = definitions_with(&many);
        assert!(
            text.contains(
                "export type AnimatorParam =\n    \"parameter_00\"\n    | \"parameter_01\"\n"
            ),
            "{text}"
        );
        // A file that is not a graph's says so; a bad parameter line keeps its number.
        assert!(
            GraphNames::default()
                .add_text("[scene version=1]\n")
                .is_err()
        );
        let mut bad = graph_text(&[("speed", ParamKind::Float)]);
        bad.push_str("params[\"x\"].kind = Sideways\n");
        let line = bad.lines().count();
        let why = GraphNames::default().add_text(&bad).unwrap_err();
        assert!(why.contains(&line.to_string()), "line {line}: {why}");
    }
}
