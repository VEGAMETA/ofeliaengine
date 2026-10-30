//! The project's assets (ADR 0108, ADR 0107 §5): every asset file under a game's folder
//! with its id, found by walking the folder and kept in `.ofelia/assets.index`, a cache
//! rebuilt from the files at any time.
//!
//! - **Ids come from the files:** our formats' headers (`[scene version=3 id=…]`,
//!   `[material version=1 id=…]`), and a `.meta` beside each foreign file (`[asset
//!   version=1 id=…]`, [`AssetMeta`]), written the first time the database meets the file
//!   without one.
//! - **A file met without its `.meta`** takes back the id of the file gone from the same
//!   path or, moved outside the editor, of a file gone with the same size and content (for
//!   files whose `.meta` the database wrote); else it gets a new one.
//! - **The index** keeps each file's size and modification time, so a scan of an unchanged
//!   project reads no file but the `.meta` files it checks are still there.
//! - **Two files with one id** (a file copied with its `.meta`): the later path gets a new
//!   id in a `.meta` of its own; our own files are reported, not written.
//!
//! The index, a line per file, its fields apart by tabs (id, kind, size, modification
//! time, content hash or `-`, path), then the gone files' lines marked `gone`:
//!
//! ```text
//! [assets version=1]
//! 5f2c0a3e9d2b47c88e51f0d4a7b3c912 model 1048576 1337258112000000000 - props/crate.glb
//! ```

use crate::{AssetId, AssetPaths, AssetRef, LoadError, ValueReport};
use engine_reflect::Reflect;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

/// The file beside a foreign asset (`<file>.meta`, ADR 0108): `[asset version=1 id=…]`, the
/// asset's id, then its importer's settings, only those that are not the defaults (a file
/// of defaults is its header alone).
///
/// ```text
/// [asset version=1 id=5f2c0a3e9d2b47c88e51f0d4a7b3c912]
///
/// texture.compression = Uncompressed
/// model.scale = 0.01
/// ```
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct AssetMeta {
    /// How an image is imported: an image file's own, or the images inside a model.
    pub texture: TextureImport,
    /// How a model (a glTF file) is imported.
    pub model: ModelImport,
}

/// How an image is imported (ADR 0108's E4-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub struct TextureImport {
    /// Whether it is block-compressed where there is a derived data cache (the editor's).
    pub compression: TextureCompression,
}

/// Whether an image is block-compressed (ADR 0041).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum TextureCompression {
    /// BC7 for color and data, BC5 for normal maps: a quarter of the memory.
    #[default]
    Auto,
    /// Kept as decoded, RGBA8: pixel art, gradients and text that BC7 would smudge.
    Uncompressed,
}

/// How a model is imported (ADR 0108's E4-9).
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct ModelImport {
    /// Its meshes' positions multiplied by this: 0.01 for a file in centimeters.
    #[reflect(min = 0.0001, max = 10000.0)]
    pub scale: f32,
    /// What the editor adds to its clips (ADR 0124's E12-3: question 7 keeps an imported
    /// clip's settings in its model's `.meta`, so a new export keeps them): their events.
    pub clips: Vec<ClipImport>,
}

impl Default for ModelImport {
    fn default() -> Self {
        Self {
            scale: 1.0,
            clips: Vec::new(),
        }
    }
}

/// What a model's `.meta` adds to one of its clips (ADR 0124's E12-3b, E12-3e): its events
/// and how it plays.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct ClipImport {
    /// The clip, by its name in the file (an unnamed one by the name the editor shows it
    /// by, `Clip 2`).
    pub name: String,
    /// Its events, in time order.
    pub events: Vec<ClipEvent>,
    /// It plays once and holds its last moment (a death, a door opening); false: it loops.
    pub once: bool,
    /// What of its root's motion root motion takes out of the pose (E12-3e): its crossing of
    /// the ground,
    pub root_across: bool,
    /// its rise and fall (a jump that lifts the character; off, the height stays in the
    /// pose),
    pub root_up: bool,
    /// and its turn about up.
    pub root_turn: bool,
}

impl Default for ClipImport {
    fn default() -> Self {
        Self {
            name: String::new(),
            events: Vec::new(),
            once: false,
            root_across: true,
            root_up: false,
            root_turn: true,
        }
    }
}

