//! File-type icons as Nerd Font glyphs, devicons style: a glyph and a color
//! per well-known file name and extension.
//!
//! The glyphs are Nerd Fonts v3 codepoints, chosen by glyph name (the name is
//! in each entry's comment) and checked against the bundled Symbols Nerd Font
//! by `nfm-egui`'s tests. The colors follow the usual devicons palette.

use nfm_search_core::icon::{IconColor, IconRef};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Devicon {
    pub glyph: char,
    pub color: [u8; 3],
}

impl Devicon {
    /// The icon reference for this glyph and color.
    pub fn reference(&self) -> String {
        IconRef::glyph(
            self.glyph.encode_utf8(&mut [0; 4]),
            Some(IconColor(self.color)),
        )
    }
}

/// The icon for a path or file name. A trailing `/` or `\` marks a folder.
/// Unknown types get the generic file icon.
pub fn lookup(path: &str) -> Devicon {
    let path = path.trim();
    if path.ends_with(['/', '\\']) {
        return FOLDER;
    }
    lookup_entry(path, false)
}

/// The icon for a path or name whose kind the caller already knows.
pub fn lookup_entry(path: &str, is_dir: bool) -> Devicon {
    if is_dir {
        return FOLDER;
    }
    let name = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_lowercase();
    if let Some((_, icon)) = NAMES.iter().find(|(known, _)| *known == name) {
        return *icon;
    }
    // A leading dot (`.bashrc`) is part of the name, not an extension.
    let extension = name
        .char_indices()
        .skip(1)
        .filter(|&(_, c)| c == '.')
        .last()
        .map(|(index, _)| &name[index + 1..]);
    extension
        .and_then(|extension| EXTENSIONS.iter().find(|(known, _)| *known == extension))
        .map_or(FILE, |(_, icon)| *icon)
}

/// Every glyph in the table, for checking them against a font.
pub fn all() -> impl Iterator<Item = Devicon> {
    NAMES
        .iter()
        .chain(EXTENSIONS)
        .map(|(_, icon)| *icon)
        .chain([FOLDER, FILE])
}

// Generated from the Symbols Nerd Font glyph names; see the module docs.
#[rustfmt::skip]
pub const FOLDER: Devicon = Devicon { glyph: '\u{e613}', color: [0x7e, 0xba, 0xe4] }; // seti-folder
#[rustfmt::skip]
pub const FILE: Devicon = Devicon { glyph: '\u{e612}', color: [0x6d, 0x80, 0x86] }; // custom-default

/// Exact file names, lowercase. Checked before extensions.
#[rustfmt::skip]
pub const NAMES: &[(&str, Devicon)] = &[
    ("dockerfile", Devicon { glyph: '\u{e650}', color: [0x45, 0x8e, 0xe6] }), // seti-docker
    ("makefile", Devicon { glyph: '\u{e673}', color: [0x6d, 0x80, 0x86] }), // seti-makefile
    ("cmakelists.txt", Devicon { glyph: '\u{e673}', color: [0x6d, 0x80, 0x86] }), // seti-makefile
    (".gitignore", Devicon { glyph: '\u{e65d}', color: [0xf5, 0x4d, 0x27] }), // seti-git_ignore
    (".gitattributes", Devicon { glyph: '\u{e65d}', color: [0xf5, 0x4d, 0x27] }), // seti-git_ignore
    (".gitmodules", Devicon { glyph: '\u{e65d}', color: [0xf5, 0x4d, 0x27] }), // seti-git_ignore
    (".editorconfig", Devicon { glyph: '\u{e652}', color: [0xff, 0xf2, 0xf2] }), // seti-editorconfig
    ("license", Devicon { glyph: '\u{e60a}', color: [0xd0, 0xbf, 0x41] }), // seti-license
    ("license.md", Devicon { glyph: '\u{e60a}', color: [0xd0, 0xbf, 0x41] }), // seti-license
    ("license.txt", Devicon { glyph: '\u{e60a}', color: [0xd0, 0xbf, 0x41] }), // seti-license
    ("cargo.lock", Devicon { glyph: '\u{e672}', color: [0xde, 0xa5, 0x84] }), // seti-lock
    ("package.json", Devicon { glyph: '\u{e616}', color: [0xe8, 0x27, 0x4b] }), // seti-npm_ignored
    ("readme.md", Devicon { glyph: '\u{e66a}', color: [0x42, 0xa5, 0xf5] }), // seti-info
    ("readme", Devicon { glyph: '\u{e66a}', color: [0x42, 0xa5, 0xf5] }), // seti-info
];

