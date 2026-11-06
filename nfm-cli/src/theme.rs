//! `--color`, `--font` and `--font-size` applied to the picker appearance.
use nfm_egui::{
    Appearance, FontFace, Palette,
    egui::{self, Color32, FontId},
};
use std::path::Path;

const NAMES: &[&str] = &[
    "background",
    "text",
    "muted",
    "selection",
    "selection-text",
    "preview-selection",
    "match",
    "divider",
    "error",
    "accent",
    "success",
    "cursor",
];

/// Validate one `--color` entry.
pub fn check(name: &str, color: &str) -> Result<(), String> {
    if !NAMES.contains(&name) {
        return Err(format!(
            "unknown color '{name}'; expected one of {}",
            NAMES.join(", ")
        ));
    }
    parse_color(color).map(|_| ())
}

pub fn parse_color(text: &str) -> Result<Color32, String> {
    let hex = text
        .strip_prefix('#')
        .ok_or_else(|| format!("color '{text}' must look like #RRGGBB"))?;
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16);
    let parsed = match hex.len() {
        6 => (|| Ok::<_, std::num::ParseIntError>(Color32::from_rgb(channel(0)?, channel(2)?, channel(4)?)))(),
        8 => (|| {
            Ok(Color32::from_rgba_unmultiplied(
                channel(0)?,
                channel(2)?,
                channel(4)?,
                channel(6)?,
            ))
        })(),
        _ => return Err(format!("color '{text}' must look like #RRGGBB or #RRGGBBAA")),
    };
    parsed.map_err(|_| format!("color '{text}' is not hexadecimal"))
}

fn slot<'a>(palette: &'a mut Palette, name: &str) -> &'a mut Color32 {
    match name {
        "background" => &mut palette.background,
        "text" => &mut palette.text,
        "muted" => &mut palette.muted,
        "selection" => &mut palette.selection_background,
        "selection-text" => &mut palette.selection_text,
        "preview-selection" => &mut palette.preview_selection_background,
        "match" => &mut palette.match_highlight,
        "divider" => &mut palette.divider,
        "error" => &mut palette.error,
        "accent" => &mut palette.accent,
        "success" => &mut palette.success,
        _ => unreachable!("color names are validated while parsing"),
    }
}

pub fn appearance(colors: &[(String, String)], font_size: Option<f32>) -> Appearance {
    let mut appearance = Appearance::default();
    for (name, color) in colors {
        let Ok(color) = parse_color(color) else {
            continue;
        };
        if name == "cursor" {
            appearance.palette.copy_cursor_background = Some(color);
        } else {
            *slot(&mut appearance.palette, name) = color;
        }
    }
    if let Some(size) = font_size {
        let typography = &mut appearance.typography;
        let scale = size / typography.normal.size;
        typography.normal.size = size;
        typography.bold.size = size;
        typography.row_height = (typography.row_height * scale).round();
        typography.cell_width *= scale;
    }
    appearance
}

/// Register `--font` as the picker's family. Returns an error for unreadable
/// files; egui itself rejects malformed fonts when the atlas is built.
pub fn install_font(
    ctx: &egui::Context,
    appearance: &mut Appearance,
    path: &Path,
) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("--font {}: {e}", path.display()))?;
    let mut definitions = egui::FontDefinitions::default();
    let family = FontFace {
        bytes,
        face_index: 0,
    }
    .register(&mut definitions, "nfm-cli-font");
    ctx.set_fonts(definitions);
    let typography = &mut appearance.typography;
    typography.normal = FontId::new(typography.normal.size, family.clone());
    typography.bold = FontId::new(typography.bold.size, family);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_and_apply() {
        assert_eq!(parse_color("#ff8000").unwrap(), Color32::from_rgb(255, 128, 0));
        assert!(parse_color("ff8000").is_err());
        assert!(parse_color("#ff80").is_err());
        assert!(parse_color("#gg8000").is_err());
        assert!(check("accent", "#123456").is_ok());
        assert!(check("bogus", "#123456").is_err());
        let appearance = appearance(&[("accent".into(), "#010203".into())], Some(28.0));
        assert_eq!(appearance.palette.accent, Color32::from_rgb(1, 2, 3));
        assert_eq!(appearance.typography.normal.size, 28.0);
        assert_eq!(appearance.typography.row_height, 40.0);
    }
}