impl ClipImport {
    /// It says nothing beyond its name: no events, every setting its default.
    pub fn is_plain(&self) -> bool {
        self.events.is_empty() && !self.once && self.root_across && !self.root_up && self.root_turn
    }
}

/// A moment of a clip that something happens at (ADR 0124's E12-3b): when a character's
/// animator plays past it, scripts hear `on_animation_event(name)` and its sound plays at
/// the character.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct ClipEvent {
    /// Seconds into the clip.
    pub time: f32,
    /// What it is called ("footstep").
    pub name: String,
    /// What else it tells scripts ("left"); empty: nothing.
    pub data: String,
    /// A sound played at the character when it happens, by its path in the project; empty:
    /// none.
    pub sound: String,
}

impl ModelImport {
    /// The scale to apply: [`Self::scale`], or 1 for one that is not a positive number.
    pub fn applied_scale(&self) -> f32 {
        if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        }
    }
}

impl AssetMeta {
    /// The header's kind.
    pub const KIND: &'static str = "asset";
    /// The format's version.
    pub const VERSION: u32 = 1;

    /// The `.meta` beside `file` (`yard.glb.meta`).
    pub fn path(file: &Path) -> PathBuf {
        let mut path = file.as_os_str().to_owned();
        path.push(".meta");
        PathBuf::from(path)
    }

    /// The file's text, with its id: the header, then the settings that are not the
    /// defaults, a line each.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut all = String::new();
        crate::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut all);
        let mut defaults = String::new();
        crate::write_value(
            Self::KIND,
            Self::VERSION,
            Some(id),
            &Self::default(),
            None,
            &mut defaults,
        );
        // By path, not by place: a list with items writes more lines than the defaults'
        // empty one, and every line after it would pair with the wrong default.
        let defaults: HashSet<&str> = defaults.lines().collect();
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            // The header and the blank line after it are the same in both.
            if line.is_empty() || line.starts_with('[') || !defaults.contains(line) {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A `.meta` from its text.
    ///
    /// # Errors
    ///
    /// Not a `.meta`, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut meta = Self::default();
        let report = crate::read_value(Self::KIND, Self::VERSION, text, &mut meta)?;
        Ok((meta, report))
    }

    /// The id of `file`: its `.meta`'s, or a new one written to a new `.meta` beside it
    /// (what the editor does the first time it meets a file without one).
    ///
    /// # Errors
    ///
    /// The `.meta` cannot be read, is not one, or cannot be written.
    pub fn ensure(file: &Path) -> Result<AssetId, String> {
        let path = Self::path(file);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::id_in(&path, &text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = AssetId::new();
                std::fs::write(&path, Self::default().to_text(id))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(id)
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// The settings in the `.meta` beside `file`: the defaults when it has none or it
    /// cannot be read (the asset database reports a broken one).
    pub fn read(file: &Path) -> Self {
        std::fs::read_to_string(Self::path(file))
            .ok()
            .and_then(|text| Self::parse(&text).ok())
            .map_or_else(Self::default, |(meta, _)| meta)
    }

    /// Writes the `.meta` beside `file` with `id` and these settings.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn write(&self, file: &Path, id: AssetId) -> Result<(), String> {
        let path = Self::path(file);
        std::fs::write(&path, self.to_text(id)).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The id a `.meta`'s text at `path` names.
    fn id_in(path: &Path, text: &str) -> Result<AssetId, String> {
        let (_, report) = Self::parse(text).map_err(|e| format!("{}:{e}", path.display()))?;
        report
            .id
            .ok_or_else(|| format!("{}: its header has no id", path.display()))
    }
}

/// What an asset file is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetKind {
    /// A scene (`.scene`).
    Scene,
    /// A material file (`.material`).
    Material,
    /// A glTF file (`.glb`, `.gltf`): meshes, materials, a scene.
    Model,
    /// An image (`.png`, `.jpg`, `.jpeg`, `.ktx2`, `.hdr`, `.exr`, `.dds`).
    Texture,
    /// A sound (`.wav`, `.ogg`, `.opus`, `.flac`).
    Sound,
    /// A video (`.webm`).
    Video,
    /// A font (`.ttf`, `.otf`).
    Font,
    /// A Luau script (`.luau`).
    Script,
    /// A Slang module (`.slang`): a material type or the code it imports (ADR 0092).
    Shader,
    /// A mixer layout (`.mixer`): the buses and their faders (ADR 0121).
    Mixer,
    /// A material graph (`.mgraph`): a material type drawn as nodes (ADR 0123).
    Graph,
    /// A material graph's function (`.mfunc`): nodes called as one node (ADR 0123's
    /// E10-3g-2).
    Function,
}

