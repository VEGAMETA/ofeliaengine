//! Strings that name something else (ADR 0107's E11-3d, the team's 4a): a field marked as
//! a sound (`Hint::Sound`) names a file under the sounds folder by its path, one marked as
//! a bus (`Hint::Bus`) a mixer bus by its name. A rename follows them: in the world, in
//! the lines undo keeps of a component, and in scenes' text.

use engine_ecs::{ComponentId, Entity, QueryState, World};
use engine_reflect::{Hint, TypeInfo, TypeKind};
use engine_scene::{SceneId, text};

use crate::History;

/// A scene type's strings marked with one hint.
#[derive(Debug)]
struct Marks {
    /// The type's name: its lines in a scene start with it (`AudioSource3D.source.sound`).
    name: &'static str,
    /// Each string: its offset in the value, and its path below the type (`source.sound`).
    fields: Vec<(usize, String)>,
}

/// The strings marked with one hint in every type scenes save.
#[derive(Debug)]
pub(crate) struct Marked {
    types: Vec<Marks>,
}

impl Marked {
    /// The strings marked `hint` in the program's scene types (its scripts' too).
    pub(crate) fn of(hint: Hint) -> Self {
        let registry = engine_scene::SceneRegistry::from_program();
        let mut types = Vec::new();
        for info in registry.types() {
            let mut fields = Vec::new();
            collect(info, hint, 0, &mut String::new(), &mut fields);
            if !fields.is_empty() {
                types.push(Marks {
                    name: info.name,
                    fields,
                });
            }
        }
        Self { types }
    }

    /// The marks of type `info`, by its name (scene types' names are unique).
    fn of_type(&self, info: &TypeInfo) -> Option<&Marks> {
        self.types.iter().find(|m| m.name == info.name)
    }

    /// Whether `key`, a scene line's path (`AudioSource3D.source.sound`), is a marked
    /// string's.
    fn is_marked(&self, key: &str) -> bool {
        let (name, path) = text::component_of(key);
        self.types
            .iter()
            .any(|m| m.name == name && m.fields.iter().any(|(_, p)| p == path))
    }

    /// The scene entities' marked strings naming `name`: each entity, its component, and
    /// the string's offset in it.
    fn find_in_world(&self, world: &mut World, name: &str) -> Vec<(Entity, ComponentId, usize)> {
        let mut q = QueryState::<(Entity, &SceneId)>::new(world);
        let entities: Vec<Entity> = q.iter_readonly(world).map(|(e, _)| e).collect();
        let reg = world.components();
        let mut found = Vec::new();
        for e in entities {
            let Some(ids) = world.component_ids_of(e) else {
                continue;
            };
            for &id in ids {
                let Some(marks) = reg.type_info(id).and_then(|info| self.of_type(info)) else {
                    continue;
                };
                let Some(ptr) = world.get_raw(e, id) else {
                    continue;
                };
                for &(offset, _) in &marks.fields {
                    // SAFETY: the entity's live value of the component, whose type holds a
                    // `String` at `offset`.
                    let value = unsafe { engine_reflect::read_str(ptr.as_ptr().add(offset)) };
                    if value == name {
                        found.push((e, id, offset));
                    }
                }
            }
        }
        found
    }

    /// How many of the scene entities' marked strings name `name`.
    pub(crate) fn count_in_world(&self, world: &mut World, name: &str) -> usize {
        self.find_in_world(world, name).len()
    }

    /// Every scene entity's marked string naming `from` set to `to`, each entity touched in
    /// `history` first when one is given (in a step it began): how many strings were.
    pub(crate) fn rename_in_world(
        &self,
        world: &mut World,
        mut history: Option<&mut History>,
        from: &str,
        to: &str,
    ) -> usize {
        let found = self.find_in_world(world, from);
        let mut touched: Vec<Entity> = Vec::new();
        for &(e, id, offset) in &found {
            if let Some(h) = history.as_deref_mut()
                && !touched.contains(&e)
            {
                h.touch(world, e);
                touched.push(e);
            }
            if let Some(ptr) = world.get_raw_mut(e, id) {
                // SAFETY: the entity's live value of the component, whose type holds a
                // `String` at `offset`.
                unsafe { engine_reflect::write_str(ptr.as_ptr().add(offset), to) };
            }
        }
        found.len()
    }

    /// Whether `line` (a scene's) is a marked string naming `name`.
    pub(crate) fn names(&self, line: &str, name: &str) -> bool {
        text::split_assignment(line.trim()).is_some_and(|(key, value)| {
            self.is_marked(key) && text::unquote(value).is_ok_and(|v| v == name)
        })
    }

