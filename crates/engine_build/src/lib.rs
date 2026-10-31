//! The one-button game build (ADR 0126): what goes into a game's folder and how it is made.
//!
//! - **The build file** ([`BuildFile`], `game.build` in the game's folder, committed): the
//!   game's name, its start scene and its other scenes, the folders and files always in
//!   (what scripts load by path), the kind of build and where it goes.
//! - **What goes in** ([`plan`]): the scenes and every file they reach: an `asset("path", …)`
//!   in a scene, a prefab, a material, a graph, a clip or a mixer file names one; a glTF
//!   file's buffers and images by their `uri`; each file's `.meta` beside it; the folders
//!   always in, whole. Dot-files and dot-folders (the editor's `.ofelia/`) never.
//! - **The build** ([`build`]): the game built by cargo in the build's profile (`release`
//!   for a Build, `shipping` for a Ship; the game's `editor` feature off), then its folder
//!   written ([`write_folder`]): the program as `<name>.exe`, `data/` with the files (those
//!   unchanged since the last build kept, those no longer in removed), the game's
//!   `sounds/` folder as `data/sounds.pack` (made again only when a sound changed; a
//!   released player reads the pack, ADR 0105), the material types the material files
//!   name compiled into `data/materials.pack` ([`materials`], G2: no Slang where the game
//!   runs), the textures compressed and the models' colliders cooked into
//!   `data/derived.pack` ([`derived`], G3a: what the player would otherwise compute on
//!   every run), an empty `plugins/` for the vendors' DLLs a player supplies (CLAUDE.md §6),
//!   and the third parties' notices.
//! - The player run from that folder takes the `data/` beside it as its project
//!   (`engine_app::run`).
//!
//! G1 of ADR 0126 copies the files as the player reads them while a game is developed; G2
//! compiles the material types ahead; G3a carries the derived data; the cooked packs of
//! G3b replace the models' and images' sources.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use engine_reflect::Reflect;
use engine_scene::{AssetId, LoadError, ValueReport};

pub mod derived;
pub mod materials;

use engine_assets::DERIVED_PACK;
use engine_assets::material_pack::MATERIAL_PACK;

/// The build file's name in a game's folder.
pub const BUILD_FILE: &str = "game.build";

/// The game's PSO record (ADR 0092, ADR 0126's G5a), beside its build file and committed
/// with it: the material types' entry points the editor's Play drew, merged at each Stop;
/// the build compiles only those of the types it names.
pub const PSO_RECORD: &str = "game.pso.ron";

/// The folder builds go to by default, under the game's folder (its `.gitignore` names it).
pub const DEFAULT_OUTPUT: &str = "builds";

/// The engine's third-party notices, written beside every build: the licenses of what it
/// is made of (MIT, Apache and BSD ask for them to travel with the program).
const NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES.md");

/// The folders a new build file keeps always in when the game has them: what scripts load
/// by path and what the player reads by folder (the sounds go as a pack instead).
const ALWAYS_FOLDERS: [&str; 2] = ["scripts", "loc"];

/// The game's sounds folder, the sound pack made of it in `data/`, and its compression
/// level (`ofeliaengine sounds build`'s).
const SOUNDS: &str = "sounds";
const SOUND_PACK: &str = "sounds.pack";
const SOUND_LEVEL: i32 = 19;

/// Files never read for the assets they name: they are not text.
const BINARY: [&str; 16] = [
    "png", "jpg", "jpeg", "ktx2", "dds", "bin", "glb", "wav", "ogg", "opus", "flac", "webm", "ttf",
    "otf", "pack", "exe",
];

/// The largest file read for the assets it names.
const TEXT_AT_MOST: u64 = 16 << 20;

/// What kind of build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Kind {
    /// Quick to make, for testing: cargo's `release` profile.
    #[default]
    Build = 0,
    /// What players get: the `shipping` profile (fat LTO, one codegen unit).
    Ship = 1,
}

impl Kind {
    /// Its cargo profile.
    pub fn profile(self) -> &'static str {
        match self {
            Self::Build => "release",
            Self::Ship => "shipping",
        }
    }
}

