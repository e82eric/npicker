use std::{
    env, fs,
    hash::{DefaultHasher, Hasher},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn main() {
    for name in [
        "GHOSTTY_ROOT",
        "NFM_GHOSTTY_VT_INCLUDE_DIR",
        "NFM_GHOSTTY_VT_LIB_DIR",
        "NFM_GHOSTTY_VT_LIB_NAME",
        "GHOSTTY_VT_ZIG",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=vendor/ghostty.tar.gz");
    println!("cargo:rerun-if-changed=build.rs");
    let root = env::var_os("GHOSTTY_ROOT").map(PathBuf::from);
    let explicit_include = env::var_os("NFM_GHOSTTY_VT_INCLUDE_DIR").map(PathBuf::from);
    let explicit_lib = env::var_os("NFM_GHOSTTY_VT_LIB_DIR").map(PathBuf::from);
    let (include, lib) = if root.is_some() || explicit_include.is_some() || explicit_lib.is_some() {
        let include = explicit_include.or_else(|| root.as_ref().map(|r| r.join("include")))
            .expect("NFM_GHOSTTY_VT_INCLUDE_DIR must accompany NFM_GHOSTTY_VT_LIB_DIR when GHOSTTY_ROOT is unset");
        let lib = explicit_lib.or_else(|| root.as_ref().map(|r| {
            let preview = r.join("zig-out/nfm-vt/lib");
            if preview.is_dir() { preview } else { r.join("zig-out/lib") }
        })).expect("NFM_GHOSTTY_VT_LIB_DIR must accompany NFM_GHOSTTY_VT_INCLUDE_DIR when GHOSTTY_ROOT is unset");
        println!(
            "cargo:rerun-if-changed={}",
            include.join("ghostty/vt").display()
        );
        println!("cargo:rerun-if-changed={}", lib.display());
        (include, lib)
    } else {
        bundled_build()
    };
    assert!(
        include.join("ghostty/vt/terminal.h").is_file(),
        "Ghostty VT headers missing in {}",
        include.display()
    );
    let name = env::var("NFM_GHOSTTY_VT_LIB_NAME").unwrap_or_else(|_| "ghostty-vt-static".into());
    let filename = if env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc" {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    };
    assert!(
        lib.join(filename).is_file(),
        "Ghostty VT static library missing in {}",
        lib.display()
    );
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static={name}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=ntdll");
        println!("cargo:rustc-link-lib=kernel32");
    }
    println!("cargo:include={}", include.display());
    println!("cargo:lib_dir={}", lib.display());
}

fn bundled_build() -> (PathBuf, PathBuf) {
    let zig = env::var_os("GHOSTTY_VT_ZIG").unwrap_or_else(|| "zig".into());
    let version = Command::new(&zig)
        .arg("version")
        .output()
        .expect("Ghostty VT requires Zig 0.16.0 on PATH (or set GHOSTTY_VT_ZIG to its executable)");
    assert!(
        version.status.success() && String::from_utf8_lossy(&version.stdout).trim() == "0.16.0",
        "Bundled Ghostty VT requires Zig 0.16.0; selected Zig reported {}",
        String::from_utf8_lossy(&version.stdout).trim()
    );
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let arch = match arch.as_str() {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => panic!("Unsupported bundled Ghostty VT architecture: {other}"),
    };
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let abi = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target = match (os.as_str(), abi.as_str()) {
        ("windows", "msvc") => format!("{arch}-windows-msvc"),
        ("windows", "gnu") => format!("{arch}-windows-gnu"),
        ("linux", "gnu" | "musl") => format!("{arch}-linux-{abi}"),
        ("macos", _) => format!("{arch}-macos"),
        _ => panic!(
            "Unsupported bundled Ghostty VT target: {}",
            env::var("TARGET").unwrap()
        ),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/ghostty.tar.gz");
    let bytes = fs::read(archive).expect("Missing bundled Ghostty source archive");
    // Content-specific directories avoid stale files when the source pin changes.
    // This hash is a cache identifier, not a security checksum.
    let mut hash = DefaultHasher::new();
    hash.write(&bytes);
    let source = out.join(format!("ghostty-source-{:016x}", hash.finish()));
    let extracted = source.join(".cargo-extracted");
    if !extracted.is_file() {
        fs::create_dir_all(&source).unwrap();
        tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()))
            .unpack(&source)
            .expect("Failed to unpack bundled Ghostty source");
        fs::write(extracted, b"complete").unwrap();
    }
    let prefix = out.join("ghostty-install");
    let status = Command::new(zig)
        .current_dir(&source)
        .args([
            "build",
            "-Demit-lib-vt",
            "-Doptimize=ReleaseFast",
            "-Dvt-features=-kitty-graphics",
            "-Dversion-string=1.3.2-dev.1a5061b",
            "-Dlib-version-string=0.1.0-dev.1a5061b",
        ])
        .arg(format!("-Dtarget={target}"))
        .arg(format!(
            "-j{}",
            env::var("NUM_JOBS").unwrap_or_else(|_| "1".into())
        ))
        .arg("--prefix")
        .arg(&prefix)
        .arg("--cache-dir")
        .arg(out.join("zig-cache"))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to start Zig for Ghostty VT");
    assert!(status.success(), "Ghostty VT Zig build failed: {status}");
    (source.join("include"), prefix.join("lib"))
}