/// Every kind with its name and extensions.
const KINDS: [(AssetKind, &str, &[&str]); 12] = [
    (AssetKind::Scene, "scene", &["scene"]),
    (AssetKind::Material, "material", &["material"]),
    (AssetKind::Model, "model", &["glb", "gltf"]),
    (
        AssetKind::Texture,
        "texture",
        &["png", "jpg", "jpeg", "ktx2", "hdr", "exr", "dds"],
    ),
    (AssetKind::Sound, "sound", &["wav", "ogg", "opus", "flac"]),
    (AssetKind::Video, "video", &["webm"]),
    (AssetKind::Font, "font", &["ttf", "otf"]),
    (AssetKind::Script, "script", &["luau"]),
    (AssetKind::Shader, "shader", &["slang"]),
    (AssetKind::Mixer, "mixer", &["mixer"]),
    (AssetKind::Graph, "graph", &["mgraph"]),
    (AssetKind::Function, "function", &["mfunc"]),
];

impl AssetKind {
    /// The kind of the file at `path`, if it is an asset. A Luau definition file
    /// (`types/ofelia.d.luau`, the editor's) declares types: no asset.
    pub fn of(path: &Path) -> Option<Self> {
        let file = path.file_name()?.to_str()?;
        if file.to_ascii_lowercase().ends_with(".d.luau") {
            return None;
        }
        let ext = path.extension()?.to_str()?;
        KINDS
            .iter()
            .find(|(_, _, exts)| exts.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .map(|(k, _, _)| *k)
    }

    /// The kind's name: `scene`, `material`, `model`, `texture`, `sound`, `video`, `font`,
    /// `script`, `shader`, `mixer`, `graph`.
    pub fn name(self) -> &'static str {
        KINDS
            .iter()
            .find(|(k, _, _)| *k == self)
            .map_or("asset", |(_, n, _)| n)
    }

    /// The kind named `name`.
    pub fn named(name: &str) -> Option<Self> {
        KINDS
            .iter()
            .find(|(_, n, _)| *n == name)
            .map(|(k, _, _)| *k)
    }

    /// Whether files of the kind carry their id inside (ours), not in a `.meta` beside
    /// them.
    pub fn own_id(self) -> bool {
        matches!(
            self,
            Self::Scene | Self::Material | Self::Mixer | Self::Graph | Self::Function
        )
    }
}

/// An asset file the database knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetEntry {
    /// Its id.
    pub id: AssetId,
    /// Its path under the project's folder, with `/` (`props/crate.glb`).
    pub path: String,
    /// What it is.
    pub kind: AssetKind,
    /// Its size when its id was last read.
    pub size: u64,
    /// Its modification time then (nanoseconds since 1970).
    pub modified: u64,
    /// Its content's hash, for files whose `.meta` the database wrote: how such a file
    /// moved outside the editor without its `.meta` is found again.
    pub hash: Option<u64>,
}

/// What a scan found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Asset files under the folder.
    pub files: usize,
    /// Files whose id was read (new, or changed since the index).
    pub read: usize,
    /// `.meta` files written (files met without one, or duplicate ids).
    pub metas_written: usize,
    /// Files met without a `.meta` that took back a gone file's id.
    pub found_again: usize,
    /// Indexed files not found any more.
    pub gone: usize,
    /// Problems (also in [`AssetDatabase::problems`]).
    pub problems: usize,
    /// Files written anew since the last scan: at the same path, with the same id, of
    /// another size or time. What was made from them is to be made again.
    pub changed: Vec<AssetId>,
}

/// The project's assets ([module](self)).
#[derive(Debug, Default)]
pub struct AssetDatabase {
    root: PathBuf,
    entries: Vec<AssetEntry>,
    by_id: HashMap<AssetId, usize>,
    by_path: HashMap<String, usize>,
    /// Files gone since they were indexed, kept to give their ids back.
    gone: Vec<AssetEntry>,
    /// A file's path and what is wrong with it.
    problems: Vec<(String, String)>,
}

