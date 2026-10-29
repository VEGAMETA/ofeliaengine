//! A material file edited in the inspector (ADR 0120's E10-1c; the materials UX note's
//! "what we take" 3 and 4): picked in the FileSystem dock, its values are rows of the
//! inspector. The editor holds the file as it is edited ([`MaterialDoc`]) and gives it to
//! the player each time it changes, drawn by the next frame with nothing compiled; every
//! edit is a step of the one history (the file's text before and after it); Ctrl+S writes
//! it.
//!
//! - **A standard material's rows:** base color, metallic, roughness, emissive, alpha and
//!   its cutoff (masked), double sided.
//! - **A material type's:** the type and its module; a row per field of its `Params` struct
//!   as Slang reflects it (numbers, a vector's parts, an image and its encoding; an array
//!   shows how many values it holds); its look, with the look's values when it is its
//!   own; then alpha and sides.

use crate::inspector::push_number;
use engine_assets::material_file::{Alpha, Look, MaterialFile, MaterialTexture, TextureEncoding};
use engine_render::material::MaterialTypeId;
use engine_scene::{AssetId, AssetPaths, AssetRef};
use engine_shader::{FieldKind, StructLayout};
use std::path::{Path, PathBuf};

/// What a row of a material's section shows and edits.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Row {
    /// A variant's base (E10-4b): the material file whose values it has where it sets none.
    Base,
    /// The type and its module.
    Type,
    /// What the type's shading costs this GPU (E10-4c): its registers beside the engine's.
    Cost,
    /// The code its type's shading reaches (E10-4f): instructions and image samples beside
    /// the engine's, and how long its last compile took.
    Code,
    /// Whether the view draws it as its fallback, its look (E10-4f's Draw as Fallback):
    /// the editor's, not the file's.
    Fallback,
    /// A field of the type's `Params`: its path, what it holds, its elements.
    Param {
        name: String,
        kind: FieldKind,
        count: u32,
    },
    /// How an image parameter's image is encoded.
    Encoding {
        name: String,
    },
    Look,
    BaseColor,
    Metallic,
    Roughness,
    Emissive,
    Alpha,
    AlphaCutoff,
    DoubleSided,
}

/// How a row is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shown {
    /// Text that cannot be edited here.
    Text,
    /// Numbers dragged sideways: how many, whether whole, their labels.
    Numbers {
        parts: usize,
        whole: bool,
        labels: &'static [&'static str],
    },
    /// One of these names.
    Choice(&'static [&'static str]),
    /// A checkbox.
    Check,
    /// An image, dropped from the FileSystem dock.
    Image,
}

const RGBA: &[&str] = &["r", "g", "b", "a"];
const XYZW: &[&str] = &["x", "y", "z", "w"];
const LOOKS: &[&str] = &["Own", "Replaced"];
const ALPHAS: &[&str] = &["Opaque", "Mask", "Blend"];
const ENCODINGS: &[&str] = &["Color", "Data", "Normal"];

/// A vector parameter whose name says it is a color: its parts read r g b a (until Slang's
/// attributes give hints, E10-2).
fn colorish(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    ["color", "colour", "tint", "albedo", "base"]
        .iter()
        .any(|w| last.contains(w))
}

impl Row {
    /// Its name in the inspector.
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Base => "base",
            Self::Cost => "cost",
            Self::Code => "code",
            Self::Fallback => "draw as fallback",
            Self::Type => "type",
            Self::Param { name, .. } => name,
            Self::Encoding { .. } => "  encoding",
            Self::Look => "look",
            Self::BaseColor => "base_color",
            Self::Metallic => "metallic",
            Self::Roughness => "roughness",
            Self::Emissive => "emissive",
            Self::Alpha => "alpha",
            Self::AlphaCutoff => "alpha_cutoff",
            Self::DoubleSided => "double_sided",
        }
    }

    /// How it is shown.
    pub(crate) fn shown(&self) -> Shown {
        let numbers = |parts: usize, whole: bool, labels: &'static [&'static str]| Shown::Numbers {
            parts,
            whole,
            labels,
        };
        match self {
            Self::Base => Shown::Image,
            Self::Cost | Self::Code => Shown::Text,
            Self::Fallback => Shown::Check,
            Self::Type => Shown::Text,
            Self::Param { count, .. } if *count != 1 => Shown::Text,
            Self::Param { name, kind, .. } => match *kind {
                FieldKind::Texture => Shown::Image,
                FieldKind::Float(n) | FieldKind::Uint(n) | FieldKind::Int(n) => {
                    let n = usize::from(n.clamp(1, 4));
                    let whole = !matches!(kind, FieldKind::Float(_));
                    let labels: &'static [&'static str] = match n {
                        1 => &[],
                        _ if colorish(name) => &RGBA[..n],
                        _ => &XYZW[..n],
                    };
                    numbers(n, whole, labels)
                }
            },
            Self::Encoding { .. } => Shown::Choice(ENCODINGS),
            Self::Look => Shown::Choice(LOOKS),
            Self::BaseColor => numbers(4, false, RGBA),
            Self::Emissive => numbers(3, false, &RGBA[..3]),
            Self::Metallic | Self::Roughness | Self::AlphaCutoff => numbers(1, false, &[]),
            Self::Alpha => Shown::Choice(ALPHAS),
            Self::DoubleSided => Shown::Check,
        }
    }

    /// Whether its numbers stay within 0 and 1.
    pub(crate) fn unit(&self) -> bool {
        matches!(self, Self::Metallic | Self::Roughness | Self::AlphaCutoff)
    }
}

