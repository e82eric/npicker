//! Icons in card slots: glyphs, which the picker draws itself, and anything
//! else through resolvers the host installs.
//!
//! An icon slot's field holds an icon reference (see
//! [`nfm_search_core::icon`]). `glyph:` references are drawn directly with the
//! bundled Symbols Nerd Font (feature `symbols-font`, on by default), so Nerd
//! Font icons work whatever font the host uses. Every other reference goes to
//! the host's [`IconResolver`]s, in the order they were added, on a background
//! thread; the slot stays empty until one answers. A host without resolvers
//! draws only glyphs, so data can never make the picker open files, windows
//! or anything else.

use crate::appearance::Appearance;
#[cfg(feature = "symbols-font")]
use egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
use egui::{Color32, FontFamily, FontId};
use nfm_search_core::icon::{IconColor, IconRef};
use std::collections::HashMap;
use std::sync::Arc;

/// The font family glyph icons are drawn with.
pub const SYMBOLS_FONT_FAMILY: &str = "nfm-symbols";

#[cfg(feature = "symbols-font")]
static SYMBOLS_FONT: &[u8] = include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf");

/// Painted by a host: the painter, the icon's square, and egui's time in
/// seconds for animation.
pub type PaintIcon = Arc<dyn Fn(&egui::Painter, egui::Rect, f64) + Send + Sync>;

/// What a resolver returns for a reference.
#[derive(Clone)]
pub enum IconSource {
    /// Text drawn with the icon font, falling back to the normal font.
    Glyph {
        text: String,
        color: Option<Color32>,
    },
    /// An image, scaled into the icon's square.
    Image(Arc<egui::ColorImage>),
    /// Drawn by the host each frame. `animated` icons repaint continuously
    /// while they are visible.
    Painted { paint: PaintIcon, animated: bool },
}

impl std::fmt::Debug for IconSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Glyph { text, color } => f
                .debug_struct("Glyph")
                .field("text", text)
                .field("color", color)
                .finish(),
            Self::Image(image) => f.debug_tuple("Image").field(&image.size).finish(),
            Self::Painted { animated, .. } => f
                .debug_struct("Painted")
                .field("animated", animated)
                .finish(),
        }
    }
}

/// Turns host references (anything but `glyph:`) into icons.
///
/// Resolvers run on the picker's icon thread, one reference at a time, so a
/// resolver may block briefly but should bound how long. `size_px` is the
/// icon's square in physical pixels. Return None for references a resolver
/// does not handle; the next resolver is asked.
pub trait IconResolver: Send + Sync + 'static {
    fn resolve(&self, reference: &str, size_px: u32) -> Option<IconSource>;
}

impl<F> IconResolver for F
where
    F: Fn(&str, u32) -> Option<IconSource> + Send + Sync + 'static,
{
    fn resolve(&self, reference: &str, size_px: u32) -> Option<IconSource> {
        self(reference, size_px)
    }
}

/// Cached icons are dropped when there are more than this many.
const MAX_ENTRIES: usize = 1024;
/// References waiting for the icon thread; more wait for a later frame.
const MAX_PENDING: usize = 256;

type Key = (String, u32);

enum Entry {
    Pending,
    Missing,
    Ready {
        source: IconSource,
        texture: Option<egui::TextureHandle>,
    },
}

struct Worker {
    jobs: crossbeam_channel::Sender<Key>,
    results: crossbeam_channel::Receiver<(Key, Option<IconSource>)>,
}

/// Host-resolved icons for one picker.
#[derive(Default)]
pub(crate) struct IconCache {
    resolvers: Vec<Arc<dyn IconResolver>>,
    entries: HashMap<Key, Entry>,
    pending: usize,
    worker: Option<Worker>,
}

impl IconCache {
    pub(crate) fn add_resolver(&mut self, resolver: Arc<dyn IconResolver>) {
        self.resolvers.push(resolver);
        // A new resolver may answer references that were missing, and the
        // running thread has the old list.
        self.entries.clear();
        self.pending = 0;
        self.worker = None;
    }

