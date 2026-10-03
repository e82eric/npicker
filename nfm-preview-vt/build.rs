use std::{env, path::PathBuf};
fn main() {
    for name in [
        "GHOSTTY_ROOT",
        "NFM_GHOSTTY_VT_INCLUDE_DIR",
        "NFM_GHOSTTY_VT_LIB_DIR",
        "NFM_GHOSTTY_VT_LIB_NAME",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let root = env::var_os("GHOSTTY_ROOT").map(PathBuf::from);
    let required = |name: &str, relative: &str| {
        env::var_os(name).map(PathBuf::from)
            .or_else(|| root.as_ref().map(|root| root.join(relative)))
            .unwrap_or_else(|| panic!("{name} or GHOSTTY_ROOT must select a standalone Ghostty VT build; see nfm-preview-vt/README.md"))
    };
    let include = required("NFM_GHOSTTY_VT_INCLUDE_DIR", "include");
    // A text-preview build avoids image-codec symbol collisions with Skia hosts.
    let preview_lib = root.as_ref().map(|root| root.join("zig-out/nfm-vt/lib"));
    let lib = if env::var_os("NFM_GHOSTTY_VT_LIB_DIR").is_none()
        && preview_lib.as_ref().is_some_and(|path| path.is_dir())
    {
        preview_lib.unwrap()
    } else {
        required("NFM_GHOSTTY_VT_LIB_DIR", "zig-out/lib")
    };
    assert!(
        include.join("ghostty/vt/terminal.h").is_file(),
        "VT headers not found in {}",
        include.display()
    );
    assert!(
        lib.is_dir(),
        "VT library directory not found: {}",
        lib.display()
    );
    cc::Build::new()
        .file("src/preview_vt.c")
        .define("GHOSTTY_STATIC", None)
        .include(&include)
        .warnings(true)
        .compile("nfm_preview_vt_shim");
    println!("cargo:rustc-link-search=native={}", lib.display());
    let name = env::var("NFM_GHOSTTY_VT_LIB_NAME").unwrap_or_else(|_| "ghostty-vt-static".into());
    println!("cargo:rustc-link-lib=static={name}");
    println!("cargo:rerun-if-changed=src/preview_vt.c");
    println!(
        "cargo:rerun-if-changed={}",
        include.join("ghostty/vt").display()
    );
    println!("cargo:rerun-if-changed={}", lib.display());
}