/// A game's build file (`game.build`).
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct BuildFile {
    /// The game's name: its program's (`<name>.exe`) and its folder's.
    pub name: String,
    /// The scene it starts in, by path under the game's folder.
    pub start: String,
    /// Its other scenes, by path.
    pub scenes: Vec<String>,
    /// Folders and files always in, whole, by path: what scripts load by path.
    pub always: Vec<String>,
    /// A Build (`release`) or a Ship (`shipping`), when none is asked for.
    pub kind: Kind,
    /// Where builds go, under the game's folder (or an absolute path); empty: `builds`.
    pub output: String,
}

impl BuildFile {
    /// Its kind in its file's header.
    pub const KIND: &'static str = "build";
    /// Its format's version.
    pub const VERSION: u32 = 1;

    /// A build file for game `name` starting in scene `start`, with the folders of
    /// [`ALWAYS_FOLDERS`] the game at `root` has, and its `.mixer` files at its root (the
    /// player's settings name them by path).
    pub fn for_game(root: &Path, name: &str, start: &str) -> Self {
        let mut always: Vec<String> = ALWAYS_FOLDERS
            .iter()
            .filter(|f| root.join(f).is_dir())
            .map(|f| (*f).to_owned())
            .collect();
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut mixers: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".mixer"))
                .collect();
            mixers.sort();
            always.extend(mixers);
        }
        Self {
            name: name.to_owned(),
            start: start.to_owned(),
            scenes: Vec::new(),
            always,
            kind: Kind::Build,
            output: String::from(DEFAULT_OUTPUT),
        }
    }

    /// The file's text, with its id.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut out = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut out);
        out
    }

    /// A build file from its text, and what reading it reported.
    ///
    /// # Errors
    ///
    /// Not a build file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// The build file of the game at `root`, made and written the first time (the game's
    /// package name, its `start` scene, else `scenes/main.scene`, else its first scene);
    /// whether it was made.
    ///
    /// # Errors
    ///
    /// A build file that cannot be read, a folder without a game's `Cargo.toml`, or the
    /// new file not written.
    pub fn open_or_make(root: &Path, start: Option<&str>) -> Result<(Self, bool), BuildError> {
        if root.join(BUILD_FILE).is_file() {
            return Self::load(root).map(|file| (file, false));
        }
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| {
            BuildError::new(format!(
                "{}: {e} (not a game's folder?)",
                root.join("Cargo.toml").display()
            ))
        })?;
        let name = manifest
            .lines()
            .find_map(|l| l.trim().strip_prefix("name").map(str::trim))
            .and_then(|l| l.strip_prefix('='))
            .map(|l| l.trim().trim_matches('"').to_owned())
            .unwrap_or_else(|| String::from("game"));
        let start = match start {
            Some(start) => start.to_owned(),
            None if root.join("scenes/main.scene").is_file() => String::from("scenes/main.scene"),
            None => first_scene(root, root).unwrap_or_default(),
        };
        let file = Self::for_game(root, &name, &start);
        let path = root.join(BUILD_FILE);
        std::fs::write(&path, file.to_text(AssetId::new()))
            .map_err(|e| BuildError::new(format!("{}: {e}", path.display())))?;
        Ok((file, true))
    }

    /// The build file of the game at `root`.
    ///
    /// # Errors
    ///
    /// When it cannot be read or parsed, with its path.
    pub fn load(root: &Path) -> Result<Self, BuildError> {
        let path = root.join(BUILD_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| BuildError::new(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| BuildError::new(format!("{}: {e}", path.display())))
    }
}

/// Why a build stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError {
    /// What went wrong, in a line or a few.
    pub what: String,
    /// The files named but not found, with what named them.
    pub missing: Vec<Missing>,
}

impl From<String> for BuildError {
    fn from(what: String) -> Self {
        Self::new(what)
    }
}

impl BuildError {
    fn new(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            missing: Vec::new(),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)?;
        for m in &self.missing {
            write!(f, "\n  {} (named by {})", m.path, m.from)?;
        }
        Ok(())
    }
}

impl std::error::Error for BuildError {}

/// A file named but not found: its path and the file that named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// The file that named it.
    pub from: String,
    /// Its path under the game's folder.
    pub path: String,
}

/// What goes into a build: the files by their paths under the game's folder (`/` between
/// folders), those the build makes itself in `data/`, and those named but not found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The files, sorted.
    pub files: BTreeSet<String>,
    /// Made by the build in `data/` (the sound pack): kept by [`write_folder`].
    pub made: BTreeSet<String>,
    /// Named but not found.
    pub missing: Vec<Missing>,
}

