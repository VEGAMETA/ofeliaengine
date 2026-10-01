//! Builds the ACL shim against clones of ACL and RTM (`ACL_DIR`, `RTM_DIR`).

fn main() {
    let acl = std::env::var("ACL_DIR").expect("ACL_DIR: a clone of nfrechette/acl (v2.1.0)");
    let rtm = std::env::var("RTM_DIR").expect("RTM_DIR: a clone of nfrechette/rtm (v2.3.1)");
    cc::Build::new()
        .cpp(true)
        .file("src/acl_shim.cpp")
        .include(format!("{acl}/includes"))
        .include(format!("{rtm}/includes"))
        .flag_if_supported("/std:c++17")
        .flag_if_supported("/arch:AVX2")
        .define("NDEBUG", None)
        .opt_level(3)
        .compile("acl_shim");
    println!("cargo:rerun-if-changed=src/acl_shim.cpp");
    println!("cargo:rerun-if-env-changed=ACL_DIR");
    println!("cargo:rerun-if-env-changed=RTM_DIR");
}
