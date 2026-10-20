//! The inspector's properties (ADR 0107 phase E1-4): the fields of an entity's components,
//! found through reflection, each with how it is shown and how an edit is written back.
//!
//! - **Numbers** are dragged sideways ([`crate::Snapping`] has the modifier keys).
//! - **Vectors** and small arrays of numbers show their parts side by side.
//! - **A rotation** (a quaternion) shows as Euler angles in degrees, applied in the order
//!   Y, X, Z (yaw, pitch, roll), so a step of 45° means what it says.
//! - **Booleans** are checkboxes; **enums** pick a variant from a list.
//! - **Strings** are typed; references and containers are shown as text.
//! - **A changed field** (one that differs from its type's `Default`) can be reset
//!   ([`Defaults`], [`reset`]).
//! - **A selection of several entities** shows the components they all have; a part
//!   where they hold different values is marked ([`Inspector::compare`], shown "—").

use crate::snap::Quantity;
use engine_core::math::{EulerRot, Quat};
use engine_ecs::{ComponentId, Entity, World};
use engine_reflect::{Attrs, Hint, Primitive, TypeInfo, TypeKind, Value};
use engine_scene::text::{is_asset_ref, is_entity, newtype_field, vector_fields};
use engine_scene::{AssetPaths, AssetRef, Name};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::ptr::NonNull;

/// Each component type's `Default` value, made the first time it is asked for and kept
/// for the session (one value per type): what tells a changed field and what reset puts
/// back.
#[derive(Debug, Default)]
pub struct Defaults {
    values: HashMap<ComponentId, Option<NonNull<u8>>>,
}

impl Defaults {
    /// No values yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Component `id`'s default value; none for a type without reflection or `Default`.
    pub fn of(&mut self, world: &World, id: ComponentId) -> Option<NonNull<u8>> {
        *self.values.entry(id).or_insert_with(|| {
            let reg = world.components();
            let info = reg.type_info(id)?;
            let layout = reg.layout(id)?;
            let engine_ecs::ReflectedKind::Component { default, .. } =
                &engine_ecs::find_reflected(info.name)?.kind
            else {
                return None;
            };
            let write = default()?;
            let layout =
                std::alloc::Layout::from_size_align(layout.size().max(1), layout.align()).ok()?;
            // SAFETY: a non-zero size. The value is written by the type's default writer
            // and kept for the session: never freed, so never dropped.
            unsafe {
                let ptr = std::alloc::alloc(layout);
                if ptr.is_null() {
                    return None;
                }
                write(ptr);
                NonNull::new(ptr)
            }
        })
    }
}

/// How a property is shown and edited.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// Text only.
    Text,
    /// Numbers side by side (one for a plain number), all of type `element`.
    Numbers {
        /// How many.
        len: u8,
        /// What they are, for speed and step.
        quantity: Quantity,
        /// Their type (`f32`, `u32`…).
        element: &'static TypeInfo,
        /// Bytes from one to the next.
        stride: u16,
        /// Stored in radians, shown in degrees.
        degrees: bool,
    },
    /// A quaternion shown as three Euler angles in degrees.
    Rotation,
    /// A checkbox.
    Bool,
    /// One of an enum's variants.
    Choice,
}

/// One property: a field of a component, at a fixed place in it.
#[derive(Debug, Clone, Copy)]
pub struct Property {
    /// Its component.
    pub component: ComponentId,
    /// Its byte offset in the component.
    pub offset: usize,
    /// Its type.
    pub ty: &'static TypeInfo,
    /// Its attributes (range, step, hint).
    pub attrs: Attrs,
    /// How it is shown.
    pub kind: Kind,
    /// The component is written whole (`Name = "Door"`): its scene path is the component's
    /// name alone.
    pub whole: bool,
    /// It differs from its type's default and can be reset ([`reset`]); with several
    /// entities, in any of them.
    pub changed: bool,
    /// The parts (bit `i` for part `i`) where the other selected entities hold another
    /// value ([`Inspector::compare`]): shown "—".
    pub mixed: u8,
    /// Its name in [`Inspector::text`].
    name: (u32, u32),
    /// Its values' texts in [`Inspector::text`].
    values: [(u32, u32); 4],
    count: u8,
}