/// What goes into the build of the game at `root` by `file` (see the module): its scenes,
/// the folders always in, and every file they reach.
pub fn plan(root: &Path, file: &BuildFile) -> Plan {
    let mut plan = Plan::default();
    if root.join(SOUNDS).is_dir() {
        plan.made.insert(String::from(SOUND_PACK));
    }
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    for scene in std::iter::once(&file.start)
        .chain(&file.scenes)
        .filter(|s| !s.is_empty())
    {
        queue.push_back((normal(scene), String::from(BUILD_FILE)));
    }
    for always in &file.always {
        let path = normal(always);
        let full = root.join(&path);
        if full.is_dir() {
            let mut found = Vec::new();
            walk(root, &full, &mut found);
            found.sort();
            queue.extend(found.into_iter().map(|f| (f, String::from(BUILD_FILE))));
        } else {
            queue.push_back((path, String::from(BUILD_FILE)));
        }
    }
    while let Some((path, from)) = queue.pop_front() {
        if plan.files.contains(&path) {
            continue;
        }
        let full = root.join(&path);
        if !full.is_file() {
            if !plan.missing.iter().any(|m| m.path == path) {
                plan.missing.push(Missing { from, path });
            }
            continue;
        }
        plan.files.insert(path.clone());
        let meta = format!("{path}.meta");
        if root.join(&meta).is_file() {
            plan.files.insert(meta);
        }
        let ext = extension(&path);
        if BINARY.contains(&ext.as_str()) {
            continue;
        }
        let small = std::fs::metadata(&full).is_ok_and(|m| m.len() <= TEXT_AT_MOST);
        let Some(text) = small.then(|| std::fs::read_to_string(&full).ok()).flatten() else {
            continue;
        };
        for named in asset_paths(&text) {
            queue.push_back((named, path.clone()));
        }
        if ext == "gltf" {
            let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
            for uri in gltf_uris(&text) {
                let joined = if dir.is_empty() {
                    uri
                } else {
                    format!("{dir}/{uri}")
                };
                queue.push_back((normal(&joined), path.clone()));
            }
        }
    }
    plan
}

/// A path under the game's folder as plans keep it: `/` between folders, no `./`.
fn normal(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

/// The lower-case extension of `path`.
fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Every file under folder `dir` (of the game at `root`) by its path, but dot-files,
/// dot-folders and `.meta` files (each comes with its file).
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(".meta") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(normal(&rel.to_string_lossy()));
        }
    }
}

/// The paths `text` names as assets (`asset("models/fox.gltf#fox1", …)`: the file, its part
/// cut off), but the engine's built-in ones.
fn asset_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("asset(\"") {
        rest = &rest[at + 7..];
        let Some(end) = rest.find('"') else {
            break;
        };
        let named = &rest[..end];
        let file = named.split('#').next().unwrap_or("");
        if !file.is_empty() && !file.starts_with("builtin") {
            out.push(normal(file));
        }
        rest = &rest[end..];
    }
    out
}

/// The files a glTF file's `uri`s name (its buffers and images), percent-decoded; embedded
/// data (`data:`) none.
fn gltf_uris(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("\"uri\"") {
        rest = &rest[at + 5..];
        let Some(open) = rest.find('"') else {
            break;
        };
        // Only a colon and spaces between the key and its value.
        if !rest[..open].chars().all(|c| c == ':' || c.is_whitespace()) {
            continue;
        }
        rest = &rest[open + 1..];
        let Some(close) = rest.find('"') else {
            break;
        };
        let uri = &rest[..close];
        if !uri.starts_with("data:") && !uri.is_empty() {
            out.push(percent_decoded(uri));
        }
        rest = &rest[close + 1..];
    }
    out
}

/// `%20` and its kind decoded.
fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a build is asked for beyond its file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// A Build or a Ship in place of the file's.
    pub kind: Option<Kind>,
    /// Where builds go in place of the file's.
    pub output: Option<PathBuf>,
    /// The engine's shader folder (`forward.slang`, which material types compile with):
    /// needed when the game has typed materials.
    pub engine_shaders: Option<PathBuf>,
    /// The folder zipped as `<folder>.zip` beside it too (G5b).
    pub zip: bool,
}

