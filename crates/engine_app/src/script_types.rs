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

use engine_reflect::{Primitive, TypeInfo, TypeKind};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Names the definitions give their own types; a component of one of these names is left
/// out (it would shadow them).
const RESERVED: [&str; 5] = [
    "Entity",
    "Quat",
    "ScriptSelf",
    "ScriptComponent",
    "PropOptions",
];

/// The definitions' first part: what does not depend on the program's components.
const HEADER: &str = r#"-- types/ofelia.d.luau: the engine's scripting API (ADR 0114), made by the editor from
-- the program's components and the project's scripts. Do not edit: it is written again.

-- A rotation: `q * v` rotates a vector, `a * b` composes.
declare extern type Quat with
    x: number
    y: number
    z: number
    w: number
    __mul: ((self: Quat, other: Quat) -> Quat) & ((self: Quat, other: vector) -> vector)
end

-- What `prop.number` and the other helpers take after the default.
type PropOptions = {
    min: number?,
    max: number?,
    step: number?,
    tooltip: string?,
    hidden: boolean?,
    skip: boolean?,
}
"#;

/// The definitions' last part: the libraries.
const LIBRARIES: &str = r#"
-- A script component's table, which `component` returns: its hooks, defined on it.
type ScriptComponent<P> = {
    name: string,
    props: P,
    on_start: ((self: P & ScriptSelf) -> ())?,
    on_tick: ((self: P & ScriptSelf, dt: number) -> ())?,
    on_frame: ((self: P & ScriptSelf, dt: number, alpha: number) -> ())?,
    on_stop: ((self: P & ScriptSelf) -> ())?,
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
    entity: (default: nil, options: PropOptions?) -> Entity?,
}

declare world: {
    -- The entity with that `Name` (the first made, of several), or nil.
    find: (name: string) -> Entity?,
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
/// ([`engine_ecs::runtime_types`]), sorted by name.
pub fn definitions() -> String {
    let mut components: BTreeMap<&'static str, &'static TypeInfo> = BTreeMap::new();
    for t in engine_ecs::reflected_types() {
        if matches!(t.kind, engine_ecs::ReflectedKind::Component { .. })
            && !t.info.attrs.hidden
            && !t.info.attrs.skip
        {
            components.entry(t.info.name).or_insert(t.info);
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
    let mut declared: Vec<&'static str> = Vec::new();
    for info in components.values() {
        declare_struct(info, &mut declared, &mut out);
    }
    // An entity, and `self`: their components by name.
    out.push_str(
        "\n-- An entity: its components by name. Reading one it lacks is an error.\n\
         declare extern type Entity with\n    \
         -- Its `Name`, if it has one.\n    \
         name: string?\n    \
         -- Whether it still exists.\n    \
         alive: boolean\n",
    );
    for name in components.keys() {
        let _ = writeln!(out, "    {name}: {name}");
    }
    out.push_str(
        "end\n\n-- `self` in a hook, beside its component's props.\ntype ScriptSelf = {\n",
    );
    out.push_str("    entity: Entity,\n    locals: { [string]: any },\n");
    for name in components.keys() {
        let _ = writeln!(out, "    {name}: {name},");
    }
    out.push_str("}\n");
    out.push_str(LIBRARIES);
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
        "AssetRef" => return None,
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
    }
}