/// A new material type's names for an entity named `name` (E10-2c): `Water` gives the base
/// `Water` (`WaterMaterial`, its parameters `WaterParams`) and the file stem `water`
/// (`water.slang`, `water.material`); a name ending in "Material" keeps one; no name, or
/// one starting with a digit, starts with `Custom`. While `taken(stem)`, a number is added
/// (`Water2`, `water_2`).
pub(crate) fn type_names(name: &str, taken: impl Fn(&str) -> bool) -> (String, String) {
    let mut words = crate::app::name_words(name);
    if words.last().is_some_and(|w| w == "material") && words.len() > 1 {
        words.pop();
    }
    if words
        .first()
        .is_none_or(|w| w.starts_with(|c: char| c.is_ascii_digit()))
    {
        words.insert(0, String::from("custom"));
    }
    let pascal: String = words
        .iter()
        .map(|w| w[..1].to_ascii_uppercase() + &w[1..])
        .collect();
    let snake = words.join("_");
    let candidate = |n: u32| {
        if n == 1 {
            (pascal.clone(), snake.clone())
        } else {
            (format!("{pascal}{n}"), format!("{snake}_{n}"))
        }
    };
    (1..u32::MAX)
        .map(&candidate)
        .find(|(_, stem)| !taken(stem))
        .unwrap_or_else(|| candidate(1))
}

/// A new material type's module (E10-2c): `{base}Material` with its parameters
/// `{base}Params` (a color, a roughness, a metallic), lit, its surface read straight from
/// them, every other part of `IMaterial` at its plain value; comments say where to write.
pub(crate) fn type_template(base: &str) -> String {
    format!(
        r#"// {base}Material, a material type (ADR 0092; docs/dev-guide/materials.md). Its
// parameters are the fields of {base}Params: each one a row in the inspector, its value
// kept by name in the material files that use it. Save (Ctrl+S) and what uses it is drawn
// with the change; until it compiles, and on an error, the previous version draws.
struct {base}Params
{{
    float4 base_color;
    float roughness;
    float metallic;
}};

struct {base}Material : IMaterial
{{
    typealias Params = {base}Params;

    // The shading model: MODEL_LIT, MODEL_UNLIT, MODEL_CLEARCOAT, MODEL_CLOTH, MODEL_HAIR,
    // MODEL_TOON, MODEL_SHEEN or MODEL_SHEEN_CLEARCOAT.
    static uint model()
    {{
        return MODEL_LIT;
    }}

    static {base}Params load(MaterialContext ctx)
    {{
        return material_params<{base}Params>(ctx);
    }}

    // What a point of the surface looks like: ctx has its place (world_position), normal,
    // side and texture coordinates.
    static Surface surface(MaterialContext ctx, {base}Params p, bool masked)
    {{
        Surface s;
        s.base = p.base_color;
        s.normal = normalize(ctx.normal * ctx.side);
        s.metallic = p.metallic;
        s.roughness = p.roughness;
        s.subsurface = 0;
        return surface_terms(s);
    }}

    static float occlusion(MaterialContext ctx, {base}Params p) {{ return 1.0; }}
    static float3 emission(MaterialContext ctx, {base}Params p) {{ return 0.0; }}
    static float alpha(MaterialContext ctx, {base}Params p) {{ return p.base_color.a; }}
    static float alpha_cutoff({base}Params p) {{ return 0.5; }}
    static float thickness(MaterialContext ctx, {base}Params p) {{ return 0.0; }}
}}
"#
    )
}

/// Where a new material type opens (E10-2c): at the start of its surface's first line of
/// its own (0-based line and byte column).
pub(crate) fn type_cursor(template: &str) -> (usize, usize) {
    template
        .lines()
        .enumerate()
        .find_map(|(k, l)| {
            l.trim_start()
                .starts_with("s.base")
                .then(|| (k, l.len() - l.trim_start().len()))
        })
        .unwrap_or((0, 0))
}

/// `file`'s rows into `out`, a material type's parameters from `layout` (none yet: the type
/// and its look only).
pub(crate) fn rows(file: &MaterialFile, layout: Option<&StructLayout>, out: &mut Vec<Row>) {
    out.clear();
    // A variant's base first (E10-4b).
    if !file.base.is_none() {
        out.push(Row::Base);
    }
    let own = if file.is_typed() {
        out.push(Row::Type);
        // Fields starting with `_` are the code's own (a graph's editing numbers, a struct's
        // placeholder, ADR 0123): not the material's rows.
        for field in layout
            .map_or(&[][..], |l| &l.fields[..])
            .iter()
            .filter(|f| !f.name.starts_with('_'))
        {
            out.push(Row::Param {
                name: field.name.clone(),
                kind: field.kind,
                count: field.count,
            });
            if field.kind == FieldKind::Texture
                && field.count == 1
                && file.textures.contains_key(&field.name)
            {
                out.push(Row::Encoding {
                    name: field.name.clone(),
                });
            }
        }
        out.push(Row::Look);
        file.look == Look::Own
    } else {
        true
    };
    if own {
        out.extend([Row::BaseColor, Row::Metallic, Row::Roughness, Row::Emissive]);
    }
    out.push(Row::Alpha);
    if file.alpha == Alpha::Mask {
        out.push(Row::AlphaCutoff);
    }
    out.push(Row::DoubleSided);
}