/// The index's header.
const INDEX_HEADER: &str = "[assets version=1]";
/// Gone files kept for their ids, at most.
const GONE_KEPT: usize = 4096;

impl AssetDatabase {
    /// An empty database of the project in `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ..Self::default()
        }
    }

    /// The database of the project in `root` as its index left it (empty without one, or
    /// with an index this build cannot read): [`Self::scan`] brings it up to date.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let mut db = Self::new(root);
        if let Ok(text) = std::fs::read_to_string(Self::index_path(&db.root)) {
            db.read_index(&text);
        }
        db
    }

    /// Where the index of the project in `root` is kept.
    pub fn index_path(root: &Path) -> PathBuf {
        root.join(".ofelia").join("assets.index")
    }

    /// The project's folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every asset file known, in no particular order.
    pub fn entries(&self) -> &[AssetEntry] {
        &self.entries
    }

    /// The file with id `id`.
    pub fn get(&self, id: AssetId) -> Option<&AssetEntry> {
        self.by_id.get(&id).map(|&i| &self.entries[i])
    }

    /// The file at `path` (under the folder, with `/`).
    pub fn find(&self, path: &str) -> Option<&AssetEntry> {
        self.by_path.get(path).map(|&i| &self.entries[i])
    }

    /// What is wrong with files: unreadable ids, two files with one id.
    pub fn problems(&self) -> &[(String, String)] {
        &self.problems
    }

    /// Each file's path by its id (the whole file's reference): what scenes are written and
    /// loaded with, the id first and the path its readable fallback.
    pub fn paths(&self) -> AssetPaths {
        let mut paths = AssetPaths::new();
        for e in &self.entries {
            paths.insert(AssetRef::file(e.id), e.path.clone());
        }
        paths
    }

    /// Walks the folder and brings the database up to date: new and changed files read
    /// (a foreign file without a `.meta` given one), gone files kept apart for their ids.
    pub fn scan(&mut self) -> ScanReport {
        let mut found = Vec::new();
        walk(&self.root, &self.root, &mut found);
        found.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut report = ScanReport {
            files: found.len(),
            ..ScanReport::default()
        };
        self.problems.clear();
        // What is gone goes apart first, so a file met without its `.meta` can take back the
        // id of one that moved.
        let here: std::collections::HashSet<String> =
            found.iter().map(|f| f.path.clone()).collect();
        let mut old: HashMap<String, AssetEntry> = HashMap::with_capacity(self.entries.len());
        let mut vanished: Vec<AssetId> = Vec::new();
        for e in self.entries.drain(..) {
            if here.contains(&e.path) {
                old.insert(e.path.clone(), e);
            } else {
                vanished.push(e.id);
                self.gone.push(e);
            }
        }
        let mut kept = Vec::with_capacity(found.len());
        for Found {
            path,
            kind,
            size,
            modified,
            meta,
        } in found
        {
            let known = old.get(&path);
            let unchanged = known.is_some_and(|e| {
                e.size == size
                    && e.modified == modified
                    && e.kind == kind
                    && (kind.own_id() || meta)
            });
            if let (true, Some(e)) = (unchanged, known) {
                kept.push(e.clone());
                continue;
            }
            report.read += 1;
            match self.read_id(&path, kind, size, known, &mut report) {
                Ok((id, hash)) => {
                    // The same file, written anew (not only given its `.meta`).
                    if known
                        .is_some_and(|e| e.id == id && (e.size != size || e.modified != modified))
                    {
                        report.changed.push(id);
                    }
                    kept.push(AssetEntry {
                        id,
                        path,
                        kind,
                        size,
                        modified,
                        hash,
                    });
                }
                Err(why) => self.problems.push((path, why)),
            }
        }
        // Files gone since the last scan, but those found again: their ids kept a while.
        let ids: std::collections::HashSet<AssetId> = kept.iter().map(|e| e.id).collect();
        report.gone = vanished.iter().filter(|id| !ids.contains(id)).count();
        if self.gone.len() > GONE_KEPT {
            let extra = self.gone.len() - GONE_KEPT;
            self.gone.drain(..extra);
        }
        self.entries = kept;
        self.resolve_duplicates(&mut report);
        self.reindex();
        report.problems = self.problems.len();
        report
    }

    /// One file changed, appeared or went (a watcher's event): the database follows it.
    pub fn update(&mut self, path: &Path) {
        // A walk is a few milliseconds for thousands of files, and keeps renames, ids and
        // duplicates right; one file at a time is not worth its own path.
        let _ = path;
        self.scan();
    }

    /// Writes the index into `.ofelia/`.
    ///
    /// # Errors
    ///
    /// The folder or the file cannot be written.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::index_path(&self.root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = String::with_capacity(self.entries.len() * 96 + 32);
        text.push_str(INDEX_HEADER);
        text.push('\n');
        let mut line = |e: &AssetEntry, gone: bool| {
            let hash = e
                .hash
                .map_or_else(|| String::from("-"), |h| format!("{h:016x}"));
            let mark = if gone { "gone " } else { "" };
            let _ = writeln!(
                text,
                "{mark}{}\t{}\t{}\t{}\t{hash}\t{}",
                e.id,
                e.kind.name(),
                e.size,
                e.modified,
                e.path
            );
        };
        for e in &self.entries {
            line(e, false);
        }
        for e in &self.gone {
            line(e, true);
        }
        std::fs::write(path, text)
    }

    /// The id of the file at `path`: from its header (ours) or its `.meta`, a gone file's
    /// for one met without a `.meta`, else a new one written beside it. With the content's
    /// hash when the database read it.
    fn read_id(
        &mut self,
        path: &str,
        kind: AssetKind,
        size: u64,
        known: Option<&AssetEntry>,
        report: &mut ScanReport,
    ) -> Result<(AssetId, Option<u64>), String> {
        let full = self.root.join(path);
        if kind.own_id() {
            let text = read_head(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            return file_id(&text)
                .map(|id| (id, None))
                .ok_or_else(|| String::from("its header names no id (save it from the editor)"));
        }
        let meta = AssetMeta::path(&full);
        match std::fs::read_to_string(&meta) {
            Ok(text) => {
                let id = AssetMeta::id_in(&meta, &text)?;
                // The same file as before keeps its hash.
                let hash = known
                    .filter(|e| e.id == id && e.size == size)
                    .and_then(|e| e.hash);
                return Ok((id, hash));
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("{}: {e}", meta.display()));
            }
            Err(_) => {}
        }
        // Without its `.meta`: the id of what was at this path, or of a file gone with the
        // same content; else a new one.
        let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        let hash = engine_core::hash::state_hash64(&bytes);
        let again = known.map(|e| e.id).or_else(|| {
            let i = self.gone.iter().position(|g| g.path == path).or_else(|| {
                self.gone
                    .iter()
                    .position(|g| g.kind == kind && g.size == size && g.hash == Some(hash))
            })?;
            Some(self.gone.swap_remove(i).id)
        });
        let id = match again {
            Some(id) => {
                report.found_again += 1;
                id
            }
            None => AssetId::new(),
        };
        std::fs::write(&meta, AssetMeta::default().to_text(id))
            .map_err(|e| format!("{}: {e}", meta.display()))?;
        report.metas_written += 1;
        Ok((id, Some(hash)))
    }

    /// Two files with one id: the later path of a foreign pair gets a new id in its `.meta`;
    /// our own files are reported.
    fn resolve_duplicates(&mut self, report: &mut ScanReport) {
        let mut first: HashMap<AssetId, usize> = HashMap::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            let id = self.entries[i].id;
            let Some(&j) = first.get(&id) else {
                first.insert(id, i);
                continue;
            };
            let (earlier, later) = (self.entries[j].path.clone(), self.entries[i].path.clone());
            if self.entries[i].kind.own_id() {
                self.problems.push((
                    later,
                    format!(
                        "has the id of {earlier} (a copy: save it from the editor as a new file)"
                    ),
                ));
                continue;
            }
            let new = AssetId::new();
            let file = self.root.join(&later);
            // A copy keeps the settings it was copied with.
            match AssetMeta::read(&file).write(&file, new) {
                Ok(()) => {
                    self.entries[i].id = new;
                    report.metas_written += 1;
                    first.insert(new, i);
                }
                Err(e) => self.problems.push((later, e)),
            }
        }
    }

    fn reindex(&mut self) {
        self.by_id.clear();
        self.by_path.clear();
        for (i, e) in self.entries.iter().enumerate() {
            self.by_id.entry(e.id).or_insert(i);
            self.by_path.insert(e.path.clone(), i);
        }
    }

    /// Takes the entries of an index's text (lines it cannot read are skipped: the next
    /// scan reads those files again).
    fn read_index(&mut self, text: &str) {
        let mut lines = text.lines();
        if lines.next() != Some(INDEX_HEADER) {
            return;
        }
        for line in lines {
            let (gone, line) = match line.strip_prefix("gone ") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let mut f = line.splitn(6, '\t');
            let parsed = (|| {
                let id = AssetId::parse(f.next()?)?;
                let kind = AssetKind::named(f.next()?)?;
                let size = f.next()?.parse().ok()?;
                let modified = f.next()?.parse().ok()?;
                let hash = match f.next()? {
                    "-" => None,
                    h => Some(u64::from_str_radix(h, 16).ok()?),
                };
                let path = f.next()?.to_owned();
                Some(AssetEntry {
                    id,
                    path,
                    kind,
                    size,
                    modified,
                    hash,
                })
            })();
            if let Some(e) = parsed {
                if gone {
                    self.gone.push(e);
                } else {
                    self.entries.push(e);
                }
            }
        }
        self.reindex();
    }
}