    /// Take finished lookups. Call once per frame before painting.
    pub(crate) fn poll(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        while let Ok((key, source)) = worker.results.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            let entry = match source {
                Some(source) => Entry::Ready {
                    source,
                    texture: None,
                },
                None => Entry::Missing,
            };
            self.entries.insert(key, entry);
        }
    }

    /// The icon for a host reference, asking the resolvers when it is new.
    fn get(&mut self, ctx: &egui::Context, reference: &str, size_px: u32) -> Option<&Entry> {
        if self.resolvers.is_empty() {
            return None;
        }
        let key = (reference.to_owned(), size_px);
        if !self.entries.contains_key(&key) {
            if self.pending >= MAX_PENDING {
                return None;
            }
            if self.entries.len() >= MAX_ENTRIES {
                self.entries
                    .retain(|_, entry| matches!(entry, Entry::Pending));
            }
            let worker = self
                .worker
                .get_or_insert_with(|| spawn(ctx, &self.resolvers));
            if worker.jobs.send(key.clone()).is_err() {
                return None;
            }
            self.pending += 1;
            self.entries.insert(key.clone(), Entry::Pending);
        }
        self.entries.get(&key)
    }

    /// The texture for an image entry, uploaded on first use.
    fn texture(
        &mut self,
        ctx: &egui::Context,
        reference: &str,
        size_px: u32,
    ) -> Option<egui::TextureId> {
        let key = (reference.to_owned(), size_px);
        match self.entries.get_mut(&key)? {
            Entry::Ready {
                source: IconSource::Image(image),
                texture,
            } => Some(
                texture
                    .get_or_insert_with(|| {
                        ctx.load_texture(
                            format!("nfm-icon:{reference}:{size_px}"),
                            (**image).clone(),
                            egui::TextureOptions::LINEAR,
                        )
                    })
                    .id(),
            ),
            _ => None,
        }
    }
}