/// A row's numbers (parts it lacks are 0).
pub(crate) fn numbers(file: &MaterialFile, row: &Row) -> [f64; 4] {
    let mut out = [0.0; 4];
    let mut put = |values: &[f32]| {
        for (o, &v) in out.iter_mut().zip(values) {
            *o = f64::from(v);
        }
    };
    match row {
        Row::BaseColor => put(&file.base_color),
        Row::Emissive => put(&file.emissive),
        Row::Metallic => put(&[file.metallic]),
        Row::Roughness => put(&[file.roughness]),
        Row::AlphaCutoff => put(&[file.alpha_cutoff]),
        Row::Param { name, .. } => {
            if let Some(v) = file.numbers.get(name) {
                put(&[*v]);
            } else if let Some(v) = file.vectors.get(name) {
                put(v);
            }
        }
        _ => {}
    }
    out
}

/// A row's numbers set to `values` (as many as it has; within 0 and 1 where it says so,
/// whole where it is whole).
pub(crate) fn set_numbers(file: &mut MaterialFile, row: &Row, values: [f64; 4]) {
    let Shown::Numbers { parts, whole, .. } = row.shown() else {
        return;
    };
    let value = |k: usize| {
        let v = if whole { values[k].round() } else { values[k] };
        let v = if row.unit() { v.clamp(0.0, 1.0) } else { v };
        v as f32
    };
    match row {
        Row::BaseColor => file.base_color = [value(0), value(1), value(2), value(3)],
        Row::Emissive => file.emissive = [value(0), value(1), value(2)],
        Row::Metallic => file.metallic = value(0),
        Row::Roughness => file.roughness = value(0),
        Row::AlphaCutoff => file.alpha_cutoff = value(0),
        Row::Param { name, .. } => {
            if parts == 1 {
                file.vectors.remove(name);
                file.numbers.insert(name.clone(), value(0));
            } else {
                file.numbers.remove(name);
                file.vectors
                    .insert(name.clone(), (0..parts).map(value).collect());
            }
        }
        _ => {}
    }
}

/// A choice row's current option.
pub(crate) fn chosen(file: &MaterialFile, row: &Row) -> usize {
    match row {
        Row::Look => usize::from(file.look == Look::Replaced),
        Row::Alpha => match file.alpha {
            Alpha::Opaque => 0,
            Alpha::Mask => 1,
            Alpha::Blend => 2,
        },
        Row::Encoding { name } => match file.textures.get(name).map(|t| t.encoding) {
            Some(TextureEncoding::Data) => 1,
            Some(TextureEncoding::Normal) => 2,
            _ => 0,
        },
        _ => 0,
    }
}

/// A choice row set to option `i`.
pub(crate) fn choose(file: &mut MaterialFile, row: &Row, i: usize) {
    match row {
        Row::Look => file.look = if i == 1 { Look::Replaced } else { Look::Own },
        Row::Alpha => {
            file.alpha = match i {
                1 => Alpha::Mask,
                2 => Alpha::Blend,
                _ => Alpha::Opaque,
            }
        }
        Row::Encoding { name } => {
            if let Some(t) = file.textures.get_mut(name) {
                t.encoding = match i {
                    1 => TextureEncoding::Data,
                    2 => TextureEncoding::Normal,
                    _ => TextureEncoding::Color,
                };
            }
        }
        _ => {}
    }
}

/// The checkbox row turned the other way.
pub(crate) fn toggle(file: &mut MaterialFile, row: &Row) {
    if *row == Row::DoubleSided {
        file.double_sided = !file.double_sided;
    }
}

/// Whether a row differs from what it is without it: a variant's from its base's value
/// (`base`, E10-4b: one of the variant's own), else from its default (a parameter: whether
/// the file sets it).
pub(crate) fn changed(file: &MaterialFile, row: &Row, base: Option<&MaterialFile>) -> bool {
    let plain = MaterialFile::default();
    let d = base.unwrap_or(&plain);
    match row {
        Row::Base | Row::Type | Row::Cost | Row::Code | Row::Fallback => false,
        Row::Param { name, .. } => match base {
            Some(b) => {
                file.numbers.get(name) != b.numbers.get(name)
                    || file.vectors.get(name) != b.vectors.get(name)
                    || file.textures.get(name) != b.textures.get(name)
            }
            None => {
                file.numbers.contains_key(name)
                    || file.vectors.contains_key(name)
                    || file.textures.contains_key(name)
            }
        },
        Row::Encoding { name } => {
            let encoding = |f: &MaterialFile| f.textures.get(name).map(|t| t.encoding);
            match base {
                Some(b) => encoding(file) != encoding(b),
                None => encoding(file).is_some_and(|e| e != TextureEncoding::Color),
            }
        }
        Row::Look => file.look != d.look,
        Row::BaseColor => file.base_color != d.base_color,
        Row::Metallic => file.metallic != d.metallic,
        Row::Roughness => file.roughness != d.roughness,
        Row::Emissive => file.emissive != d.emissive,
        Row::Alpha => file.alpha != d.alpha,
        Row::AlphaCutoff => file.alpha_cutoff != d.alpha_cutoff,
        Row::DoubleSided => file.double_sided != d.double_sided,
    }
}