/// The id a file of ours names in its header (`[<kind> version=… id=…]`), the first line
/// that is not empty or a comment.
pub fn file_id(text: &str) -> Option<AssetId> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    inner
        .split_whitespace()
        .find_map(|item| item.strip_prefix("id="))
        .and_then(AssetId::parse)
}

/// The first few kilobytes of a file, as text: enough for a header.
fn read_head(path: &Path) -> io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 4096];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// An asset file a walk found.
struct Found {
    /// Its path under the folder, with `/`.
    path: String,
    kind: AssetKind,
    size: u64,
    modified: u64,
    /// A `.meta` lies beside it.
    meta: bool,
}

/// The asset files under `dir`, each with whether a `.meta` lies beside it (seen in the
/// same listing: no file is opened). Folders whose name starts with `.` (`.ofelia`, `.git`)
/// and `target` are left out.
fn walk(root: &Path, dir: &Path, out: &mut Vec<Found>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    // This folder's files, apart from its subfolders' (which go straight into `out`).
    let mut files: Vec<Found> = Vec::new();
    let mut metas: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if !name.starts_with('.') && name != "target" {
                walk(root, &entry.path(), out);
            }
            continue;
        }
        if let Some(stem) = name.strip_suffix(".meta") {
            metas.push(stem.to_owned());
            continue;
        }
        let path = entry.path();
        let Some(kind) = AssetKind::of(&path) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push(Found {
            path: relative,
            kind,
            size: meta.len(),
            modified,
            meta: false,
        });
    }
    // The folder's `.meta` files, matched to its files by name.
    if !metas.is_empty() {
        metas.sort_unstable();
        for f in &mut files {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            f.meta = metas.binary_search_by(|m| m.as_str().cmp(name)).is_ok();
        }
    }
    out.append(&mut files);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ofelia-assetdb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("props")).unwrap();
        dir
    }

    /// What the database costs a project of 10,000 asset files in 100 folders: the first
    /// scan (each file read, a `.meta` written), the index written and read back, a scan of
    /// the unchanged project. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn measure_ten_thousand_assets() {
        let dir = temp("measure");
        for folder in 0..100 {
            let sub = dir.join(format!("assets/f{folder:03}"));
            std::fs::create_dir_all(&sub).unwrap();
            for i in 0..100 {
                std::fs::write(sub.join(format!("t{i:03}.png")), format!("{folder}/{i}")).unwrap();
            }
        }
        let t = std::time::Instant::now();
        let mut db = AssetDatabase::open(&dir);
        let first = db.scan();
        let first_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        db.save().unwrap();
        let save_ms = t.elapsed().as_secs_f64() * 1e3;
        let bytes = std::fs::metadata(AssetDatabase::index_path(&dir))
            .unwrap()
            .len();
        let mut best = [f64::MAX; 2];
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut db = AssetDatabase::open(&dir);
            let open_ms = t.elapsed().as_secs_f64() * 1e3;
            let t = std::time::Instant::now();
            let again = db.scan();
            let scan_ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!((again.files, again.read), (10_000, 0));
            best = [best[0].min(open_ms), best[1].min(scan_ms)];
        }
        println!(
            "10,000 files: first scan {first_ms:.0} ms ({} metas), index saved {save_ms:.1} ms ({} KB), opened {:.1} ms, unchanged scan {:.0} ms",
            first.metas_written,
            bytes / 1024,
            best[0],
            best[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kinds_by_extension() {
        assert_eq!(AssetKind::of(Path::new("a/b.GLB")), Some(AssetKind::Model));
        assert_eq!(AssetKind::of(Path::new("x.scene")), Some(AssetKind::Scene));
        assert_eq!(
            AssetKind::of(Path::new("w/water.slang")),
            Some(AssetKind::Shader)
        );
        assert_eq!(AssetKind::of(Path::new("x.rs")), None);
        assert_eq!(AssetKind::of(Path::new("x.glb.meta")), None);
        for (k, n, _) in KINDS {
            assert_eq!(AssetKind::named(n), Some(k));
            assert_eq!(k.name(), n);
        }
    }

    #[test]
    fn file_ids_come_from_headers() {
        let id = AssetId::from_u128(0x5f2c_0a3e_9d2b_47c8_8e51_f0d4_a7b3_c912);
        assert_eq!(
            file_id(&format!("# c\n\n[scene version=3 id={id}]\n")),
            Some(id)
        );
        assert_eq!(file_id("[material version=1]"), None);
        assert_eq!(file_id("hello"), None);
    }

    #[test]
    fn a_scan_gives_foreign_files_metas_and_finds_them_again() {
        let dir = temp("scan");
        std::fs::write(dir.join("props/crate.glb"), b"glTF crate").unwrap();
        std::fs::write(dir.join("props/rock.png"), b"png rock").unwrap();
        let scene = AssetId::new();
        std::fs::write(
            dir.join("main.scene"),
            format!("[scene version=3 id={scene}]\n"),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join(".ofelia")).unwrap();
        std::fs::write(dir.join(".ofelia/hidden.png"), b"not an asset").unwrap();
        let mut db = AssetDatabase::open(&dir);
        let report = db.scan();
        assert_eq!(
            (report.files, report.read, report.metas_written),
            (3, 3, 2),
            "{report:?}"
        );
        assert_eq!(db.find("main.scene").unwrap().id, scene);
        let crate_id = db.find("props/crate.glb").unwrap().id;
        assert_eq!(
            AssetMeta::ensure(&dir.join("props/crate.glb")).unwrap(),
            crate_id
        );
        assert_eq!(db.get(crate_id).unwrap().kind, AssetKind::Model);
        assert_eq!(
            db.paths().get(AssetRef::file(crate_id)),
            Some("props/crate.glb")
        );
        // Unchanged: nothing read again, also from the index.
        db.save().unwrap();
        let mut db = AssetDatabase::open(&dir);
        assert_eq!(db.entries().len(), 3);
        assert_eq!(db.scan().read, 0);
        // The crate moved outside the editor, its `.meta` left behind and deleted: its
        // content gives its id back.
        std::fs::rename(dir.join("props/crate.glb"), dir.join("crate.glb")).unwrap();
        std::fs::remove_file(dir.join("props/crate.glb.meta")).unwrap();
        let report = db.scan();
        assert_eq!((report.found_again, report.gone), (1, 0), "{report:?}");
        assert_eq!(db.find("crate.glb").unwrap().id, crate_id);
        // A copy with its `.meta` (and a setting): a new id for the later path, the setting
        // kept.
        let pixel_art = AssetMeta {
            texture: TextureImport {
                compression: TextureCompression::Uncompressed,
            },
            ..AssetMeta::default()
        };
        let rock = db.find("props/rock.png").unwrap().id;
        pixel_art.write(&dir.join("props/rock.png"), rock).unwrap();
        std::fs::copy(dir.join("props/rock.png"), dir.join("props/rock2.png")).unwrap();
        std::fs::copy(
            dir.join("props/rock.png.meta"),
            dir.join("props/rock2.png.meta"),
        )
        .unwrap();
        db.scan();
        let (a, b) = (
            db.find("props/rock.png").unwrap().id,
            db.find("props/rock2.png").unwrap().id,
        );
        assert_ne!(a, b);
        assert_eq!(AssetMeta::ensure(&dir.join("props/rock2.png")).unwrap(), b);
        assert_eq!(AssetMeta::read(&dir.join("props/rock2.png")), pixel_art);
        // A scene without an id is a problem, reported.
        std::fs::write(dir.join("old.scene"), "[scene version=3]\n").unwrap();
        db.scan();
        assert!(db.problems().iter().any(|(p, _)| p == "old.scene"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_names_the_files_written_anew() {
        let dir = temp("changed");
        std::fs::write(dir.join("props/crate.glb"), b"glTF crate").unwrap();
        std::fs::write(dir.join("props/rock.png"), b"png rock").unwrap();
        let mut db = AssetDatabase::open(&dir);
        assert!(
            db.scan().changed.is_empty(),
            "new files are not changed ones"
        );
        let crate_id = db.find("props/crate.glb").unwrap().id;
        // Written anew: named, its id kept. The rock, untouched, is not.
        std::fs::write(dir.join("props/crate.glb"), b"glTF crate, bigger").unwrap();
        let report = db.scan();
        assert_eq!(report.changed, [crate_id], "{report:?}");
        assert_eq!(db.find("props/crate.glb").unwrap().id, crate_id);
        assert!(db.scan().changed.is_empty(), "once");
        // Moved: not written anew.
        std::fs::rename(dir.join("props/rock.png"), dir.join("rock.png")).unwrap();
        std::fs::rename(dir.join("props/rock.png.meta"), dir.join("rock.png.meta")).unwrap();
        assert!(db.scan().changed.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metas_write_only_settings_that_are_not_the_defaults() {
        let id = AssetId::new();
        // Defaults: the header alone, as before settings existed.
        let text = AssetMeta::default().to_text(id);
        assert_eq!(
            text,
            format!(
                "[asset version=1 id={id}]

"
            )
        );
        let meta = AssetMeta {
            texture: TextureImport {
                compression: TextureCompression::Uncompressed,
            },
            model: ModelImport {
                scale: 0.01,
                ..ModelImport::default()
            },
        };
        let text = meta.to_text(id);
        assert_eq!(
            text,
            format!(
                "[asset version=1 id={id}]

texture.compression = Uncompressed
model.scale = 0.01
"
            )
        );
        let (read, report) = AssetMeta::parse(&text).unwrap();
        assert_eq!((read, report.id), (meta.clone(), Some(id)));
        // One setting back to its default: its line goes.
        let back = AssetMeta {
            model: ModelImport::default(),
            ..meta.clone()
        };
        assert!(!back.to_text(id).contains("model.scale"));
        // A scale that is not a positive number applies as 1.
        for scale in [0.0, -2.0, f32::NAN] {
            let model = ModelImport {
                scale,
                ..ModelImport::default()
            };
            assert_eq!(model.applied_scale(), 1.0);
        }
        // A clip's events (ADR 0124's E12-3b) round the file.
        let mut walking = meta.clone();
        walking.model.clips.push(ClipImport {
            name: String::from("Walk"),
            events: vec![
                ClipEvent {
                    time: 0.25,
                    name: String::from("footstep"),
                    data: String::from("left"),
                    sound: String::from("sounds/step.wav"),
                },
                ClipEvent {
                    time: 0.75,
                    name: String::from("footstep"),
                    ..ClipEvent::default()
                },
            ],
            ..ClipImport::default()
        });
        // A clip with settings and no events (E12-3e).
        walking.model.clips.push(ClipImport {
            name: String::from("Jump"),
            once: true,
            root_up: true,
            root_turn: false,
            ..ClipImport::default()
        });
        assert!(!walking.model.clips[1].is_plain());
        assert!(
            ClipImport {
                name: String::from("Idle"),
                ..ClipImport::default()
            }
            .is_plain()
        );
        let text = walking.to_text(id);
        println!("{text}");
        assert!(
            text.contains("model.clips.0.events.0.data = \"left\"")
                && text.contains("model.clips.0.events.0.sound = \"sounds/step.wav\"")
                && text.contains("model.clips.0.events.1.time = 0.75")
                && text.contains("model.clips.1.once = true")
                && text.contains("model.clips.1.root_up = true"),
            "{text}"
        );
        let (read, _) = AssetMeta::parse(&text).unwrap();
        assert_eq!(read, walking, "{text}");
        // Without a `.meta`, or with a broken one: the defaults.
        let dir = temp("meta-read");
        let file = dir.join("props/sign.png");
        assert_eq!(AssetMeta::read(&file), AssetMeta::default());
        std::fs::write(AssetMeta::path(&file), "not a meta").unwrap();
        assert_eq!(AssetMeta::read(&file), AssetMeta::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
