//! The query box: a prompt, one line of text to edit, and muted placeholder
//! text while it is empty. A picker's query and a prompt page's text are each
//! one of these.
use crate::Appearance;

pub(crate) struct QueryBox<'a> {
    /// The text box's egui id, which keeps its cursor and undo history.
    pub id: egui::Id,
    /// Drawn before the text.
    pub prompt: &'a str,
    /// Drawn muted in place of the text while there is none.
    pub placeholder: &'a str,
    pub interactive: bool,
    /// Width to leave at the right of the row, for what its owner draws there.
    pub reserve: f32,
}

pub(crate) struct QueryBoxOutput {
    pub response: egui::Response,
    /// Where the text starts.
    pub text_pos: egui::Pos2,
    /// The cursor's byte offset in the text; its end when there is no cursor.
    pub cursor: usize,
    pub state: egui::text_edit::TextEditState,
}

impl QueryBox<'_> {
    /// Draw the prompt and the text box in a horizontal row `row_height`
    /// tall. Tab, Escape and the arrow keys stay with the box's owner.
    pub(crate) fn show(
        self,
        ui: &mut egui::Ui,
        appearance: &Appearance,
        row_height: f32,
        text: &mut String,
    ) -> QueryBoxOutput {
        let prompt = ui.painter().layout_no_wrap(
            self.prompt.to_owned(),
            appearance.typography.normal.clone(),
            appearance.palette.accent,
        );
        let (prompt_rect, _) = ui.allocate_exact_size(
            egui::vec2(prompt.size().x, row_height),
            egui::Sense::hover(),
        );
        ui.painter().galley(
            egui::pos2(
                prompt_rect.left(),
                prompt_rect.center().y - prompt.size().y / 2.0,
            ),
            prompt,
            appearance.palette.accent,
        );
        let width = (ui.available_width() - self.reserve).max(1.0);
        let edit = egui::TextEdit::singleline(text)
            .id(self.id)
            .interactive(self.interactive)
            .font(appearance.typography.normal.clone())
            .vertical_align(egui::Align::Center)
            .margin(0)
            .min_size(egui::vec2(width, row_height))
            .desired_width(width)
            .frame(egui::Frame::NONE)
            .event_filter(egui::EventFilter {
                tab: true,
                escape: true,
                horizontal_arrows: true,
                vertical_arrows: true,
            })
            .show(ui);
        let cursor = edit.cursor_range.map_or(text.len(), |range| {
            text.char_indices()
                .nth(range.primary.index.into())
                .map_or(text.len(), |(offset, _)| offset)
        });
        if text.is_empty() && !self.placeholder.is_empty() {
            let rect = edit.response.rect;
            ui.painter()
                .with_clip_rect(rect.intersect(ui.clip_rect()))
                .text(
                    egui::pos2(edit.galley_pos.x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    self.placeholder,
                    appearance.typography.normal.clone(),
                    appearance.palette.muted,
                );
        }
        QueryBoxOutput {
            response: edit.response.response,
            text_pos: edit.galley_pos,
            cursor,
            state: edit.state,
        }
    }
}
