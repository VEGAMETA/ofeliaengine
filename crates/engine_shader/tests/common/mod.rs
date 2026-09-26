//! Helpers shared by the Slang tests. They need the Slang library (Vulkan SDK, or
//! `ENGINE_SLANG_LIBRARY`); without it they print why and pass, unless
//! `ENGINE_REQUIRE_SLANG=1` (set on the GPU workflow and in development containers).

#![allow(dead_code, clippy::print_stdout, clippy::unwrap_used)]

use engine_shader::SlangLibrary;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn slang() -> Option<Arc<SlangLibrary>> {
    match SlangLibrary::load() {
        Ok(library) => {
            println!(
                "slang {} from {}",
                library.version(),
                library.path().display()
            );
            Some(library)
        }
        Err(e) if std::env::var("ENGINE_REQUIRE_SLANG").is_ok_and(|v| v == "1") => panic!("{e}"),
        Err(e) => {
            println!("skipping (set ENGINE_REQUIRE_SLANG=1 to make this an error): {e}");
            None
        }
    }
}

pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine_shader_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