/// A step of a build, as it goes: cargo's lines while the game builds, then the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step<'a> {
    /// What the files are: how many, how many bytes.
    Planned {
        /// Files.
        files: usize,
    },
    /// A line cargo printed (`Compiling engine_core …`, a warning).
    Cargo(&'a str),
    /// Textures compressed and colliders cooked into the derived data pack (G3a).
    Deriving,
    /// Material types compiling into the material pack (G2): how many.
    Materials {
        /// Types.
        types: usize,
    },
    /// The folder being written: files done of all.
    Writing {
        /// Done.
        done: usize,
        /// All.
        of: usize,
    },
}

/// A build made.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    /// Its folder.
    pub folder: PathBuf,
    /// Its program.
    pub program: PathBuf,
    /// The kind it was.
    pub kind: Kind,
    /// The files in `data/`.
    pub files: usize,
    /// The material types in its material pack.
    pub material_types: usize,
    /// What its derived data pack holds.
    pub derived: derived::Derived,
    /// Its size report beside the folder (`<folder>.report.txt`, G5b).
    pub report: PathBuf,
    /// Its zip, when asked for.
    pub zip: Option<PathBuf>,
    /// Their bytes.
    pub bytes: u64,
    /// Those copied this time (the others were kept).
    pub copied: usize,
    /// Those removed from the last build.
    pub removed: usize,
    /// Seconds cargo took.
    pub compile_seconds: f64,
    /// Seconds the whole build took.
    pub seconds: f64,
}

/// The game at `root` built by its build file `file` (see the module), `tell` told each
/// step.
///
/// # Errors
///
/// Files named but not found (all of them, with what named them), cargo failing (its last
/// lines), or the folder not written.
pub fn build(
    root: &Path,
    file: &BuildFile,
    options: &Options,
    mut tell: impl FnMut(Step<'_>),
) -> Result<Built, BuildError> {
    let started = Instant::now();
    let kind = options.kind.unwrap_or(file.kind);
    let mut plan = plan(root, file);
    if !plan.missing.is_empty() {
        let mut e = BuildError::new(format!(
            "{} file{} named but not found",
            plan.missing.len(),
            if plan.missing.len() == 1 { "" } else { "s" }
        ));
        e.missing = plan.missing;
        return Err(e);
    }
    // The material types, before cargo: a material that cannot be resolved stops the build
    // at once.
    let types = materials::material_types(root, &mut plan)?;
    let shaders = if types.is_empty() {
        None
    } else {
        plan.made.insert(String::from(MATERIAL_PACK));
        Some(options.engine_shaders.as_deref().ok_or_else(|| {
            BuildError::new(
                "the game has material types, and the engine's shader folder is not known",
            )
        })?)
    };
    plan.made.insert(String::from(DERIVED_PACK));
    tell(Step::Planned {
        files: plan.files.len(),
    });
    let compiled = Instant::now();
    let program = compile(root, kind, &mut tell)?;
    let compile_seconds = compiled.elapsed().as_secs_f64();
    let output = options.output.clone().unwrap_or_else(|| {
        let out = if file.output.is_empty() {
            DEFAULT_OUTPUT
        } else {
            file.output.as_str()
        };
        root.join(out)
    });
    let folder = output.join(folder_name(&file.name));
    // The derived data first: the images it stands in for are left out of the folder.
    tell(Step::Deriving);
    let jobs = engine_core::jobs::ThreadPool::builder()
        .thread_name("engine-build")
        .build()
        .map_err(|e| BuildError::new(format!("cannot start worker threads: {e:?}")))?;
    let derived = derived::derived_pack(
        root,
        &plan,
        &root.join(derived::EDITOR_CACHE),
        &folder.join("data").join(DERIVED_PACK),
        Some(&jobs),
    )?;
    for image in &derived.left_out {
        plan.files.remove(image);
    }
    let written = write_folder(root, &plan, &program, &file.name, &folder, &mut tell)?;
    if plan.made.contains(SOUND_PACK) {
        sound_pack(&root.join(SOUNDS), &folder.join("data").join(SOUND_PACK))?;
    }
    if let Some(shaders) = shaders {
        let key = materials::material_key(root, shaders, &plan, &types);
        let pack = folder.join("data").join(MATERIAL_PACK);
        if engine_assets::material_pack::MaterialPack::read_key(&pack) != Some(key) {
            tell(Step::Materials { types: types.len() });
            materials::material_pack(root, shaders, &types, key, &pack)?;
        }
    }
    let report = beside(&folder, "report.txt");
    std::fs::write(&report, size_report(&folder))
        .map_err(|e| BuildError::new(format!("{}: {e}", report.display())))?;
    let zip = if options.zip {
        Some(zip_folder(&folder)?)
    } else {
        None
    };
    Ok(Built {
        program: folder.join(format!("{}.exe", folder_name(&file.name))),
        report,
        zip,
        folder,
        kind,
        files: plan.files.len(),
        material_types: types.len(),
        derived,
        bytes: written.bytes,
        copied: written.copied,
        removed: written.removed,
        compile_seconds,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The game at `root` built by cargo in `kind`'s profile, its `editor` feature off (its
/// own `.cargo/config.toml` deciding where: a target folder set around it is not used);
/// cargo's lines told; its program's path.
fn compile(
    root: &Path,
    kind: Kind,
    tell: &mut impl FnMut(Step<'_>),
) -> Result<PathBuf, BuildError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut child = Command::new(cargo)
        .args(["build", "--profile", kind.profile()])
        .arg("--message-format=json-render-diagnostics")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BuildError::new(format!("cargo did not start: {e}")))?;
    // Its messages on stdout (the program's path), its human lines on stderr.
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut program = None;
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if line.contains("\"reason\":\"compiler-artifact\"")
                    && let Some(exe) = json_string(&line, "\"executable\":\"")
                {
                    program = Some(PathBuf::from(exe));
                }
            }
        }
        program
    });
    let mut last: VecDeque<String> = VecDeque::new();
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            tell(Step::Cargo(&line));
            if last.len() == 20 {
                last.pop_front();
            }
            last.push_back(line);
        }
    }
    let status = child
        .wait()
        .map_err(|e| BuildError::new(format!("cargo: {e}")))?;
    let program = reader.join().ok().flatten();
    if !status.success() {
        let lines: Vec<String> = last.into_iter().collect();
        return Err(BuildError::new(format!(
            "the game did not build:\n{}",
            lines.join("\n")
        )));
    }
    program.ok_or_else(|| BuildError::new("the game built, but cargo named no program"))
}