/// A row back to what it is without it: a variant's to its base's value (`base`, E10-4b:
/// the variant's own value gone), else to its default (a parameter: unset, zero; an image
/// none).
pub(crate) fn reset(file: &mut MaterialFile, row: &Row, base: Option<&MaterialFile>) {
    let plain = MaterialFile::default();
    let d = base.unwrap_or(&plain);
    match row {
        Row::Base | Row::Type | Row::Cost | Row::Code | Row::Fallback => {}
        Row::Param { name, .. } => {
            let key = name.clone();
            match d.numbers.get(name) {
                Some(&v) => file.numbers.insert(key.clone(), v),
                None => file.numbers.remove(name),
            };
            match d.vectors.get(name) {
                Some(v) => file.vectors.insert(key.clone(), v.clone()),
                None => file.vectors.remove(name),
            };
            match d.textures.get(name) {
                Some(&t) => file.textures.insert(key, t),
                None => file.textures.remove(name),
            };
        }
        Row::Encoding { name } => {
            let encoding = d
                .textures
                .get(name)
                .map_or(TextureEncoding::Color, |t| t.encoding);
            if let Some(t) = file.textures.get_mut(name) {
                t.encoding = encoding;
            }
        }
        Row::Look => file.look = d.look,
        Row::BaseColor => file.base_color = d.base_color,
        Row::Metallic => file.metallic = d.metallic,
        Row::Roughness => file.roughness = d.roughness,
        Row::Emissive => file.emissive = d.emissive,
        Row::Alpha => file.alpha = d.alpha,
        Row::AlphaCutoff => file.alpha_cutoff = d.alpha_cutoff,
        Row::DoubleSided => file.double_sided = d.double_sided,
    }
}

/// Image parameter `name` given image `image` at `path` (from the project's root), its
/// encoding kept, else `Normal` for a field named a normal map's and `Color` otherwise.
pub(crate) fn set_image(
    file: &mut MaterialFile,
    written: &mut AssetPaths,
    name: &str,
    image: AssetRef,
    path: &str,
) {
    let encoding =
        file.textures
            .get(name)
            .map(|t| t.encoding)
            .unwrap_or(if name.contains("normal") {
                TextureEncoding::Normal
            } else {
                TextureEncoding::Color
            });
    written.insert(image, path);
    file.textures
        .insert(name.to_owned(), MaterialTexture { image, encoding });
}

/// Values over a type's shading variants as a row says them: `35` or `35–40`; `None` for
/// none.
fn spread(values: impl Iterator<Item = u32>) -> Option<String> {
    let (lo, hi) = values.fold((u32::MAX, 0), |(lo, hi), v| (lo.min(v), hi.max(v)));
    match (lo, hi) {
        (u32::MAX, _) => None,
        (lo, hi) if lo == hi => Some(format!("{lo}")),
        (lo, hi) => Some(format!("{lo}–{hi}")),
    }
}

/// A material type's cost as its row says it (E10-4c): the fragment stage's registers over
/// its shading variants made (`35–40 registers`), the engine's own beside them, what spills,
/// and its parameters' bytes (`params`).
pub(crate) fn cost_text(
    cost: &engine_render::forward::MaterialCost,
    params: Option<u32>,
) -> String {
    use std::fmt::Write;
    let made = || cost.variants.iter().flatten();
    let mut text = String::new();
    if let Some(r) = spread(made().map(|v| v.registers)) {
        let _ = write!(text, "{r} registers");
    }
    if let Some(e) = spread(made().filter_map(|v| v.engine)) {
        let _ = write!(text, " (the engine's {e})");
    }
    let spill = made().map(|v| v.spill).max().unwrap_or(0);
    if spill > 0 {
        let _ = write!(text, ", {spill} B spilled");
    }
    if let Some(bytes) = params {
        let _ = write!(text, " · {bytes} B of parameters");
    }
    text
}

/// A material type's code as its row says it (E10-4f): the instructions and image samples
/// its fragment entry points reach over its shading variants made, the engine's own beside
/// them, and how long its last compile took; empty when nothing is known.
pub(crate) fn code_text(cost: &engine_render::forward::MaterialCost) -> String {
    use std::fmt::Write;
    let made = || cost.variants.iter().flatten();
    let code = || made().filter_map(|v| v.code);
    let engine = || made().filter_map(|v| v.engine_code);
    let mut text = String::new();
    if let Some(i) = spread(code().map(|c| c.instructions)) {
        let _ = write!(text, "{i} instructions");
        if let Some(e) = spread(engine().map(|c| c.instructions)) {
            let _ = write!(text, " (the engine's {e})");
        }
    }
    if let Some(s) = spread(code().map(|c| c.samples)) {
        let word = if s == "1" { "sample" } else { "samples" };
        let _ = write!(text, ", {s} {word}");
        if let Some(e) = spread(engine().map(|c| c.samples)) {
            let _ = write!(text, " (the engine's {e})");
        }
    }
    if let Some((took, entries)) = cost.compiled {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        let s = if entries == 1 { "" } else { "s" };
        let _ = write!(
            text,
            "compiled in {:.2} s, {entries} entry point{s}",
            took.as_secs_f64()
        );
    }
    text
}

/// The file name of the path `written` gives `asset` (`none` for none).
fn file_name(written: &AssetPaths, asset: AssetRef) -> &str {
    if asset.is_none() {
        return "none";
    }
    written
        .get(asset)
        .map_or("?", |p| p.rsplit('/').next().unwrap_or(p))
}

