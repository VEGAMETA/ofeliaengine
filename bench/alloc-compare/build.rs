//! `--features segment-heap`: the system heap, with the process heap made a Windows 10
//! 2004+ segment heap by the application manifest.
fn main() {
    println!("cargo:rerun-if-changed=segment-heap.manifest");
    if std::env::var_os("CARGO_FEATURE_SEGMENT_HEAP").is_some() {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("segment-heap.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
