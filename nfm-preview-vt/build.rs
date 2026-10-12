use std::{env, path::PathBuf};

fn main() {
    let include = PathBuf::from(
        env::var_os("DEP_GHOSTTY_VT_INCLUDE").expect("ghostty-vt-sys include metadata"),
    );
    cc::Build::new()
        .file("src/preview_vt.c")
        .define("GHOSTTY_STATIC", None)
        .include(&include)
        .warnings(true)
        .compile("nfm_preview_vt_shim");
    println!("cargo:rerun-if-changed=src/preview_vt.c");
    println!(
        "cargo:rerun-if-changed={}",
        include.join("ghostty/vt").display()
    );
}