/// A component's properties.
#[derive(Debug, Clone)]
pub struct Section {
    /// The component's name.
    pub title: &'static str,
    /// The component.
    pub component: ComponentId,
    /// Its properties in [`Inspector::properties`].
    pub properties: Range<usize>,
}

/// The properties of one entity, built again every frame (cheap: a few hundred bytes of
/// text, buffers kept between frames).
#[derive(Debug, Default)]
pub struct Inspector {
    text: String,
    properties: Vec<Property>,
    sections: Vec<Section>,
    scratch: String,
    /// What [`Inspector::compare`] rebuilds into, kept between frames.
    spare: Vec<Property>,
    spare_sections: Vec<Section>,
    other_text: String,
}

impl Inspector {
    /// No properties.
    pub fn new() -> Self {
        Self::default()
    }

    /// No properties (nothing selected).
    pub fn clear(&mut self) {
        self.text.clear();
        self.properties.clear();
        self.sections.clear();
    }

    /// The entity's components and their properties, sorted by component name: those
    /// scenes show (not `hidden`, not `skip`); each property marked changed when it differs
    /// from its type's default.
    pub fn build(
        &mut self,
        world: &World,
        entity: Entity,
        assets: &AssetPaths,
        defaults: &mut Defaults,
    ) {
        self.clear();
        let Some(ids) = world.component_ids_of(entity) else {
            return;
        };
        let mut components: Vec<(&'static TypeInfo, ComponentId)> = ids
            .iter()
            .filter_map(|&id| Some((world.components().type_info(id)?, id)))
            .filter(|(info, _)| !info.attrs.hidden && !info.attrs.skip)
            .collect();
        components.sort_by_key(|(info, _)| info.name);
        for (info, id) in components {
            let Some(ptr) = world.get_raw(entity, id) else {
                continue;
            };
            let first = self.properties.len();
            let mut path = String::new();
            // SAFETY: the entity's live value of the component `info` describes.
            unsafe {
                self.walk(
                    world,
                    assets,
                    id,
                    ptr.as_ptr().cast_const(),
                    info,
                    info.attrs,
                    0,
                    &mut path,
                );
            }
            // A name is what the entity is called, not a value away from a default: it
            // has no reset.
            if info.name != "Name"
                && let Some(default) = defaults.of(world, id)
            {
                for p in &mut self.properties[first..] {
                    // SAFETY: both point to live values of the component.
                    p.changed = unsafe {
                        differs(p, ptr.as_ptr().cast_const(), default.as_ptr().cast_const())
                    };
                }
            }
            // A transform reads position, rotation, scale (Godot, Unity), whatever the
            // order of its fields.
            if info.name == "Transform" {
                let text = &self.text;
                let order = |p: &Property| match &text[p.name.0 as usize..p.name.1 as usize] {
                    "translation" => 0,
                    "rotation" => 1,
                    "scale" => 2,
                    _ => 3,
                };
                self.properties[first..].sort_by_key(order);
            }
            self.sections.push(Section {
                title: info.name,
                component: id,
                properties: first..self.properties.len(),
            });
        }
    }

    /// Shows a selection: keeps the sections whose component every entity of `others`
    /// has, marks the parts of each property where one of them holds another value than
    /// `entity` (the one built from) as [`Property::mixed`], and marks a property changed
    /// when any of them differs from its default. Plain values are compared by their
    /// bytes, part by part (a rotation as a whole), strings and containers by their text.
    pub fn compare(
        &mut self,
        world: &World,
        assets: &AssetPaths,
        entity: Entity,
        others: &[Entity],
        defaults: &mut Defaults,
    ) {
        if others.is_empty() {
            return;
        }
        let mut kept = core::mem::take(&mut self.spare);
        let mut sections = core::mem::take(&mut self.spare_sections);
        kept.clear();
        sections.clear();
        'sections: for section in &self.sections {
            let id = section.component;
            let Some(base) = world.get_raw(entity, id) else {
                continue;
            };
            // A name has no reset (see `build`).
            let default = (section.title != "Name")
                .then(|| defaults.of(world, id))
                .flatten();
            let first = kept.len();
            kept.extend_from_slice(&self.properties[section.properties.clone()]);
            // One lookup per entity and component; a component one of them lacks is not
            // shown.
            for &o in others {
                let Some(other) = world.get_raw(o, id) else {
                    kept.truncate(first);
                    continue 'sections;
                };
                let (a, b) = (base.as_ptr().cast_const(), other.as_ptr().cast_const());
                for p in &mut kept[first..] {
                    // SAFETY: both are live values of the property's component.
                    unsafe {
                        p.mixed |= mixed_parts(p, a, b, world, assets, &mut self.other_text);
                        if !p.changed
                            && let Some(d) = default
                        {
                            p.changed = differs(p, b, d.as_ptr().cast_const());
                        }
                    }
                }
            }
            sections.push(Section {
                properties: first..kept.len(),
                ..section.clone()
            });
        }
        self.spare = core::mem::replace(&mut self.properties, kept);
        self.spare_sections = core::mem::replace(&mut self.sections, sections);
    }