/// The string after `key` in a line of JSON, its escapes undone (`\\` and `\"`).
fn json_string(line: &str, key: &str) -> Option<String> {
    let at = line.find(key)? + key.len();
    let mut out = String::new();
    let mut chars = line[at..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// A name a folder and a program can have: letters, digits, spaces, `-` and `_` kept, the
/// rest `_`; `game` for none.
pub fn folder_name(name: &str) -> String {
    let kept: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if kept.is_empty() {
        String::from("game")
    } else {
        kept
    }
}

/// What writing a build's folder did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    /// Bytes of `data/`.
    pub bytes: u64,
    /// Files copied (new or changed).
    pub copied: usize,
    /// Files of the last build no longer in, removed.
    pub removed: usize,
}

/// Build folder `folder` written for the game at `root`: `program` as `<name>.exe`, the
/// plan's files under `data/` (one of the same size and no older kept), what `data/` had
/// beyond them removed, `plugins/`, the notices.
///
/// # Errors
///
/// When a file cannot be copied or written, with its path.
pub fn write_folder(
    root: &Path,
    plan: &Plan,
    program: &Path,
    name: &str,
    folder: &Path,
    tell: &mut impl FnMut(Step<'_>),
) -> Result<Written, BuildError> {
    let io = |what: &Path, e: std::io::Error| BuildError::new(format!("{}: {e}", what.display()));
    let data = folder.join("data");
    std::fs::create_dir_all(&data).map_err(|e| io(&data, e))?;
    let plugins = folder.join("plugins");
    std::fs::create_dir_all(&plugins).map_err(|e| io(&plugins, e))?;
    let exe = folder.join(format!("{}.exe", folder_name(name)));
    std::fs::copy(program, &exe).map_err(|e| io(&exe, e))?;
    let notices = folder.join("THIRD_PARTY_NOTICES.txt");
    if std::fs::read_to_string(&notices).ok().as_deref() != Some(NOTICES) {
        std::fs::write(&notices, NOTICES).map_err(|e| io(&notices, e))?;
    }
    let mut written = Written::default();
    let of = plan.files.len();
    for (k, path) in plan.files.iter().enumerate() {
        let from = root.join(path);
        let to = data.join(path);
        let source = std::fs::metadata(&from).map_err(|e| io(&from, e))?;
        written.bytes += source.len();
        let same = std::fs::metadata(&to).is_ok_and(|t| {
            t.len() == source.len()
                && matches!((t.modified(), source.modified()), (Ok(a), Ok(b)) if a >= b)
        });
        if !same {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
            }
            std::fs::copy(&from, &to).map_err(|e| io(&to, e))?;
            written.copied += 1;
        }
        tell(Step::Writing { done: k + 1, of });
    }
    // What the last build had and this one has not.
    let mut there = Vec::new();
    walk_all(&data, &data, &mut there);
    for path in there {
        if !plan.files.contains(&path) && !plan.made.contains(&path) {
            let full = data.join(&path);
            std::fs::remove_file(&full).map_err(|e| io(&full, e))?;
            written.removed += 1;
        }
    }
    remove_empty(&data);
    Ok(written)
}

/// Sound folder `folder` packed into `pack` (ADR 0105), unless the pack is newer than every
/// file in it.
fn sound_pack(folder: &Path, pack: &Path) -> Result<(), BuildError> {
    let newest = |dir: &Path| {
        let mut files = Vec::new();
        walk_all(dir, dir, &mut files);
        files
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok()?.modified().ok())
            .max()
    };
    let packed = std::fs::metadata(pack).and_then(|m| m.modified()).ok();
    if packed.is_some() && newest(folder) <= packed {
        return Ok(());
    }
    engine_assets::sounds::write_sound_pack(folder, pack, SOUND_LEVEL)
        .map(|_| ())
        .map_err(|e| BuildError::new(format!("{}: {e}", pack.display())))
}