/// The rows' shown texts into `texts`, each row's range of them in `spans`.
fn texts(
    file: &MaterialFile,
    written: &AssetPaths,
    [cost, code]: [&str; 2],
    rows: &[Row],
    texts: &mut Vec<String>,
    spans: &mut Vec<(usize, usize)>,
) {
    // The next text, emptied (their room kept between frames).
    fn next(texts: &mut Vec<String>, n: &mut usize) -> usize {
        if *n == texts.len() {
            texts.push(String::new());
        }
        texts[*n].clear();
        *n += 1;
        *n - 1
    }
    let mut n = 0;
    spans.clear();
    for row in rows {
        let start = n;
        match row.shown() {
            Shown::Numbers { parts, whole, .. } => {
                let values = numbers(file, row);
                for v in values.iter().take(parts) {
                    let k = next(texts, &mut n);
                    push_number(&mut texts[k], *v, if whole { 0 } else { 3 });
                }
            }
            Shown::Choice(names) => {
                let k = next(texts, &mut n);
                texts[k].push_str(names[chosen(file, row).min(names.len() - 1)]);
            }
            Shown::Check => {}
            Shown::Image => {
                let k = next(texts, &mut n);
                let image = match row {
                    Row::Param { name, .. } => file.textures.get(name).map(|t| t.image),
                    Row::Base => Some(file.base),
                    _ => None,
                };
                texts[k].push_str(file_name(written, image.unwrap_or(AssetRef::NONE)));
            }
            Shown::Text => {
                let k = next(texts, &mut n);
                match row {
                    Row::Type => {
                        texts[k].push_str(&file.type_name);
                        texts[k].push_str(" · ");
                        texts[k].push_str(file_name(written, file.shader));
                    }
                    Row::Param { count, .. } => {
                        use std::fmt::Write;
                        let _ = write!(texts[k], "{count} values (an array)");
                    }
                    Row::Cost => texts[k].push_str(cost),
                    Row::Code => texts[k].push_str(code),
                    _ => {}
                }
            }
        }
        spans.push((start, n));
    }
    texts.truncate(n);
}

/// A material file the inspector edits: as the editor holds it (see the module
/// documentation).
pub(crate) struct MaterialDoc {
    pub(crate) id: AssetId,
    /// Its path from the project's root.
    pub(crate) path: String,
    pub(crate) file: MaterialFile,
    /// The paths its references were written with.
    pub(crate) written: AssetPaths,
    /// Its text as the editor holds it, and as its file holds it: unsaved while they
    /// differ.
    pub(crate) text: String,
    pub(crate) saved: String,
    /// How many times it changed, its type as the player registered it at that count, and
    /// the layout of the type's parameters with the layout's revision.
    pub(crate) revision: u64,
    pub(crate) ty: Option<(MaterialTypeId, u64)>,
    pub(crate) layout: Option<(u32, Result<StructLayout, String>)>,
    /// Given to the player since it last changed.
    pub(crate) given: bool,
    /// The change count its unsaved text was autosaved at, and when an autosave is due.
    pub(crate) autosaved: Option<u64>,
    pub(crate) due: Option<std::time::Instant>,
    /// Its rows, their shown texts and each row's range of them.
    pub(crate) rows: Vec<Row>,
    pub(crate) texts: Vec<String>,
    pub(crate) spans: Vec<(usize, usize)>,
    /// A variant's base, resolved, with the paths its references were written with
    /// (E10-4a): `file` is it under the variant's own values, and the text the variant's
    /// own lines.
    pub(crate) base: Option<(MaterialFile, AssetPaths)>,
    /// What its type's shading costs this GPU (E10-4c), as read at its layout's revision,
    /// and its row's text.
    pub(crate) cost: Option<(u32, engine_render::forward::MaterialCost)>,
    pub(crate) cost_text: String,
    /// Its code row's text (E10-4f): empty until known.
    pub(crate) code_text: String,
}

impl MaterialDoc {
    /// Material file `id` at `path` (from the project's root `root`), read over its bases
    /// (a variant's, E10-4a: each as `open` holds it, else from its file).
    pub(crate) fn open(
        root: &Path,
        id: AssetId,
        path: &str,
        open: &dyn Fn(AssetRef) -> Option<(MaterialFile, AssetPaths)>,
    ) -> Result<Self, String> {
        let text = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
        Self::from_text(id, path, &text, root, open)
    }

    /// Material file `id` at `path` from its text, read over its bases as [`Self::open`]
    /// reads them.
    pub(crate) fn from_text(
        id: AssetId,
        path: &str,
        text: &str,
        root: &Path,
        open: &dyn Fn(AssetRef) -> Option<(MaterialFile, AssetPaths)>,
    ) -> Result<Self, String> {
        let read = MaterialFile::read_with_bases(text, root, open).map_err(|e| match &e.path {
            Some(_) => e.to_string(),
            None => format!("{path}: {}", e.message),
        })?;
        let (file, written, base) = (read.file, read.report.assets, read.base);
        // As the editor writes it: unsaved only once edited.
        let text = match &base {
            Some((under, _)) => file.to_variant_text(id, &written, under),
            None => file.to_text_with(id, &written),
        };
        let mut doc = Self {
            id,
            path: path.to_owned(),
            file,
            written,
            base,
            cost: None,
            cost_text: String::new(),
            code_text: String::new(),
            saved: text.clone(),
            text,
            revision: 0,
            ty: None,
            layout: None,
            given: false,
            autosaved: None,
            due: None,
            rows: Vec::new(),
            texts: Vec::new(),
            spans: Vec::new(),
        };
        doc.refresh();
        Ok(doc)
    }

    /// It has edits its file does not.
    pub(crate) fn dirty(&self) -> bool {
        self.text != self.saved
    }