    /// The sections, in order.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The properties of every section.
    pub fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// A property's name (`translation`, `shadow.bias`; `value` for a component written
    /// whole, such as `Name`).
    pub fn name(&self, p: &Property) -> &str {
        &self.text[p.name.0 as usize..p.name.1 as usize]
    }

    /// The labels of a property's numbers: `r g b a` for a color, `x y z w` otherwise.
    pub fn labels(p: &Property) -> &'static [&'static str] {
        let all: &'static [&'static str] = if p.attrs.hint == Hint::Color {
            &["r", "g", "b", "a"]
        } else {
            &["x", "y", "z", "w"]
        };
        &all[..usize::from(p.count).min(4)]
    }

    /// A property's shown values: one, or a vector's parts.
    pub fn values<'a>(&'a self, p: &'a Property) -> impl Iterator<Item = &'a str> + 'a {
        p.values[..usize::from(p.count)]
            .iter()
            .map(|&(a, b)| &self.text[a as usize..b as usize])
    }

    /// Adds properties for the value of `ty` at `offset` in the component at `base`.
    ///
    /// # Safety
    ///
    /// `base` points to a live value of the component, and `ty` describes the value at
    /// `base + offset`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn walk(
        &mut self,
        world: &World,
        assets: &AssetPaths,
        component: ComponentId,
        base: *const u8,
        ty: &'static TypeInfo,
        attrs: Attrs,
        offset: usize,
        path: &mut String,
    ) {
        // SAFETY: for the block, every step stays inside the value its description lays
        // out (fields at their offsets, array elements at their strides).
        unsafe {
            let ptr = base.add(offset);
            if let Some(f) = newtype_field(ty) {
                self.walk(
                    world,
                    assets,
                    component,
                    base,
                    f.ty,
                    f.attrs,
                    offset + f.offset,
                    path,
                );
                return;
            }
            let Some(kind) = kind_of(ty, attrs, path) else {
                // A struct: its fields one by one, named by their paths.
                if let TypeKind::Struct(fields) = ty.kind {
                    for f in fields {
                        if f.attrs.skip || f.attrs.hidden {
                            continue;
                        }
                        let len = path.len();
                        if len > 0 {
                            path.push('.');
                        }
                        path.push_str(f.name);
                        self.walk(
                            world,
                            assets,
                            component,
                            base,
                            f.ty,
                            f.attrs,
                            offset + f.offset,
                            path,
                        );
                        path.truncate(len);
                    }
                }
                return;
            };
            let whole = path.is_empty();
            let name = self.push_text(if whole { "value" } else { path });
            let mut values = [(0, 0); 4];
            let count = match kind {
                Kind::Numbers {
                    len,
                    element,
                    stride,
                    degrees,
                    ..
                } => {
                    for (i, slot) in values.iter_mut().enumerate().take(usize::from(len)) {
                        let v = engine_reflect::read(ptr.add(i * usize::from(stride)), element)
                            .map_or(0.0, Value::as_f64);
                        let start = self.text.len();
                        if degrees {
                            push_number(&mut self.text, v.to_degrees(), 2);
                        } else if matches!(
                            element.kind,
                            TypeKind::Primitive(Primitive::F32 | Primitive::F64)
                        ) {
                            push_number(&mut self.text, v, 3);
                        } else {
                            let _ = write!(self.text, "{v}");
                        }
                        *slot = (start as u32, self.text.len() as u32);
                    }
                    len
                }
                Kind::Rotation => {
                    let degrees = euler_degrees(read_quat(ptr));
                    for (i, d) in degrees.into_iter().enumerate() {
                        let start = self.text.len();
                        push_number(&mut self.text, d, 2);
                        values[i] = (start as u32, self.text.len() as u32);
                    }
                    3
                }
                Kind::Bool | Kind::Choice | Kind::Text => {
                    let start = self.text.len();
                    self.scratch.clear();
                    show(&mut self.scratch, world, assets, ty, ptr);
                    self.text.push_str(&self.scratch);
                    values[0] = (start as u32, self.text.len() as u32);
                    1
                }
            };
            self.properties.push(Property {
                component,
                offset,
                ty,
                attrs,
                kind,
                whole,
                changed: false,
                mixed: 0,
                name,
                values,
                count,
            });
        }
    }

    fn push_text(&mut self, s: &str) -> (u32, u32) {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        (start, self.text.len() as u32)
    }
}

