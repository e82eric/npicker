use egui::{Color32, FontDefinitions, FontFamily, FontId};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Palette {
    pub background: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub selection_background: Color32,
    pub selection_text: Color32,
    /// Preview selection fill. Text keeps its syntax/ANSI foreground colors.
    pub preview_selection_background: Color32,
    pub match_highlight: Color32,
    pub divider: Color32,
    pub error: Color32,
    pub accent: Color32,
    /// Copy cursor fill; None uses accent.
    pub copy_cursor_background: Option<Color32>,
    pub success: Color32,
    /// Optional copy-mode colors; None preserves the generic palette defaults.
    pub search_match_background: Option<Color32>,
    pub search_match_text: Option<Color32>,
    pub search_active_background: Option<Color32>,
    pub search_active_text: Option<Color32>,
    pub yank_background: Option<Color32>,
    pub yank_text: Option<Color32>,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Color32::from_rgb(0x1d, 0x20, 0x21),
            text: Color32::from_rgb(235, 219, 178),
            muted: Color32::from_rgb(168, 153, 132),
            selection_background: Color32::from_rgb(0x3d, 0x39, 0x37),
            selection_text: Color32::from_rgb(0xeb, 0xdb, 0xb2),
            preview_selection_background: Color32::from_rgb(0x26, 0x45, 0x77),
            match_highlight: Color32::from_rgb(0xfb, 0x49, 0x34),
            divider: Color32::from_rgb(0x92, 0x83, 0x74),
            error: Color32::from_rgb(251, 73, 52),
            accent: Color32::from_rgb(250, 189, 47),
            copy_cursor_background: None,
            success: Color32::from_rgb(184, 187, 38),
            search_match_background: None,
            search_match_text: None,
            search_active_background: None,
            search_active_text: None,
            yank_background: None,
            yank_text: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Typography {
    pub normal: FontId,
    pub bold: FontId,
    /// Logical points, not framebuffer pixels.
    pub row_height: f32,
    pub cell_width: f32,
}
impl Default for Typography {
    fn default() -> Self {
        Self {
            normal: FontId::monospace(14.0),
            bold: FontId::monospace(14.0),
            row_height: 20.0,
            cell_width: 8.5,
        }
    }
}
impl Typography {
    /// Picker and preview rows share three logical points of padding on each side.
    pub fn preview_row_height(&self, ctx: &egui::Context) -> f32 {
        let scale = ctx.pixels_per_point().max(0.01);
        ((self.preview_cell_height(ctx) + 6.0) * scale).round() / scale
    }

    /// Preview cell height follows the larger of the normal and bold font
    /// metrics, rounded to a whole physical pixel at the current DPI.
    /// Shared row layout adds padding to this font height.
    pub fn preview_cell_height(&self, ctx: &egui::Context) -> f32 {
        let height = ctx.fonts_mut(|fonts| {
            fonts
                .row_height(&self.normal)
                .max(fonts.row_height(&self.bold))
        });
        let scale = ctx.pixels_per_point().max(0.01);
        (height * scale).round().max(1.0) / scale
    }

    /// Use host-provided physical cell metrics at the current DPI.
    pub fn with_cell_metrics(
        mut self,
        width_px: f32,
        height_px: f32,
        pixels_per_point: f32,
    ) -> Self {
        let scale = pixels_per_point.max(0.01);
        self.cell_width = (width_px / scale).max(1.0);
        self.row_height = (height_px / scale).max(1.0);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Appearance {
    pub palette: Palette,
    pub typography: Typography,
}

/// Owned font data; host FFI buffers can be released immediately after copying.
#[derive(Clone)]
pub struct FontFace {
    pub bytes: Vec<u8>,
    pub face_index: u32,
}
impl FontFace {
    /// Add a named family without removing fonts already registered by the host.
    /// The host calls `Context::set_fonts` once after registering its faces.
    pub fn register(
        self,
        definitions: &mut FontDefinitions,
        name: impl Into<String>,
    ) -> FontFamily {
        let name = name.into();
        let mut data = egui::FontData::from_owned(self.bytes);
        data.index = self.face_index;
        definitions.font_data.insert(name.clone(), Arc::new(data));
        let mut fallback = definitions
            .families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default();
        fallback.insert(0, name.clone());
        let family = FontFamily::Name(name.into());
        definitions.families.insert(family.clone(), fallback);
        family
    }
}