    /// `edit` applied to the file (and the paths its references are written with): its
    /// text before and after, when it changed.
    pub(crate) fn edit(
        &mut self,
        edit: impl FnOnce(&mut MaterialFile, &mut AssetPaths),
    ) -> Option<(String, String)> {
        edit(&mut self.file, &mut self.written);
        let after = self.file_text();
        if after == self.text {
            return None;
        }
        let before = core::mem::replace(&mut self.text, after.clone());
        self.given = false;
        self.revision += 1;
        self.refresh();
        Some((before, after))
    }

    /// Its text as the editor writes it: a variant's lines its base has not (E10-4a), else
    /// every value.
    fn file_text(&self) -> String {
        match &self.base {
            Some((under, _)) => self.file.to_variant_text(self.id, &self.written, under),
            None => self.file.to_text_with(self.id, &self.written),
        }
    }

    /// Text `text` of it read: over its base where it names the one it has (E10-4a).
    fn read_text(&self, text: &str) -> Result<(MaterialFile, AssetPaths), String> {
        let (own, report) = MaterialFile::parse(text).map_err(|e| format!("{}:{e}", self.path))?;
        match &self.base {
            Some((under, paths)) if !own.base.is_none() => {
                let (file, report) = MaterialFile::read_over(text, under)
                    .map_err(|e| format!("{}:{e}", self.path))?;
                let mut written = paths.clone();
                written.extend(&report.assets);
                Ok((file, written))
            }
            _ => Ok((own, report.assets)),
        }
    }

    /// A variant's base as it is now (`base`: its resolved values and their paths; E10-4a):
    /// its own values kept over it, and given to the player again. Whether it changed.
    pub(crate) fn set_base(&mut self, base: (MaterialFile, AssetPaths)) -> bool {
        if self.base.as_ref().map(|(b, _)| b) == Some(&base.0) {
            return false;
        }
        let Ok((file, report)) = MaterialFile::read_over(&self.text, &base.0) else {
            return false;
        };
        let mut written = base.1.clone();
        written.extend(&report.assets);
        self.file = file;
        self.written = written;
        self.base = Some(base);
        self.given = false;
        self.revision += 1;
        self.refresh();
        true
    }

    /// Its text put back (undo, redo).
    pub(crate) fn put_text(&mut self, text: &str) -> Result<(), String> {
        let (file, written) = self.read_text(text)?;
        self.file = file;
        self.written = written;
        self.text = text.to_owned();
        self.given = false;
        self.revision += 1;
        self.refresh();
        Ok(())
    }

    /// A material type's module (an absolute path) and type; `None` for the standard
    /// material or a reference without a path.
    pub(crate) fn module(&self, root: &Path) -> Option<(PathBuf, String)> {
        let typed = self.file.typed(&self.written, root).ok()??;
        Some((typed.shader, typed.type_name))
    }

    /// Its rows and their texts made again.
    pub(crate) fn refresh(&mut self) {
        let layout = self
            .layout
            .as_ref()
            .and_then(|(_, l)| l.as_ref().ok())
            .cloned();
        rows(&self.file, layout.as_ref(), &mut self.rows);
        // After its type: its cost once the type's shading has been made (E10-4c), its code
        // (E10-4f), and whether it is drawn as its fallback.
        if let Some(k) = self.rows.iter().position(|r| *r == Row::Type) {
            let mut at = k + 1;
            if self.cost.is_some() {
                self.rows.insert(at, Row::Cost);
                at += 1;
                if !self.code_text.is_empty() {
                    self.rows.insert(at, Row::Code);
                    at += 1;
                }
            }
            self.rows.insert(at, Row::Fallback);
        }
        texts(
            &self.file,
            &self.written,
            [&self.cost_text, &self.code_text],
            &self.rows,
            &mut self.texts,
            &mut self.spans,
        );
    }

    /// What its type's shading costs, as the player read it at layout revision `revision`:
    /// its cost row's text with its parameters' bytes.
    pub(crate) fn set_cost(&mut self, revision: u32, cost: engine_render::forward::MaterialCost) {
        let params = self
            .layout
            .as_ref()
            .and_then(|(_, l)| l.as_ref().ok())
            .map(|l| l.size);
        self.cost_text = cost_text(&cost, params);
        self.code_text = code_text(&cost);
        self.cost = Some((revision, cost));
        self.refresh();
    }