/// The file `<folder's name>.<extension>` beside `folder`.
fn beside(folder: &Path, extension: &str) -> PathBuf {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    folder.with_file_name(format!("{name}.{extension}"))
}

/// How many of the largest files a size report lists.
const REPORT_LARGEST: usize = 15;

/// A build folder's size report (G5b): its bytes in all, then by part (the program, each
/// pack, each folder of `data/`), largest first, then its largest files.
pub fn size_report(folder: &Path) -> String {
    use std::fmt::Write as _;
    let mut files = Vec::new();
    walk_all(folder, folder, &mut files);
    let mut sized: Vec<(String, u64)> = files
        .into_iter()
        .map(|f| {
            let bytes = std::fs::metadata(folder.join(&f)).map_or(0, |m| m.len());
            (f, bytes)
        })
        .collect();
    let total: u64 = sized.iter().map(|(_, b)| b).sum();
    let mut parts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (path, bytes) in &sized {
        let part = match path.split('/').collect::<Vec<_>>().as_slice() {
            ["data", file] => format!("data/{file}"),
            ["data", dir, ..] => format!("data/{dir}/"),
            [file] => (*file).to_owned(),
            [dir, ..] => format!("{dir}/"),
            [] => String::new(),
        };
        *parts.entry(part).or_default() += bytes;
    }
    let mut parts: Vec<(String, u64)> = parts.into_iter().collect();
    parts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sized.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mb = |b: u64| b as f64 / 1e6;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {:.1} MB in {} files",
        folder.display(),
        mb(total),
        sized.len()
    );
    let _ = writeln!(out, "\nBy part:");
    for (part, bytes) in &parts {
        let share = if total == 0 {
            0.0
        } else {
            *bytes as f64 * 100.0 / total as f64
        };
        let _ = writeln!(out, "{:>10.2} MB {share:>5.1} %  {part}", mb(*bytes));
    }
    let _ = writeln!(out, "\nLargest files:");
    for (path, bytes) in sized.iter().take(REPORT_LARGEST) {
        let _ = writeln!(out, "{:>10.2} MB  {path}", mb(*bytes));
    }
    out
}