/// How a value is edited; `None` for a struct whose fields are shown one by one.
fn kind_of(ty: &'static TypeInfo, attrs: Attrs, path: &str) -> Option<Kind> {
    let field = path.rsplit('.').next().unwrap_or(path);
    let quantity_of = |element: &TypeInfo| match element.kind {
        TypeKind::Primitive(Primitive::F32 | Primitive::F64) => {
            if attrs.hint == Hint::Degrees {
                Quantity::Angle
            } else if matches!(field, "translation" | "position") {
                Quantity::Position
            } else if field == "scale" {
                Quantity::Scale
            } else {
                Quantity::Number { step: attrs.step }
            }
        }
        _ => Quantity::Integer,
    };
    if is_entity(ty) || is_asset_ref(ty) {
        return Some(Kind::Text);
    }
    if ty.name == "Quat" && vector_fields(ty).is_some_and(|f| f.len() == 4) {
        return Some(Kind::Rotation);
    }
    if let Some(fields) = vector_fields(ty) {
        let element = fields[0].ty;
        return Some(Kind::Numbers {
            len: fields.len() as u8,
            quantity: quantity_of(element),
            element,
            stride: element.size as u16,
            degrees: attrs.hint == Hint::Degrees,
        });
    }
    match ty.kind {
        TypeKind::Primitive(Primitive::Bool) => Some(Kind::Bool),
        TypeKind::Primitive(_) => Some(Kind::Numbers {
            len: 1,
            quantity: quantity_of(ty),
            element: ty,
            stride: ty.size as u16,
            degrees: attrs.hint == Hint::Degrees,
        }),
        TypeKind::Enum { .. } => Some(Kind::Choice),
        TypeKind::Array { element, len }
            if (1..=4).contains(&len)
                && matches!(element.kind, TypeKind::Primitive(p) if p != Primitive::Bool) =>
        {
            Some(Kind::Numbers {
                len: len as u8,
                quantity: quantity_of(element),
                element,
                stride: element.size as u16,
                degrees: attrs.hint == Hint::Degrees,
            })
        }
        TypeKind::Struct(_) => None,
        _ => Some(Kind::Text),
    }
}