    /// Row `row`'s shown texts.
    pub(crate) fn row_texts(&self, row: usize) -> &[String] {
        self.spans
            .get(row)
            .map_or(&[][..], |&(a, b)| &self.texts[a..b])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_shader::Field;

    fn field(name: &str, kind: FieldKind, count: u32) -> Field {
        Field {
            name: name.to_owned(),
            offset: 0,
            kind,
            count,
            stride: 0,
        }
    }

    #[test]
    fn a_new_types_names_come_from_the_entity_and_are_numbered_when_taken() {
        let none = |_: &str| false;
        assert_eq!(type_names("Water", none), ("Water".into(), "water".into()));
        assert_eq!(
            type_names("Car Paint Material", none),
            ("CarPaint".into(), "car_paint".into())
        );
        assert_eq!(type_names("", none), ("Custom".into(), "custom".into()));
        // The words as New Script splits them: a capital after a digit starts one.
        assert_eq!(
            type_names("3D sign", none),
            ("Custom3DSign".into(), "custom_3_d_sign".into())
        );
        assert_eq!(
            type_names("Material", none),
            ("Material".into(), "material".into())
        );
        let taken = |stem: &str| stem == "water";
        assert_eq!(
            type_names("Water", taken),
            ("Water2".into(), "water_2".into())
        );
        let text = type_template("Water");
        assert!(text.contains("struct WaterMaterial : IMaterial"), "{text}");
        assert!(text.contains("struct WaterParams"), "{text}");
        let (line, col) = type_cursor(&text);
        assert!(text.lines().nth(line).unwrap()[col..].starts_with("s.base = p.base_color;"));
    }

    fn layout() -> StructLayout {
        StructLayout {
            name: String::from("VelvetParams"),
            size: 64,
            fields: vec![
                field("color", FieldKind::Float(4), 1),
                field("roughness", FieldKind::Float(1), 1),
                field("layers", FieldKind::Uint(1), 1),
                field("pattern", FieldKind::Texture, 1),
                field("points", FieldKind::Float(3), 2),
            ],
        }
    }

    #[test]
    fn a_standard_materials_rows_are_its_values() {
        let mut file = MaterialFile::default();
        let mut out = Vec::new();
        rows(&file, None, &mut out);
        assert_eq!(
            out,
            [
                Row::BaseColor,
                Row::Metallic,
                Row::Roughness,
                Row::Emissive,
                Row::Alpha,
                Row::DoubleSided
            ]
        );
        // Within 0 and 1; a mask brings its cutoff.
        set_numbers(&mut file, &Row::Metallic, [1.7, 0.0, 0.0, 0.0]);
        assert_eq!(file.metallic, 1.0);
        assert!(changed(&file, &Row::Metallic, None));
        choose(&mut file, &Row::Alpha, 1);
        rows(&file, None, &mut out);
        assert!(out.contains(&Row::AlphaCutoff));
        reset(&mut file, &Row::Metallic, None);
        assert!(!changed(&file, &Row::Metallic, None));
        toggle(&mut file, &Row::DoubleSided);
        assert!(file.double_sided);
    }

    #[test]
    fn a_material_types_rows_follow_its_params_and_set_them_by_name() {
        let mut file = MaterialFile {
            shader: AssetRef::file(AssetId::new()),
            type_name: String::from("VelvetMaterial"),
            ..MaterialFile::default()
        };
        let layout = layout();
        let mut out = Vec::new();
        rows(&file, Some(&layout), &mut out);
        let names: Vec<&str> = out.iter().map(Row::name).collect();
        assert_eq!(
            names,
            [
                "type",
                "color",
                "roughness",
                "layers",
                "pattern",
                "points",
                "look",
                "base_color",
                "metallic",
                "roughness",
                "emissive",
                "alpha",
                "double_sided"
            ]
        );
        assert_eq!(
            out[1].shown(),
            Shown::Numbers {
                parts: 4,
                whole: false,
                labels: RGBA
            }
        );
        assert_eq!(out[5].shown(), Shown::Text, "an array is shown, not edited");
        // A vector goes to `vectors`, a number to `numbers`, a whole number rounded.
        set_numbers(&mut file, &out[1], [0.4, 0.05, 0.05, 1.0]);
        set_numbers(&mut file, &out[2], [0.6, 0.0, 0.0, 0.0]);
        set_numbers(&mut file, &out[3], [2.6, 0.0, 0.0, 0.0]);
        assert_eq!(file.vectors["color"], [0.4, 0.05, 0.05, 1.0]);
        assert_eq!(file.numbers["roughness"], 0.6);
        assert_eq!(file.numbers["layers"], 3.0);
        assert_eq!(numbers(&file, &out[1])[1], f64::from(0.05f32));
        // An image, with its encoding's row; the look replaced hides the look's values.
        let mut written = AssetPaths::new();
        let image = AssetRef::file(AssetId::new());
        set_image(
            &mut file,
            &mut written,
            "pattern",
            image,
            "textures/velvet.png",
        );
        choose(&mut file, &Row::Look, 1);
        rows(&file, Some(&layout), &mut out);
        assert!(out.contains(&Row::Encoding {
            name: String::from("pattern")
        }));
        assert!(!out.contains(&Row::BaseColor));
        let pattern = Row::Param {
            name: String::from("pattern"),
            kind: FieldKind::Texture,
            count: 1,
        };
        assert!(changed(&file, &pattern, None));
        // A variant's (E10-4b): changed where it is not its base's; reset to its base's.
        let base = file.clone();
        let mut variant = file.clone();
        variant.base = AssetRef::file(AssetId::new());
        rows(&variant, Some(&layout), &mut out);
        assert_eq!(out[0], Row::Base, "a variant's base first");
        assert!(!changed(&variant, &pattern, Some(&base)));
        assert!(
            !changed(&variant, &out[3], Some(&base)),
            "roughness as its base's"
        );
        set_numbers(&mut variant, &out[3], [0.9, 0.0, 0.0, 0.0]);
        assert!(changed(&variant, &out[3], Some(&base)));
        let roughness = out[3].clone();
        reset(&mut variant, &roughness, Some(&base));
        assert_eq!(variant.numbers["roughness"], 0.6, "back to its base's");
        variant.textures.remove("pattern");
        assert!(changed(&variant, &pattern, Some(&base)));
        reset(&mut variant, &pattern, Some(&base));
        assert_eq!(variant.textures["pattern"].image, image);
        assert!(!changed(&variant, &Row::Metallic, Some(&base)));
    }

    #[test]
    fn a_doc_edits_its_text_and_takes_it_back() {
        let id = AssetId::new();
        let text = MaterialFile::default().to_text_with(id, &AssetPaths::new());
        let mut doc =
            MaterialDoc::from_text(id, "materials/red.material", &text, Path::new("."), &|_| {
                None
            })
            .unwrap();
        assert!(!doc.dirty() && !doc.given);
        assert_eq!(doc.row_texts(0), ["0.8", "0.8", "0.8", "1"]);
        let (before, after) = doc
            .edit(|f, _| set_numbers(f, &Row::Roughness, [0.25, 0.0, 0.0, 0.0]))
            .unwrap();
        assert!(after.contains("roughness = 0.25"), "{after}");
        assert!(doc.dirty());
        assert_eq!(doc.row_texts(2), ["0.25"]);
        // The same value again: no edit.
        assert!(
            doc.edit(|f, _| set_numbers(f, &Row::Roughness, [0.25, 0.0, 0.0, 0.0]))
                .is_none()
        );
        doc.put_text(&before).unwrap();
        assert_eq!(doc.file.roughness, MaterialFile::default().roughness);
        assert!(!doc.dirty());
    }
    /// E10-4a: a variant's doc shows its base's values under its own and writes only what
    /// it changes; its base changed, it follows, its own values kept; its text put back
    /// (undo) is read over the base again.
    #[test]
    fn a_variants_doc_writes_what_it_changes_and_follows_its_base() {
        let (red, blue) = (AssetId::new(), AssetId::new());
        let base = MaterialFile {
            base_color: [1.0, 0.0, 0.0, 1.0],
            roughness: 0.25,
            ..MaterialFile::default()
        };
        let mut paths = AssetPaths::new();
        paths.insert(AssetRef::file(red), "materials/red.material");
        let text = format!(
            "[material version=2 id={blue}]\n\nbase = asset(\"materials/red.material\", {red})\n\
             metallic = 1\n"
        );
        let open = |r: AssetRef| (r == AssetRef::file(red)).then(|| (base.clone(), paths.clone()));
        let mut doc = MaterialDoc::from_text(
            blue,
            "materials/blue.material",
            &text,
            Path::new("."),
            &open,
        )
        .unwrap();
        assert_eq!(doc.file.base_color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(doc.file.metallic, 1.0);
        assert!(!doc.dirty(), "{}", doc.text);
        let (_, after) = doc.edit(|f, _| f.roughness = 0.9).unwrap();
        assert!(after.contains("roughness = 0.9"), "{after}");
        assert!(after.contains("metallic = 1"), "{after}");
        assert!(
            after.contains("base = asset(\"materials/red.material\""),
            "{after}"
        );
        assert!(!after.contains("base_color"), "{after}");
        // The base changed: followed, its own values kept.
        let mut green = base.clone();
        green.base_color = [0.0, 1.0, 0.0, 1.0];
        assert!(doc.set_base((green.clone(), paths.clone())));
        assert_eq!(doc.file.base_color, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(doc.file.roughness, 0.9);
        assert!(!doc.given);
        assert!(
            !doc.set_base((green, paths.clone())),
            "the same base changes nothing"
        );
        // Undo: the text before, over the base as it is.
        doc.put_text(&text).unwrap();
        assert_eq!(doc.file.roughness, 0.25);
        assert_eq!(doc.file.base_color, [0.0, 1.0, 0.0, 1.0]);
    }

    /// E10-4c: a type's cost says its registers over the variants made, the engine's beside
    /// them, what spills and its parameters' bytes.
    #[test]
    fn a_types_cost_says_its_registers_beside_the_engines() {
        use engine_render::forward::{MaterialCost, VariantCost};
        let v = |registers, spill, engine| {
            Some(VariantCost {
                registers,
                spill,
                engine,
                code: None,
                engine_code: None,
            })
        };
        let cost = MaterialCost {
            variants: [v(35, 0, Some(47)), None, v(40, 0, Some(56)), None],
            compiled: None,
        };
        assert_eq!(
            cost_text(&cost, Some(48)),
            "35–40 registers (the engine's 47–56) · 48 B of parameters"
        );
        let one = MaterialCost {
            variants: [v(64, 256, None), None, None, None],
            compiled: None,
        };
        assert_eq!(cost_text(&one, None), "64 registers, 256 B spilled");
    }

    /// E10-4f: a type's code says the instructions and image samples its variants reach,
    /// the engine's beside them, and its last compile.
    #[test]
    fn a_types_code_says_its_instructions_and_samples_beside_the_engines() {
        use engine_render::forward::{MaterialCost, VariantCost};
        use engine_rhi::spirv::CodeSize;
        let size = |instructions, samples| {
            Some(CodeSize {
                instructions,
                samples,
            })
        };
        let v = |code, engine_code| {
            Some(VariantCost {
                registers: 40,
                spill: 0,
                engine: None,
                code,
                engine_code,
            })
        };
        let cost = MaterialCost {
            variants: [
                v(size(1240, 6), size(1180, 9)),
                None,
                v(size(1400, 6), size(1300, 9)),
                None,
            ],
            compiled: Some((std::time::Duration::from_millis(420), 3)),
        };
        assert_eq!(
            code_text(&cost),
            "1240–1400 instructions (the engine's 1180–1300), 6 samples (the engine's 9) · \
             compiled in 0.42 s, 3 entry points"
        );
        let one = MaterialCost {
            variants: [v(size(800, 1), None), None, None, None],
            compiled: Some((std::time::Duration::from_millis(250), 1)),
        };
        assert_eq!(
            code_text(&one),
            "800 instructions, 1 sample · compiled in 0.25 s, 1 entry point"
        );
        assert_eq!(code_text(&MaterialCost::default()), "");
    }
}
