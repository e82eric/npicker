# Cargo-managed Ghostty VT

Builds the standalone VT static library from bundled source with Zig 0.16.0.
No local Ghostty checkout or directory environment variable is required.
Install Zig on PATH, or select its executable with `GHOSTTY_VT_ZIG`.
Zig downloads hash-pinned dependencies when absent from its global cache.
Cargo offline mode does not control Zig's dependency downloads.

The source archive is from https://github.com/e82eric/ghostty at commit
`1a5061b7162234c1766911f923d68389aba0c789`. It contains the original
`build.zig`, `build.zig.zon`, `src`, `include`, `pkg`, `vendor`, `dist`, and
MIT `LICENSE`, without local working-tree edits.
Archive SHA-256: `b641538601e70d53f0d9eb146ea661d4ab1a238852b16ee974cc4441747c698f`.

Regenerate from that repository with:

```powershell
git archive --format=tar.gz --output=<crate>/vendor/ghostty.tar.gz <commit> build.zig build.zig.zon src include pkg vendor dist LICENSE
```

When upgrading, update the commit, archive hash, version arguments in build.rs,
and required Zig version together. Validate nfm-preview-vt's ABI and tests.
The archive travels with the crate; the build script does not download Ghostty
itself. Zig's own dependencies still require a populated cache or network access.

Build output and Zig's local compilation cache are under OUT_DIR. The target
comes from Cargo, rather than the build machine. Windows MSVC, Windows GNU,
Linux GNU/musl and macOS targets are mapped for x86_64 and aarch64; validation
currently covers Windows MSVC. Builds use ReleaseFast and disable Kitty graphics.

Optional prebuilt overrides preserve the existing workflow:

- `GHOSTTY_ROOT`: headers in include; library in zig-out/nfm-vt/lib or zig-out/lib.
- `NFM_GHOSTTY_VT_INCLUDE_DIR` and `NFM_GHOSTTY_VT_LIB_DIR`: explicit paths;
  provide both when GHOSTTY_ROOT is unset.
- `NFM_GHOSTTY_VT_LIB_NAME`: static library name, default ghostty-vt-static.

Overrides bypass source extraction and Zig. Headers and library must match.
The crate exports include and lib_dir metadata to dependent build scripts and
owns linking the Ghostty static library.