/// Appends a number with at most `decimals` decimals, without trailing zeros (and `0`
/// for -0).
fn push_number(out: &mut String, v: f64, decimals: usize) {
    let start = out.len();
    let _ = write!(out, "{v:.decimals$}");
    if out[start..].contains('.') {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    if &out[start..] == "-0" {
        out.truncate(start);
        out.push('0');
    }
}

/// A value shown as text: strings unquoted, entities by name, assets by path, the rest
/// as a scene writes it (the first line of a container's lines, `…` after it).
///
/// # Safety
///
/// `ptr` points to a live value of `ty`.
unsafe fn show(
    out: &mut String,
    world: &World,
    assets: &AssetPaths,
    ty: &'static TypeInfo,
    ptr: *const u8,
) {
    // SAFETY: for the block, as the caller guarantees.
    unsafe {
        if is_entity(ty) {
            let e = ptr.cast::<Entity>().read_unaligned();
            match world.get::<Name>(e) {
                _ if !world.contains(e) => out.push_str("none"),
                Some(name) => out.push_str(name.as_str()),
                None => {
                    let _ = write!(out, "entity {}", e.index());
                }
            }
            return;
        }
        if is_asset_ref(ty) {
            let asset = ptr.cast::<AssetRef>().read_unaligned();
            match assets.get(asset) {
                Some(path) if !asset.is_none() => out.push_str(path),
                _ => asset.write(Some(assets), out),
            }
            return;
        }
        match ty.kind {
            TypeKind::String => out.push_str(engine_reflect::read_str(ptr)),
            TypeKind::Primitive(Primitive::Bool) => {
                let on = engine_reflect::read(ptr, ty) == Some(Value::Bool(true));
                out.push_str(if on { "true" } else { "false" });
            }
            TypeKind::Enum { variants, .. } => {
                let raw = match engine_reflect::read(ptr, ty) {
                    Some(Value::Int(i)) => i,
                    Some(Value::UInt(u)) => u as i64,
                    _ => 0,
                };
                match variants.iter().find(|v| v.value == raw) {
                    Some(v) => out.push_str(v.name),
                    None => {
                        let _ = write!(out, "{raw}");
                    }
                }
            }
            _ => {
                let mut lines = String::new();
                engine_scene::write_lines(ty, ptr, Some(assets), &mut lines);
                let mut it = lines.lines();
                if let Some(first) = it.next() {
                    out.push_str(first.split_once(" = ").map_or(first, |(_, v)| v).trim());
                    if it.next().is_some() {
                        out.push_str(" …");
                    }
                }
            }
        }
    }
}

/// The quaternion at `ptr` (x, y, z, w as four `f32`).
///
/// # Safety
///
/// `ptr` points to four `f32`.
unsafe fn read_quat(ptr: *const u8) -> Quat {
    // SAFETY: as the caller guarantees.
    let [x, y, z, w] = unsafe { ptr.cast::<[f32; 4]>().read_unaligned() };
    Quat::from_xyzw(x, y, z, w)
}

/// A rotation's Euler angles in degrees, as shown: (pitch about X, yaw about Y, roll about
/// Z), applied yaw first, then pitch, then roll.
pub fn euler_degrees(q: Quat) -> [f64; 3] {
    let (yaw, pitch, roll) = q.normalize().to_euler(EulerRot::YXZ);
    [pitch, yaw, roll].map(|a| f64::from(a).to_degrees())
}

/// The rotation of Euler angles in degrees, as [`euler_degrees`] shows them.
pub fn from_euler_degrees([pitch, yaw, roll]: [f64; 3]) -> Quat {
    let r = |d: f64| d.to_radians() as f32;
    Quat::from_euler(EulerRot::YXZ, r(yaw), r(pitch), r(roll))
}

/// An `f32` as the `f64` of its shortest decimal form (0.67, not 0.6700000166893005), so
/// sums of drag steps on it stay decimal (0.67 + 0.5 is 1.17 in the file, not 1.1700001).
fn decimal(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(f64::from(v))
}

/// An angle in degrees without the noise of its conversion (to 0.0001°).
fn clean_degrees(v: f64) -> f64 {
    let r = (v * 1e4).round() / 1e4;
    if r == 0.0 { 0.0 } else { r }
}

/// Reads the numbers of a [`Kind::Numbers`] or [`Kind::Rotation`] property as shown
/// (degrees for angles), as the start of a drag: `f32`s by their decimal form, angles
/// without conversion noise.
///
/// # Safety
///
/// `component` points to a live value of the property's component.
pub unsafe fn read_numbers(p: &Property, component: *const u8) -> [f64; 4] {
    let mut out = [0.0; 4];
    // SAFETY: for the block, the property's offset and strides stay inside the component.
    unsafe {
        let ptr = component.add(p.offset);
        match p.kind {
            Kind::Numbers {
                len,
                element,
                stride,
                degrees,
                ..
            } => {
                for (i, slot) in out.iter_mut().enumerate().take(usize::from(len)) {
                    let at = ptr.add(i * usize::from(stride));
                    let v = match element.kind {
                        TypeKind::Primitive(Primitive::F32) => {
                            decimal(at.cast::<f32>().read_unaligned())
                        }
                        _ => engine_reflect::read(at, element).map_or(0.0, Value::as_f64),
                    };
                    *slot = if degrees {
                        clean_degrees(v.to_degrees())
                    } else {
                        v
                    };
                }
            }
            Kind::Rotation => {
                let [x, y, z] = euler_degrees(read_quat(ptr)).map(clean_degrees);
                out = [x, y, z, 0.0];
            }
            _ => {}
        }
    }
    out
}

/// Writes the numbers of a [`Kind::Numbers`] or [`Kind::Rotation`] property, as shown
/// (degrees for angles), clamped to its range.
///
/// # Safety
///
/// `component` points to a live, writable value of the property's component.
pub unsafe fn write_numbers(p: &Property, component: *mut u8, values: [f64; 4]) {
    // SAFETY: for the block, the property's offset and strides stay inside the component.
    unsafe {
        let ptr = component.add(p.offset);
        match p.kind {
            Kind::Numbers {
                len,
                element,
                stride,
                degrees,
                ..
            } => {
                for (i, &v) in values.iter().enumerate().take(usize::from(len)) {
                    let v = if degrees { v.to_radians() } else { v };
                    let _ = engine_reflect::write(
                        ptr.add(i * usize::from(stride)),
                        element,
                        &p.attrs,
                        Value::Float(v),
                    );
                }
            }
            Kind::Rotation => {
                let q = from_euler_degrees([values[0], values[1], values[2]]);
                ptr.cast::<[f32; 4]>().write_unaligned([q.x, q.y, q.z, q.w]);
            }
            _ => {}
        }
    }
}

/// Flips a [`Kind::Bool`] property.
///
/// # Safety
///
/// `component` points to a live, writable value of the property's component.
pub unsafe fn toggle(p: &Property, component: *mut u8) {
    // SAFETY: the property's offset holds a `bool` of its type.
    unsafe {
        let ptr = component.add(p.offset);
        let on = engine_reflect::read(ptr, p.ty) == Some(Value::Bool(true));
        let _ = engine_reflect::write(ptr, p.ty, &p.attrs, Value::Bool(!on));
    }
}

/// The parts of a property where two values of its component differ (bit `i` for part
/// `i`).
///
/// # Safety
///
/// `a` and `b` point to live values of the property's component.
unsafe fn mixed_parts(
    p: &Property,
    a: *const u8,
    b: *const u8,
    world: &World,
    assets: &AssetPaths,
    scratch: &mut String,
) -> u8 {
    // SAFETY: the property's offset and size stay inside both values.
    unsafe {
        let (a, b) = (a.add(p.offset), b.add(p.offset));
        let same = |a: *const u8, b: *const u8, n: usize| {
            core::slice::from_raw_parts(a, n) == core::slice::from_raw_parts(b, n)
        };
        match p.kind {
            Kind::Numbers {
                len,
                element,
                stride,
                ..
            } => (0..usize::from(len))
                .filter(|&i| {
                    let at = i * usize::from(stride);
                    !same(a.add(at), b.add(at), element.size)
                })
                .fold(0, |bits, i| bits | 1 << i),
            Kind::Rotation => {
                if same(a, b, p.ty.size) {
                    0
                } else {
                    0b111
                }
            }
            Kind::Bool | Kind::Choice => u8::from(!same(a, b, p.ty.size)),
            Kind::Text if matches!(p.ty.kind, TypeKind::String) => {
                u8::from(engine_reflect::read_str(a) != engine_reflect::read_str(b))
            }
            Kind::Text if is_entity(p.ty) || is_asset_ref(p.ty) => u8::from(!same(a, b, p.ty.size)),
            Kind::Text => {
                // Containers: by the text shown.
                scratch.clear();
                show(scratch, world, assets, p.ty, a);
                let first = scratch.len();
                show(scratch, world, assets, p.ty, b);
                u8::from(scratch[..first] != scratch[first..])
            }
        }
    }
}

/// Whether a property can be reset: plain values (numbers, vectors, rotations,
/// booleans, enums) and strings.
pub fn resettable(p: &Property) -> bool {
    match p.kind {
        Kind::Numbers { .. } | Kind::Rotation | Kind::Bool | Kind::Choice => true,
        Kind::Text => matches!(p.ty.kind, TypeKind::String),
    }
}

/// Whether the property differs between two values of its component.
///
/// # Safety
///
/// Both point to live values of the property's component.
unsafe fn differs(p: &Property, a: *const u8, b: *const u8) -> bool {
    if !resettable(p) {
        return false;
    }
    // SAFETY: the property's offset and size stay inside both values.
    unsafe {
        let (a, b) = (a.add(p.offset), b.add(p.offset));
        if matches!(p.ty.kind, TypeKind::String) {
            return engine_reflect::read_str(a) != engine_reflect::read_str(b);
        }
        core::slice::from_raw_parts(a, p.ty.size) != core::slice::from_raw_parts(b, p.ty.size)
    }
}

/// Puts a property back to its value in `default` (a value of its component): plain values
/// by their bytes, strings by their text. False for a property that cannot be reset.
///
/// # Safety
///
/// `component` points to a live, writable value of the property's component, `default` to
/// a live value of it.
pub unsafe fn reset(p: &Property, component: *mut u8, default: *const u8) -> bool {
    if !resettable(p) {
        return false;
    }
    // SAFETY: the property's offset and size stay inside both values; plain values are
    // valid as any copy of a valid value, strings are replaced through their setter.
    unsafe {
        let (to, from) = (component.add(p.offset), default.add(p.offset));
        if matches!(p.ty.kind, TypeKind::String) {
            let text = engine_reflect::read_str(from).to_owned();
            engine_reflect::write_str(to, &text);
        } else {
            core::ptr::copy_nonoverlapping(from, to, p.ty.size);
        }
    }
    true
}

/// The variants' names of a [`Kind::Choice`] property, in order.
pub fn variants(p: &Property) -> impl Iterator<Item = &'static str> {
    let variants = match p.ty.kind {
        TypeKind::Enum { variants, .. } => variants,
        _ => &[],
    };
    variants.iter().map(|v| v.name)
}