/// Extensions without the dot, lowercase.
#[rustfmt::skip]
pub const EXTENSIONS: &[(&str, Devicon)] = &[
    ("rs", Devicon { glyph: '\u{e68b}', color: [0xde, 0xa5, 0x84] }), // seti-rust
    ("py", Devicon { glyph: '\u{e606}', color: [0xff, 0xbc, 0x03] }), // seti-python
    ("pyi", Devicon { glyph: '\u{e606}', color: [0xff, 0xbc, 0x03] }), // seti-python
    ("ipynb", Devicon { glyph: '\u{e678}', color: [0xf5, 0x7d, 0x01] }), // seti-notebook
    ("js", Devicon { glyph: '\u{e60c}', color: [0xcb, 0xcb, 0x41] }), // seti-javascript
    ("mjs", Devicon { glyph: '\u{e60c}', color: [0xcb, 0xcb, 0x41] }), // seti-javascript
    ("cjs", Devicon { glyph: '\u{e60c}', color: [0xcb, 0xcb, 0x41] }), // seti-javascript
    ("jsx", Devicon { glyph: '\u{e625}', color: [0x20, 0xc2, 0xe3] }), // seti-react
    ("ts", Devicon { glyph: '\u{e628}', color: [0x51, 0x9a, 0xba] }), // seti-typescript
    ("tsx", Devicon { glyph: '\u{e625}', color: [0x13, 0x54, 0xbf] }), // seti-react
    ("go", Devicon { glyph: '\u{e627}', color: [0x00, 0xad, 0xd8] }), // seti-go
    ("c", Devicon { glyph: '\u{e649}', color: [0x59, 0x9e, 0xff] }), // seti-c
    ("h", Devicon { glyph: '\u{e649}', color: [0xa0, 0x74, 0xc4] }), // seti-c
    ("cpp", Devicon { glyph: '\u{e646}', color: [0x51, 0x9a, 0xba] }), // seti-cpp
    ("cc", Devicon { glyph: '\u{e646}', color: [0x51, 0x9a, 0xba] }), // seti-cpp
    ("cxx", Devicon { glyph: '\u{e646}', color: [0x51, 0x9a, 0xba] }), // seti-cpp
    ("hpp", Devicon { glyph: '\u{e646}', color: [0xa0, 0x74, 0xc4] }), // seti-cpp
    ("cs", Devicon { glyph: '\u{e648}', color: [0x59, 0x67, 0x06] }), // seti-c_sharp
    ("csproj", Devicon { glyph: '\u{e77f}', color: [0x51, 0x2b, 0xd4] }), // dev-dotnet
    ("sln", Devicon { glyph: '\u{e70c}', color: [0x85, 0x4c, 0xc7] }), // dev-visualstudio
    ("fs", Devicon { glyph: '\u{e65a}', color: [0x51, 0x9a, 0xba] }), // seti-f_sharp
    ("java", Devicon { glyph: '\u{e66d}', color: [0xcc, 0x3e, 0x44] }), // seti-java
    ("kt", Devicon { glyph: '\u{e634}', color: [0x7f, 0x52, 0xff] }), // seti-kotlin
    ("kts", Devicon { glyph: '\u{e634}', color: [0x7f, 0x52, 0xff] }), // seti-kotlin
    ("swift", Devicon { glyph: '\u{e699}', color: [0xe3, 0x79, 0x33] }), // seti-swift
    ("rb", Devicon { glyph: '\u{e605}', color: [0x70, 0x15, 0x16] }), // seti-ruby
    ("php", Devicon { glyph: '\u{e608}', color: [0xa0, 0x74, 0xc4] }), // seti-php
    ("lua", Devicon { glyph: '\u{e620}', color: [0x51, 0xa0, 0xcf] }), // seti-lua
    ("zig", Devicon { glyph: '\u{e6a9}', color: [0xf6, 0x9a, 0x1b] }), // seti-zig
    ("hs", Devicon { glyph: '\u{e61f}', color: [0xa0, 0x74, 0xc4] }), // seti-haskell
    ("ex", Devicon { glyph: '\u{e62d}', color: [0xa0, 0x74, 0xc4] }), // seti-elixir
    ("exs", Devicon { glyph: '\u{e62d}', color: [0xa0, 0x74, 0xc4] }), // seti-elixir
    ("scala", Devicon { glyph: '\u{e68e}', color: [0xcc, 0x3e, 0x44] }), // seti-scala
    ("dart", Devicon { glyph: '\u{e64c}', color: [0x03, 0x58, 0x9c] }), // seti-dart
    ("nim", Devicon { glyph: '\u{e677}', color: [0xf3, 0xd4, 0x00] }), // seti-nim
    ("jl", Devicon { glyph: '\u{e624}', color: [0xa2, 0x70, 0xba] }), // seti-julia
    ("r", Devicon { glyph: '\u{e68a}', color: [0x22, 0x66, 0xba] }), // seti-r
    ("pl", Devicon { glyph: '\u{e67e}', color: [0x51, 0x9a, 0xba] }), // seti-perl
    ("clj", Devicon { glyph: '\u{e642}', color: [0x8d, 0xc1, 0x49] }), // seti-clojure
    ("ml", Devicon { glyph: '\u{e67a}', color: [0xe3, 0x79, 0x33] }), // seti-ocaml
    ("elm", Devicon { glyph: '\u{e62c}', color: [0x51, 0x9a, 0xba] }), // seti-elm
    ("vue", Devicon { glyph: '\u{e6a0}', color: [0x8d, 0xc1, 0x49] }), // seti-vue
    ("svelte", Devicon { glyph: '\u{e697}', color: [0xff, 0x3e, 0x00] }), // seti-svelte
    ("html", Devicon { glyph: '\u{e60e}', color: [0xe4, 0x4d, 0x26] }), // seti-html
    ("htm", Devicon { glyph: '\u{e60e}', color: [0xe4, 0x4d, 0x26] }), // seti-html
    ("css", Devicon { glyph: '\u{e614}', color: [0x42, 0xa5, 0xf5] }), // seti-css
    ("scss", Devicon { glyph: '\u{e603}', color: [0xf5, 0x53, 0x85] }), // seti-sass
    ("sass", Devicon { glyph: '\u{e603}', color: [0xf5, 0x53, 0x85] }), // seti-sass
    ("less", Devicon { glyph: '\u{e60b}', color: [0x56, 0x3d, 0x7c] }), // seti-less
    ("json", Devicon { glyph: '\u{f0626}', color: [0xcb, 0xcb, 0x41] }), // md-code_json
    ("jsonc", Devicon { glyph: '\u{f0626}', color: [0xcb, 0xcb, 0x41] }), // md-code_json
    ("yaml", Devicon { glyph: '\u{e6a8}', color: [0x6d, 0x80, 0x86] }), // seti-yml
    ("yml", Devicon { glyph: '\u{e6a8}', color: [0x6d, 0x80, 0x86] }), // seti-yml
    ("toml", Devicon { glyph: '\u{e6b2}', color: [0x9c, 0x42, 0x21] }), // custom-toml
    ("xml", Devicon { glyph: '\u{e619}', color: [0xe3, 0x79, 0x33] }), // seti-xml
    ("csv", Devicon { glyph: '\u{e64a}', color: [0x89, 0xe0, 0x51] }), // seti-csv
    ("tsv", Devicon { glyph: '\u{e64a}', color: [0x89, 0xe0, 0x51] }), // seti-csv
    ("md", Devicon { glyph: '\u{e609}', color: [0xdd, 0xdd, 0xdd] }), // seti-markdown
    ("markdown", Devicon { glyph: '\u{e609}', color: [0xdd, 0xdd, 0xdd] }), // seti-markdown
    ("txt", Devicon { glyph: '\u{e64e}', color: [0x89, 0xe0, 0x51] }), // seti-text
    ("log", Devicon { glyph: '\u{f1085}', color: [0xdd, 0xdd, 0xdd] }), // md-math_log
    ("tex", Devicon { glyph: '\u{e69b}', color: [0x3d, 0x61, 0x17] }), // seti-tex
    ("sh", Devicon { glyph: '\u{e691}', color: [0x4d, 0x5a, 0x5e] }), // seti-shell
    ("bash", Devicon { glyph: '\u{e691}', color: [0x89, 0xe0, 0x51] }), // seti-shell
    ("zsh", Devicon { glyph: '\u{e691}', color: [0x89, 0xe0, 0x51] }), // seti-shell
    ("fish", Devicon { glyph: '\u{e691}', color: [0x4d, 0x5a, 0x5e] }), // seti-shell
    ("ps1", Devicon { glyph: '\u{e683}', color: [0x42, 0x73, 0xca] }), // seti-powershell
    ("psm1", Devicon { glyph: '\u{e683}', color: [0x42, 0x73, 0xca] }), // seti-powershell
    ("bat", Devicon { glyph: '\u{e629}', color: [0xc1, 0xf1, 0x2e] }), // custom-msdos
    ("cmd", Devicon { glyph: '\u{e629}', color: [0xc1, 0xf1, 0x2e] }), // custom-msdos
    ("vim", Devicon { glyph: '\u{e62b}', color: [0x01, 0x98, 0x33] }), // custom-vim
    ("sql", Devicon { glyph: '\u{e64d}', color: [0xda, 0xd8, 0xd8] }), // seti-db
    ("db", Devicon { glyph: '\u{e64d}', color: [0xda, 0xd8, 0xd8] }), // seti-db
    ("sqlite", Devicon { glyph: '\u{e64d}', color: [0xda, 0xd8, 0xd8] }), // seti-db
    ("tf", Devicon { glyph: '\u{e69a}', color: [0x5f, 0x43, 0xe9] }), // seti-terraform
    ("graphql", Devicon { glyph: '\u{e662}', color: [0xe5, 0x35, 0xab] }), // seti-graphql
    ("proto", Devicon { glyph: '\u{e615}', color: [0x6d, 0x80, 0x86] }), // seti-config
    ("ini", Devicon { glyph: '\u{e615}', color: [0x6d, 0x80, 0x86] }), // seti-config
    ("cfg", Devicon { glyph: '\u{e615}', color: [0x6d, 0x80, 0x86] }), // seti-config
    ("conf", Devicon { glyph: '\u{e615}', color: [0x6d, 0x80, 0x86] }), // seti-config
    ("lock", Devicon { glyph: '\u{e672}', color: [0xbb, 0xbb, 0xbb] }), // seti-lock
    ("svg", Devicon { glyph: '\u{e698}', color: [0xff, 0xb1, 0x3b] }), // seti-svg
    ("png", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("jpg", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("jpeg", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("gif", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("webp", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("bmp", Devicon { glyph: '\u{e60d}', color: [0xa0, 0x74, 0xc4] }), // seti-image
    ("ico", Devicon { glyph: '\u{e623}', color: [0xcb, 0xcb, 0x41] }), // seti-favicon
    ("mp4", Devicon { glyph: '\u{e69f}', color: [0xfd, 0x97, 0x1f] }), // seti-video
    ("mkv", Devicon { glyph: '\u{e69f}', color: [0xfd, 0x97, 0x1f] }), // seti-video
    ("mov", Devicon { glyph: '\u{e69f}', color: [0xfd, 0x97, 0x1f] }), // seti-video
    ("webm", Devicon { glyph: '\u{e69f}', color: [0xfd, 0x97, 0x1f] }), // seti-video
    ("mp3", Devicon { glyph: '\u{e638}', color: [0x66, 0xd8, 0xef] }), // seti-audio
    ("wav", Devicon { glyph: '\u{e638}', color: [0x66, 0xd8, 0xef] }), // seti-audio
    ("flac", Devicon { glyph: '\u{e638}', color: [0x66, 0xd8, 0xef] }), // seti-audio
    ("ogg", Devicon { glyph: '\u{e638}', color: [0x66, 0xd8, 0xef] }), // seti-audio
    ("pdf", Devicon { glyph: '\u{e67d}', color: [0xb3, 0x0b, 0x00] }), // seti-pdf
    ("doc", Devicon { glyph: '\u{e6a5}', color: [0x18, 0x5a, 0xbd] }), // seti-word
    ("docx", Devicon { glyph: '\u{e6a5}', color: [0x18, 0x5a, 0xbd] }), // seti-word
    ("xls", Devicon { glyph: '\u{e6a6}', color: [0x20, 0x72, 0x45] }), // seti-xls
    ("xlsx", Devicon { glyph: '\u{e6a6}', color: [0x20, 0x72, 0x45] }), // seti-xls
    ("ppt", Devicon { glyph: '\u{f0227}', color: [0xcb, 0x4a, 0x32] }), // md-file_powerpoint
    ("pptx", Devicon { glyph: '\u{f0227}', color: [0xcb, 0x4a, 0x32] }), // md-file_powerpoint
    ("zip", Devicon { glyph: '\u{e6aa}', color: [0xec, 0xa5, 0x17] }), // seti-zip
    ("gz", Devicon { glyph: '\u{e6aa}', color: [0xec, 0xa5, 0x17] }), // seti-zip
    ("tar", Devicon { glyph: '\u{e6aa}', color: [0xec, 0xa5, 0x17] }), // seti-zip
    ("7z", Devicon { glyph: '\u{e6aa}', color: [0xec, 0xa5, 0x17] }), // seti-zip
    ("rar", Devicon { glyph: '\u{e6aa}', color: [0xec, 0xa5, 0x17] }), // seti-zip
    ("exe", Devicon { glyph: '\u{f08c6}', color: [0x9f, 0x05, 0x00] }), // md-application
    ("msi", Devicon { glyph: '\u{f08c6}', color: [0x9f, 0x05, 0x00] }), // md-application
    ("dll", Devicon { glyph: '\u{f107b}', color: [0x9f, 0x05, 0x00] }), // md-file_cog
    ("wasm", Devicon { glyph: '\u{e6a1}', color: [0x5c, 0x4c, 0xdb] }), // seti-wasm
    ("ttf", Devicon { glyph: '\u{e659}', color: [0xec, 0xec, 0xec] }), // seti-font
    ("otf", Devicon { glyph: '\u{e659}', color: [0xec, 0xec, 0xec] }), // seti-font
    ("woff", Devicon { glyph: '\u{e659}', color: [0xec, 0xec, 0xec] }), // seti-font
    ("woff2", Devicon { glyph: '\u{e659}', color: [0xec, 0xec, 0xec] }), // seti-font
    ("diff", Devicon { glyph: '\u{e65d}', color: [0x41, 0x53, 0x5b] }), // seti-git_ignore
    ("patch", Devicon { glyph: '\u{e65d}', color: [0x41, 0x53, 0x5b] }), // seti-git_ignore
    ("nix", Devicon { glyph: '\u{e843}', color: [0x7e, 0xba, 0xe4] }), // dev-nixos
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_win_over_extensions_and_case_is_ignored() {
        assert_eq!(lookup("src/MAIN.RS").glyph, '\u{e68b}');
        assert_eq!(lookup(r"C:\proj\Cargo.lock").color, [0xde, 0xa5, 0x84]);
        assert_eq!(lookup("Cargo.lock").glyph, lookup("x.lock").glyph);
        assert_ne!(lookup("Cargo.lock").color, lookup("x.lock").color);
        assert_eq!(lookup("archive.tar.gz"), lookup("a.zip"));
        assert_eq!(lookup("Dockerfile").glyph, '\u{e650}');
    }

    #[test]
    fn folders_dotfiles_and_unknown_types() {
        assert_eq!(lookup("src/"), FOLDER);
        assert_eq!(lookup(r"C:\src\"), FOLDER);
        assert_eq!(lookup_entry("src", true), FOLDER);
        assert_eq!(lookup(".bashrc"), FILE);
        assert_eq!(lookup(".gitignore").glyph, '\u{e65d}');
        assert_eq!(lookup("notes.unknownext"), FILE);
        assert_eq!(lookup("README"), lookup("readme.md"));
    }

    #[test]
    fn references_carry_the_glyph_and_color() {
        assert_eq!(lookup("a.rs").reference(), "glyph:\u{e68b}@#dea584");
    }

    #[test]
    fn table_keys_are_unique_and_lowercase() {
        for table in [NAMES, EXTENSIONS] {
            let mut keys: Vec<_> = table.iter().map(|(key, _)| *key).collect();
            assert!(keys.iter().all(|key| key.to_lowercase() == *key));
            let count = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), count);
        }
        assert!(EXTENSIONS.iter().all(|(key, _)| !key.contains('.')));
    }
}
