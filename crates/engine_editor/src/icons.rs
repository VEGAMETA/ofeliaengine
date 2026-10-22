//! Editor icons: glyphs of the bundled Symbols Nerd Font (`engine_text::fonts::NERD_SYMBOLS`,
//! ADR 0036), drawn as text with the UI font stack so they size, color and clip like text.
//!
//! Each constant names its Nerd Fonts glyph (`glyphnames.json` of Nerd Fonts 3.5.1). Most are
//! Material Design Icons (Apache-2.0); every set is credited in `THIRD_PARTY_NOTICES.md`.

/// Play (`md-play`).
pub const PLAY: &str = "\u{F040A}";
/// Pause (`md-pause`).
pub const PAUSE: &str = "\u{F03E4}";
/// Stop (`md-stop`).
pub const STOP: &str = "\u{F04DB}";
/// Closes a scene's tab (`md-close`).
pub const CLOSE: &str = "\u{F0156}";
/// An Output row whose text names its cause: a click shows it (`md-open_in_new`).
pub const LINK: &str = "\u{F03CC}";
/// An entity that is on, in the scene tree (`md-eye`).
pub const EYE: &str = "\u{F0208}";
/// An entity that is off (`md-eye_off`).
pub const EYE_OFF: &str = "\u{F0209}";
/// Expanded tree row (`md-chevron_down`).
pub const EXPANDED: &str = "\u{F0140}";
/// Collapsed tree row (`md-chevron_right`).
pub const COLLAPSED: &str = "\u{F0142}";

/// Folder (`md-folder`).
pub const FOLDER: &str = "\u{F024B}";
/// Open folder (`md-folder_open`).
pub const FOLDER_OPEN: &str = "\u{F0770}";
/// Scene file (`md-movie_open`).
pub const SCENE: &str = "\u{F0FCE}";
/// Luau script (`md-language_lua`).
pub const SCRIPT: &str = "\u{F08B1}";
/// Texture (`md-image`).
pub const TEXTURE: &str = "\u{F02E9}";
/// A model's file (`md-shape`): the icon a model has everywhere.
pub const MESH: &str = "\u{F0831}";
/// Sound (`md-music_note`).
pub const AUDIO: &str = "\u{F0387}";
/// Any other file (`md-file_outline`).
pub const FILE: &str = "\u{F0224}";

/// Spatial node (`md-axis_arrow`).
pub const NODE_3D: &str = "\u{F0D49}";
/// A model of a mesh from a file (`md-shape`).
pub const MODEL: &str = "\u{F0831}";
/// The built-in cube (`md-cube_outline`).
pub const CUBE: &str = "\u{F01A7}";
/// The built-in sphere (`md-sphere`).
pub const SPHERE: &str = "\u{F1954}";
/// The built-in plane (`md-square_outline`).
pub const PLANE: &str = "\u{F0763}";
/// The built-in cylinder (`md-cylinder`).
pub const CYLINDER: &str = "\u{F194E}";
/// Spot light (`md-spotlight_beam`).
pub const SPOT_LIGHT: &str = "\u{F04C9}";
/// Light (`md-lightbulb_outline`).
pub const LIGHT: &str = "\u{F0336}";
/// Sun / directional light (`md-white_balance_sunny`).
pub const SUN: &str = "\u{F05A8}";
/// Environment (`md-earth`).
pub const ENVIRONMENT: &str = "\u{F01E7}";
/// A cone brush (`md-cone`).
pub const CONE: &str = "\u{F194C}";
/// A torus brush (`md-tire`).
pub const TORUS: &str = "\u{F1896}";
/// A wedge brush, a ramp (`md-slope_uphill`).
pub const WEDGE: &str = "\u{F0E00}";
/// A stairs brush (`md-stairs`).
pub const STAIRS: &str = "\u{F04CD}";
/// A room, a hollow brush (`md-floor_plan`).
pub const ROOM: &str = "\u{F0821}";
/// A group of blockout brushes, and the add menu's blockout entries (`md-wall`).
pub const BLOCKOUT: &str = "\u{F07FE}";
/// Camera (`md-camera`).
pub const CAMERA: &str = "\u{F0100}";
/// Character (`md-account`).
pub const CHARACTER: &str = "\u{F0004}";
/// Collision shape (`md-shape_outline`).
pub const COLLISION: &str = "\u{F0832}";
/// UI control (`md-application_outline`).
pub const CONTROL: &str = "\u{F0614}";

/// Select tool (`md-cursor_default`).
pub const TOOL_SELECT: &str = "\u{F01C0}";
/// Move tool (`md-cursor_move`).
pub const TOOL_MOVE: &str = "\u{F01BE}";
/// Rotate tool (`md-rotate_3d_variant`).
pub const TOOL_ROTATE: &str = "\u{F0464}";
/// Scale tool (`md-resize`).
pub const TOOL_SCALE: &str = "\u{F0A68}";
/// Grid snapping (`md-grid`).
pub const SNAP: &str = "\u{F02C1}";
/// Filter / search box (`md-magnify`).
pub const SEARCH: &str = "\u{F0349}";
/// Match case (`cod-case_sensitive`, VS Code's own).
pub const MATCH_CASE: &str = "\u{EAB1}";
/// Whole words (`cod-whole_word`).
pub const WHOLE_WORD: &str = "\u{EB7E}";
/// The match before (`cod-arrow_up`).
pub const PREVIOUS: &str = "\u{EAA1}";
/// The next match (`cod-arrow_down`).
pub const NEXT: &str = "\u{EA9A}";
/// Replace the match (`cod-replace`).
pub const REPLACE: &str = "\u{EB3D}";
/// Replace every match (`cod-replace_all`).
pub const REPLACE_ALL: &str = "\u{EB3C}";
/// Add (`md-plus`).
pub const PLUS: &str = "\u{F0415}";
/// A checked box's mark (`md-check`).
pub const CHECK: &str = "\u{F012C}";
/// A list that opens (`md-menu_down`).
pub const MENU_DOWN: &str = "\u{F035D}";
/// A checkbox whose entities differ (`md-minus`).
pub const MIXED: &str = "\u{F0374}";
/// Back to the default value (`md-undo`).
pub const RESET: &str = "\u{F054C}";
/// Something needs a look (`md-alert`): a scene's file changed on disk since its unsaved
/// changes.
pub const ALERT: &str = "\u{F0026}";