/// Sets a [`Kind::Choice`] property to its `index`-th variant.
///
/// # Safety
///
/// `component` points to a live, writable value of the property's component.
pub unsafe fn choose(p: &Property, component: *mut u8, index: usize) {
    let TypeKind::Enum { variants, .. } = p.ty.kind else {
        return;
    };
    let Some(v) = variants.get(index) else {
        return;
    };
    // SAFETY: the property's offset holds an enum of its type; only its variants are written.
    unsafe {
        let _ = engine_reflect::write(component.add(p.offset), p.ty, &p.attrs, Value::Int(v.value));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_core::math::Vec3;
    use engine_ecs::Transform;
    use engine_scene::SceneId;

    #[test]
    fn transform_shows_as_vectors_and_degrees_and_edits_back() {
        let mut world = World::new();
        let e = world.spawn((
            SceneId::new(),
            Name::new("Box"),
            Transform::from_xyz(1.5, 0.0, -2.0)
                .with_rotation(Quat::from_rotation_y(90f32.to_radians()))
                .with_scale(Vec3::splat(2.0)),
        ));
        let mut inspector = Inspector::new();
        let mut defaults = Defaults::new();
        inspector.build(&world, e, &AssetPaths::new(), &mut defaults);
        let titles: Vec<&str> = inspector.sections().iter().map(|s| s.title).collect();
        assert_eq!(titles, ["Name", "Transform"]);
        let props = inspector.properties();
        assert_eq!(inspector.name(&props[0]), "value");
        assert_eq!(inspector.values(&props[0]).collect::<Vec<_>>(), ["Box"]);
        let shown: Vec<(&str, Vec<&str>)> = props[1..]
            .iter()
            .map(|p| (inspector.name(p), inspector.values(p).collect()))
            .collect();
        assert_eq!(
            shown,
            [
                ("translation", vec!["1.5", "0", "-2"]),
                ("rotation", vec!["0", "90", "0"]),
                ("scale", vec!["2", "2", "2"]),
            ]
        );
        let translation = props[1];
        assert!(matches!(
            translation.kind,
            Kind::Numbers {
                quantity: Quantity::Position,
                len: 3,
                ..
            }
        ));
        let rotation = props[2];
        let id = translation.component;
        let ptr = world.get_raw_mut(e, id).unwrap().as_ptr();
        // SAFETY: the entity's live `Transform`.
        unsafe {
            write_numbers(&translation, ptr, [0.4, 0.0, -2.0, 0.0]);
            let mut angles = read_numbers(&rotation, ptr);
            assert!((angles[1] - 90.0).abs() < 1e-4, "{angles:?}");
            angles[1] = 45.0;
            write_numbers(&rotation, ptr, angles);
        }
        let t = world.get::<Transform>(e).unwrap();
        assert_eq!(t.translation, Vec3::new(0.4, 0.0, -2.0));
        assert!(
            t.rotation
                .angle_between(Quat::from_rotation_y(45f32.to_radians()))
                < 1e-5
        );

        // All three transform fields differ from the default; reset puts the scale back
        // to 1.
        inspector.build(&world, e, &AssetPaths::new(), &mut defaults);
        let props = inspector.properties();
        assert!(props[1..].iter().all(|p| p.changed));
        let scale = props[3];
        assert_eq!(inspector.name(&scale), "scale");
        let default = defaults.of(&world, id).unwrap();
        let ptr = world.get_raw_mut(e, id).unwrap().as_ptr();
        // SAFETY: the entity's live `Transform` and the default one.
        assert!(unsafe { reset(&scale, ptr, default.as_ptr()) });
        assert_eq!(world.get::<Transform>(e).unwrap().scale, Vec3::ONE);
        inspector.build(&world, e, &AssetPaths::new(), &mut defaults);
        assert!(!inspector.properties()[3].changed);
        // The name differs from the empty default, but has no reset.
        assert!(!inspector.properties()[0].changed);
    }

    #[test]
    fn euler_angles_round_trip() {
        for angles in [[30.0, 45.0, 10.0], [-80.0, 170.0, 0.0], [0.0, -135.0, 60.0]] {
            let back = euler_degrees(from_euler_degrees(angles));
            for (a, b) in angles.iter().zip(back) {
                assert!((a - b).abs() < 1e-3, "{angles:?} → {back:?}");
            }
        }
    }

    #[test]
    fn numbers_drop_trailing_zeros() {
        let mut s = String::new();
        for (v, d, want) in [
            (0.5, 3, "0.5"),
            (2.0, 3, "2"),
            (-0.0001, 3, "0"),
            (12.345678, 2, "12.35"),
        ] {
            s.clear();
            push_number(&mut s, v, d);
            assert_eq!(s, want);
        }
    }
}