    /// How many of a scene's lines (`text`) are marked strings naming `name`.
    pub(crate) fn uses_in_scene(&self, text: &str, name: &str) -> usize {
        text.lines().filter(|line| self.names(line, name)).count()
    }

    /// A scene's `text` with each marked string naming `from` naming `to`, every other byte
    /// kept: the text and how many lines changed, or none when no line names it.
    pub(crate) fn rename_in_scene(
        &self,
        text: &str,
        from: &str,
        to: &str,
    ) -> Option<(String, usize)> {
        rename_lines(text, from, to, |key| self.is_marked(key))
    }

    /// The lines undo keeps of a component of type `info` (paths below the type, without
    /// its name) with each marked string naming `from` naming `to`; none when none does.
    pub(crate) fn rename_in_lines(
        &self,
        info: &TypeInfo,
        text: &str,
        from: &str,
        to: &str,
    ) -> Option<String> {
        let marks = self.of_type(info)?;
        rename_lines(text, from, to, |key| {
            marks.fields.iter().any(|(_, p)| p == key)
        })
        .map(|(text, _)| text)
    }
}

/// The strings marked `hint` in a value of type `ty` at `base`: their offsets and their
/// paths after `path`, as a scene writes them (a newtype's field adds no step, a skipped
/// field is not written).
fn collect(
    ty: &'static TypeInfo,
    hint: Hint,
    base: usize,
    path: &mut String,
    out: &mut Vec<(usize, String)>,
) {
    if let Some(f) = text::newtype_field(ty) {
        if matches!(f.ty.kind, TypeKind::String) {
            if f.attrs.hint == hint && !path.is_empty() {
                out.push((base + f.offset, path.clone()));
            }
        } else {
            collect(f.ty, hint, base + f.offset, path, out);
        }
        return;
    }
    let TypeKind::Struct(fields) = ty.kind else {
        return;
    };
    for f in fields {
        if f.attrs.skip {
            continue;
        }
        let len = path.len();
        if len > 0 {
            path.push('.');
        }
        path.push_str(f.name);
        match f.ty.kind {
            TypeKind::String if f.attrs.hint == hint => out.push((base + f.offset, path.clone())),
            TypeKind::Struct(_) => collect(f.ty, hint, base + f.offset, path, out),
            _ => {}
        }
        path.truncate(len);
    }
}

