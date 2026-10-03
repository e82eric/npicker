//! Optional renderer for a context and window owned by the host.
use egui_glow::Painter;
pub use glow;
use std::sync::Arc;

pub struct Renderer {
    painter: Painter,
}
impl Renderer {
    /// The supplied GL context must be current on this thread during creation,
    /// rendering and destruction. This wrapper never makes a context current.
    pub fn new(gl: Arc<glow::Context>) -> Result<Self, String> {
        Painter::new(gl, "", None, false)
            .map(|painter| Self { painter })
            .map_err(|e| e.to_string())
    }
    /// Host binds its target framebuffer before calling. Size is the full target
    /// size in physical pixels; UI coordinates use logical points. This paints
    /// without clearing or swapping buffers. egui_glow changes GL state: restore
    /// host state or render this last. All frame texture deltas must be delivered.
    pub fn render(
        &mut self,
        context: &egui::Context,
        size_px: [u32; 2],
        output: &mut egui::FullOutput,
    ) {
        let primitives =
            context.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        self.painter.paint_and_update_textures(
            size_px,
            output.pixels_per_point,
            &primitives,
            &mut output.textures_delta,
        );
    }
    pub fn painter_mut(&mut self) -> &mut Painter {
        &mut self.painter
    }
    /// Call once with the context current, before destroying the host context.
    pub fn destroy(&mut self) {
        self.painter.destroy();
    }
}