/// `folder` zipped as `<folder>.zip` beside it by Windows' own `tar` (no dependency): the
/// folder whole, under its own name.
///
/// # Errors
///
/// `tar` missing or failing.
pub fn zip_folder(folder: &Path) -> Result<PathBuf, BuildError> {
    let zip = beside(folder, "zip");
    let _ = std::fs::remove_file(&zip);
    let (Some(parent), Some(name)) = (folder.parent(), folder.file_name()) else {
        return Err(BuildError::new(format!(
            "{}: no parent folder",
            folder.display()
        )));
    };
    let tar = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .filter(|t| t.is_file())
        .unwrap_or_else(|| PathBuf::from("tar"));
    let status = Command::new(tar)
        .arg("-a")
        .arg("-c")
        .arg("-f")
        .arg(&zip)
        .arg("-C")
        .arg(parent)
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| BuildError::new(format!("tar did not start: {e}")))?;
    if !status.status.success() {
        return Err(BuildError::new(format!(
            "{}: {}",
            zip.display(),
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    Ok(zip)
}

/// The first `.scene` under `at`, by its path from `root`, in the folders' sorted order
/// (dot-folders, `target` and `builds` passed over).
fn first_scene(root: &Path, at: &Path) -> Option<String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(at)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.') || n == "target" || n == DEFAULT_OUTPUT)
        })
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(found) = first_scene(root, &path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|e| e == "scene") {
            return path
                .strip_prefix(root)
                .ok()
                .map(|p| normal(&p.to_string_lossy()));
        }
    }
    None
}

/// Every file under `dir` by its path from `base`, dot-files included.
fn walk_all(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_all(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(normal(&rel.to_string_lossy()));
        }
    }
}