/// `text`'s lines with each one `marked` says is a marked string's, and whose string is
/// `from`, naming `to`; their indentation and line ends kept: the text and how many, or
/// none.
fn rename_lines(
    text: &str,
    from: &str,
    to: &str,
    marked: impl Fn(&str) -> bool,
) -> Option<(String, usize)> {
    let mut out: Option<String> = None;
    let mut n = 0;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim_start();
        let renamed = text::split_assignment(trimmed)
            .filter(|&(key, value)| marked(key) && text::unquote(value).is_ok_and(|v| v == from));
        if let Some((key, _)) = renamed {
            let o = out.get_or_insert_with(|| text[..start].to_owned());
            o.push_str(&body[..body.len() - trimmed.len()]);
            o.push_str(key);
            o.push_str(" = ");
            text::quote_into(o, to);
            o.push_str(&line[body.len()..]);
            n += 1;
        } else if let Some(o) = out.as_mut() {
            o.push_str(line);
        }
    }
    out.map(|o| (o, n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_app::{AudioSource, AudioSource3D};
    use engine_reflect::Reflect;

    fn paths(marked: &Marked, name: &str) -> Vec<String> {
        marked
            .types
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.fields.iter().map(|(_, p)| p.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn the_marks_are_the_sources_sounds_and_buses() {
        let sounds = Marked::of(Hint::Sound);
        assert_eq!(paths(&sounds, AudioSource::TYPE_INFO.name), ["sound"]);
        assert_eq!(
            paths(&sounds, AudioSource3D::TYPE_INFO.name),
            ["source.sound"]
        );
        let buses = Marked::of(Hint::Bus);
        assert_eq!(paths(&buses, AudioSource::TYPE_INFO.name), ["bus"]);
        assert_eq!(paths(&buses, AudioSource3D::TYPE_INFO.name), ["source.bus"]);
        // Each mark at its field's place in the value.
        let at = |name: &str| {
            sounds
                .types
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.fields[0].0)
                .unwrap()
        };
        let s = AudioSource3D::new("x");
        let base = (&raw const s).cast::<u8>();
        let field = (&raw const s.source.sound).cast::<u8>();
        // SAFETY: both pointers are into `s`.
        let offset = unsafe { field.offset_from(base) } as usize;
        assert_eq!(at(AudioSource3D::TYPE_INFO.name), offset);
    }

    #[test]
    fn a_scene_names_the_new_path_only_where_a_marked_string_named_the_old() {
        let sounds = Marked::of(Hint::Sound);
        let scene = "[scene version=4 id=0123456789abcdef0123456789abcdef]\n\
            \n\
            [entity 3f35825513cd79099f8c4e4b1b3a8ccc]\n\
            AudioSource3D.source.sound = \"hum.wav\"\n\
            AudioSource3D.source.bus = \"hum.wav\"\n\
            Name = \"hum.wav\"\n\
            \n\
            [entity 40fca0fe9d48fb820ff2a121c1560a27]\n\
            AudioSource.sound = \"hum.wav\"\r\n\
            AudioSource.sound = \"humming.wav\"\n";
        assert_eq!(sounds.uses_in_scene(scene, "hum.wav"), 2);
        let (renamed, n) = sounds
            .rename_in_scene(scene, "hum.wav", "ambience/hum \"2\".wav")
            .unwrap();
        assert_eq!(n, 2);
        let expected = scene
            .replace(
                "AudioSource3D.source.sound = \"hum.wav\"",
                "AudioSource3D.source.sound = \"ambience/hum \\\"2\\\".wav\"",
            )
            .replace(
                "AudioSource.sound = \"hum.wav\"\r\n",
                "AudioSource.sound = \"ambience/hum \\\"2\\\".wav\"\r\n",
            );
        assert_eq!(
            renamed, expected,
            "the bus, the name and the other sound kept"
        );
        assert_eq!(sounds.uses_in_scene(&renamed, "ambience/hum \"2\".wav"), 2);
        assert!(sounds.rename_in_scene(scene, "gone.wav", "x.wav").is_none());
        // A bus is renamed by the bus marks alone.
        let buses = Marked::of(Hint::Bus);
        let (renamed, n) = buses.rename_in_scene(scene, "hum.wav", "voice").unwrap();
        assert_eq!(n, 1);
        assert!(renamed.contains("AudioSource3D.source.bus = \"voice\"\n"));
        assert!(renamed.contains("AudioSource3D.source.sound = \"hum.wav\"\n"));
    }

    #[test]
    fn undos_lines_name_the_new_path() {
        let sounds = Marked::of(Hint::Sound);
        let lines = "source.sound = \"hum.wav\"\nsource.bus = \"music\"\nsource.volume = 1\n";
        let renamed = sounds
            .rename_in_lines(AudioSource3D::TYPE_INFO, lines, "hum.wav", "a/hum.wav")
            .unwrap();
        assert_eq!(
            renamed,
            "source.sound = \"a/hum.wav\"\nsource.bus = \"music\"\nsource.volume = 1\n"
        );
        assert!(
            sounds
                .rename_in_lines(AudioSource::TYPE_INFO, lines, "hum.wav", "a/hum.wav")
                .is_none(),
            "a plain source's lines name `sound`"
        );
    }

    #[test]
    fn the_worlds_marked_strings_follow_a_rename_in_one_step() {
        let mut world = World::new();
        let a = world.spawn((SceneId::new(), AudioSource3D::new("hum.wav")));
        let b = world.spawn((SceneId::new(), AudioSource::new("hum.wav")));
        let c = world.spawn((SceneId::new(), AudioSource::new("other.wav")));
        world.get_mut::<AudioSource>(c).unwrap().bus = String::from("hum.wav");
        let sounds = Marked::of(Hint::Sound);
        assert_eq!(sounds.count_in_world(&mut world, "hum.wav"), 2);
        let mut history = History::new();
        history.begin("Rename", None);
        let n = sounds.rename_in_world(&mut world, Some(&mut history), "hum.wav", "a/hum.wav");
        history.commit(&mut world);
        assert_eq!(n, 2);
        assert_eq!(
            world.get::<AudioSource3D>(a).unwrap().source.sound,
            "a/hum.wav"
        );
        assert_eq!(world.get::<AudioSource>(b).unwrap().sound, "a/hum.wav");
        assert_eq!(
            world.get::<AudioSource>(c).unwrap().bus,
            "hum.wav",
            "a bus kept"
        );
        history.undo(&mut world);
        assert_eq!(
            world.get::<AudioSource3D>(a).unwrap().source.sound,
            "hum.wav"
        );
        assert_eq!(world.get::<AudioSource>(b).unwrap().sound, "hum.wav");
    }
}