fn spawn(ctx: &egui::Context, resolvers: &[Arc<dyn IconResolver>]) -> Worker {
    let (jobs, job_receiver) = crossbeam_channel::unbounded::<Key>();
    let (result_sender, results) = crossbeam_channel::unbounded();
    let resolvers: Vec<_> = resolvers.to_vec();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("nfm-icons".into())
        .spawn(move || {
            // Ends when the cache drops its sender.
            for (reference, size_px) in job_receiver {
                let source = resolvers
                    .iter()
                    .find_map(|resolver| resolver.resolve(&reference, size_px));
                if result_sender.send(((reference, size_px), source)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        })
        .expect("spawn the icon thread");
    Worker { jobs, results }
}

/// The family for glyph icons, registering the bundled font when the
/// context lacks it (a host's `set_fonts` replaces all fonts). It is ready
/// from the next frame, so this returns None until then, and always without
/// the `symbols-font` feature.
fn symbols_family(ctx: &egui::Context) -> Option<FontFamily> {
    let family = FontFamily::Name(SYMBOLS_FONT_FAMILY.into());
    if ctx.fonts(|fonts| fonts.definitions().families.contains_key(&family)) {
        return Some(family);
    }
    #[cfg(feature = "symbols-font")]
    {
        ctx.add_font(FontInsert::new(
            SYMBOLS_FONT_FAMILY,
            FontData::from_static(SYMBOLS_FONT),
            vec![InsertFontFamily {
                family,
                priority: FontPriority::Highest,
            }],
        ));
        ctx.request_repaint();
    }
    None
}

/// The color an icon is drawn in; glyphs without one use the theme's text
/// color.
pub(crate) fn icon_color(color: Option<IconColor>, appearance: &Appearance) -> Color32 {
    match color {
        Some(IconColor([r, g, b])) => Color32::from_rgb(r, g, b),
        None => appearance.palette.text,
    }
}

/// Paint `reference` in the square `rect`. Returns whether anything was (or
/// will soon be) painted, so empty references take no room.
pub(crate) fn paint_icon(
    ctx: &egui::Context,
    painter: &egui::Painter,
    rect: egui::Rect,
    reference: &str,
    cache: &mut IconCache,
    appearance: &Appearance,
) -> bool {
    match IconRef::parse(reference) {
        IconRef::None => false,
        IconRef::Glyph { text, color } => {
            paint_glyph(
                ctx,
                painter,
                rect,
                text,
                icon_color(color, appearance),
                appearance,
            );
            true
        }
        IconRef::Other(reference) => {
            let size_px = (rect.height() * ctx.pixels_per_point()).round().max(1.0) as u32;
            let source = match cache.get(ctx, reference, size_px) {
                None | Some(Entry::Missing) => return false,
                Some(Entry::Pending) => return true,
                Some(Entry::Ready { source, .. }) => source.clone(),
            };
            match source {
                IconSource::Glyph { text, color } => paint_glyph(
                    ctx,
                    painter,
                    rect,
                    &text,
                    color.unwrap_or(appearance.palette.text),
                    appearance,
                ),
                IconSource::Image(image) => {
                    if let Some(texture) = cache.texture(ctx, reference, size_px) {
                        painter.image(
                            texture,
                            image_rect(rect, image.size, ctx.pixels_per_point()),
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                }
                IconSource::Painted { paint, animated } => {
                    paint(painter, rect, ctx.input(|input| input.time));
                    if animated {
                        ctx.request_repaint();
                    }
                }
            }
            true
        }
    }
}

/// Where an image of `size` pixels is drawn in `rect`: at its own size when
/// it fits, otherwise scaled down, and on whole physical pixels so it stays
/// sharp.
fn image_rect(rect: egui::Rect, size: [usize; 2], pixels_per_point: f32) -> egui::Rect {
    let scale = pixels_per_point.max(0.01);
    let native = egui::vec2(size[0] as f32, size[1] as f32) / scale;
    let fit = (rect.width() / native.x)
        .min(rect.height() / native.y)
        .min(1.0);
    let drawn = ((native * fit) * scale).round() / scale;
    let min = ((rect.center() - drawn / 2.0) * scale).round() / scale;
    egui::Rect::from_min_size(min, drawn)
}

fn paint_glyph(
    ctx: &egui::Context,
    painter: &egui::Painter,
    rect: egui::Rect,
    text: &str,
    color: Color32,
    appearance: &Appearance,
) {
    let size = (rect.height() * 0.8).round().max(1.0);
    let symbols = symbols_family(ctx).map(|family| FontId::new(size, family));
    // Glyphs the icon font lacks (emoji, letters) use the normal font.
    let font = match symbols {
        Some(font) if ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, text)) => font,
        _ => FontId::new(size, appearance.typography.normal.family.clone()),
    };
    let galley = painter.layout_no_wrap(text.to_owned(), font, color);
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_draw_at_their_own_size_on_whole_pixels() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.3, 20.7), egui::vec2(30.0, 30.0));
        // 32 px at 1.5 pixels per point fits: drawn 1:1, on the pixel grid.
        let drawn = image_rect(rect, [32, 32], 1.5);
        assert!((drawn.width() * 1.5 - 32.0).abs() < 1e-3);
        let px = drawn.min * 1.5;
        assert!((px.x - px.x.round()).abs() < 1e-3 && (px.y - px.y.round()).abs() < 1e-3);
        assert!((drawn.center() - rect.center()).length() < 1.0);
        // Too large: scaled down to fit.
        let drawn = image_rect(rect, [256, 128], 1.0);
        assert_eq!(drawn.size(), egui::vec2(30.0, 15.0));
    }

    #[test]
    fn glyphs_without_a_color_use_the_text_color() {
        let appearance = Appearance::default();
        assert_eq!(icon_color(None, &appearance), appearance.palette.text);
        assert_eq!(
            icon_color(Some(IconColor([1, 2, 3])), &appearance),
            Color32::from_rgb(1, 2, 3)
        );
    }

    #[cfg(feature = "symbols-font")]
    #[test]
    fn every_devicon_glyph_is_in_the_bundled_font() {
        let ctx = egui::Context::default();
        // The font is registered on first use and ready the next frame.
        for _ in 0..2 {
            let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
                symbols_family(ui.ctx());
            });
            frame.textures_delta.clear();
        }
        let family = symbols_family(&ctx).expect("symbols font registered");
        let font = FontId::new(14.0, family);
        let missing: Vec<_> = nfm_picker_sources::devicon::all()
            .filter(|icon| !ctx.fonts_mut(|fonts| fonts.has_glyph(&font, icon.glyph)))
            .map(|icon| format!("{:x}", icon.glyph as u32))
            .collect();
        assert!(missing.is_empty(), "missing glyphs: {missing:?}");
    }
}