/// Empty folders under `dir` removed (not `dir`).
fn remove_empty(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_empty(&path);
            let _ = std::fs::remove_dir(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ofelia-build-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    /// A scene naming a material, a model and a built-in; the material naming a texture;
    /// the glTF naming its buffer and a percent-encoded image; a `.meta`; scripts always
    /// in (but their dot-files); a file named and missing.
    fn game(root: &Path) {
        write(
            root,
            "scenes/main.scene",
            "[scene version=3 id=00]\n\n[entity 01]\n\
             Model.mesh = asset(\"builtin#cube\", 01)\n\
             Model.material = asset(\"materials/paint.material\", 02)\n\
             [entity 02]\nModel.mesh = asset(\"models/fox.gltf#fox1\", 03)\n",
        );
        write(
            root,
            "materials/paint.material",
            "[material version=2 id=02]\n\nbase_color_texture = asset(\"textures/paint.png\", 04)\n",
        );
        write(root, "textures/paint.png", "png");
        write(root, "textures/paint.png.meta", "[meta version=1 id=04]\n");
        write(
            root,
            "models/fox.gltf",
            "{\"buffers\": [{\"uri\" : \"fox.bin\"}], \"images\": [{\"uri\": \"fur%20coat.png\"}, \
             {\"uri\": \"data:image/png;base64,AAAA\"}]}",
        );
        write(root, "models/fox.bin", "bin");
        write(root, "models/fur coat.png", "png");
        write(root, "scripts/door.luau", "return {}");
        write(root, "scripts/.hidden", "x");
        write(
            root,
            "unused/thing.material",
            "[material version=2 id=09]\n",
        );
    }

    #[test]
    fn a_plan_holds_the_scenes_and_what_they_reach() {
        let root = project("plan");
        game(&root);
        write(&root, "sounds/door.wav", "RIFF");
        let mut file = BuildFile::for_game(&root, "My game", "scenes/main.scene");
        assert_eq!(file.always, ["scripts"], "the sounds go as a pack");
        let plan = plan(&root, &file);
        let files: Vec<&str> = plan.files.iter().map(String::as_str).collect();
        assert_eq!(
            files,
            [
                "materials/paint.material",
                "models/fox.bin",
                "models/fox.gltf",
                "models/fur coat.png",
                "scenes/main.scene",
                "scripts/door.luau",
                "textures/paint.png",
                "textures/paint.png.meta",
            ]
        );
        assert!(plan.missing.is_empty(), "{:?}", plan.missing);
        assert_eq!(plan.made.iter().collect::<Vec<_>>(), ["sounds.pack"]);
        // A scene named and missing: said with what named it.
        file.scenes.push(String::from("scenes/gone.scene"));
        std::fs::remove_file(root.join("models/fox.bin")).unwrap();
        let plan = super::plan(&root, &file);
        assert_eq!(
            plan.missing,
            [
                Missing {
                    from: String::from(BUILD_FILE),
                    path: String::from("scenes/gone.scene"),
                },
                Missing {
                    from: String::from("models/fox.gltf"),
                    path: String::from("models/fox.bin"),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder's size report by part and largest file; the folder zipped beside it.
    #[test]
    fn a_folder_is_reported_by_size_and_zipped() {
        let root = project("report");
        let folder = root.join("builds/game");
        write(&folder, "game.exe", &"x".repeat(3000));
        write(&folder, "data/derived.pack", &"x".repeat(5000));
        write(&folder, "data/scenes/main.scene", &"x".repeat(1000));
        write(&folder, "data/scenes/other.scene", &"x".repeat(1000));
        let report = size_report(&folder);
        assert!(report.contains("0.0 MB in 4 files"), "{report}");
        let parts: Vec<&str> = report
            .lines()
            .skip_while(|l| *l != "By part:")
            .skip(1)
            .take_while(|l| !l.is_empty())
            .map(|l| l.rsplit("  ").next().unwrap())
            .collect();
        assert_eq!(parts, ["data/derived.pack", "game.exe", "data/scenes/"]);
        let zip = zip_folder(&folder).unwrap();
        assert_eq!(zip, root.join("builds/game.zip"));
        let bytes = std::fs::read(&zip).unwrap();
        assert_eq!(&bytes[..4], b"PK\x03\x04", "a zip");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_build_file_reads_back_as_written() {
        let root = project("file");
        game(&root);
        write(&root, "main.mixer", "[mixer version=4 id=05]\n");
        let mut file = BuildFile::for_game(&root, "My game", "scenes/main.scene");
        file.scenes.push(String::from("scenes/two.scene"));
        file.kind = Kind::Ship;
        assert_eq!(file.always, ["scripts", "main.mixer"]);
        let text = file.to_text(AssetId::new());
        let (back, report) = BuildFile::parse(&text).unwrap();
        assert_eq!(back, file, "{text}");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(Kind::Ship.profile(), "shipping");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_keeps_what_did_not_change_and_drops_what_left() {
        let root = project("folder");
        game(&root);
        write(&root, "fake.exe", "program");
        let file = BuildFile::for_game(&root, "My: game", "scenes/main.scene");
        let plan = plan(&root, &file);
        let folder = root.join("builds").join(folder_name(&file.name));
        let mut steps = 0;
        let mut tell = |_: Step<'_>| steps += 1;
        let first = write_folder(
            &root,
            &plan,
            &root.join("fake.exe"),
            &file.name,
            &folder,
            &mut tell,
        )
        .unwrap();
        assert_eq!(first.copied, plan.files.len());
        assert!(folder.join("My_ game.exe").is_file());
        assert!(folder.join("plugins").is_dir());
        assert!(folder.join("THIRD_PARTY_NOTICES.txt").is_file());
        assert!(folder.join("data/models/fur coat.png").is_file());
        // Again, unchanged: nothing copied; a file no longer in goes; what the build
        // makes stays.
        write(&folder, "data/old/stale.scene", "gone");
        let mut plan = plan;
        plan.made.insert(String::from("sounds.pack"));
        write(&folder, "data/sounds.pack", "pack");
        let again = write_folder(
            &root,
            &plan,
            &root.join("fake.exe"),
            &file.name,
            &folder,
            &mut tell,
        )
        .unwrap();
        assert_eq!((again.copied, again.removed), (0, 1));
        assert!(!folder.join("data/old").exists(), "its empty folder too");
        assert!(folder.join("data/sounds.pack").is_file());
        assert!(steps >= 2 * plan.files.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_and_names_are_kept_plain() {
        assert_eq!(normal("./a\\b/../c.scene"), "a/c.scene");
        assert_eq!(folder_name("  "), "game");
        assert_eq!(
            json_string(
                r#"{"executable":"C:\\a b\\game.exe","fresh":true}"#,
                "\"executable\":\""
            ),
            Some(String::from(r"C:\a b\game.exe"))
        );
        assert_eq!(
            asset_paths("x = asset(\"a/b.gltf#p\", 1) y = asset(\"builtin#cube\", 2)"),
            ["a/b.gltf"]
        );
    }
}
