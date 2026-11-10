//! A prompt: a page that asks for one line of text. It is a query box in the
//! panel a picker is drawn in, with nothing to pick from, so it is one row
//! tall. Like a picker it reports what the user did and leaves what comes
//! next to its host.
use crate::picker::{
    DIVIDER_HEIGHT, IdSlot, PANEL_FRAME_HEIGHT, PANEL_FRAME_WIDTH, QUERY_HEIGHT_ALLOWANCE, divider,
    panel_frame, panel_style,
};
use crate::query_box::QueryBox;
use crate::{Appearance, Placement, Position};
use egui::{Context, Id, Key, Modifiers};

/// Rows an error takes above the text.
const ERROR_ROWS: f32 = 3.0;

#[derive(Clone, Debug)]
pub struct PromptConfig {
    /// Drawn before the text: what the text is for, such as a command's name.
    pub prompt: String,
    /// Muted text in the box while it is empty.
    pub placeholder: String,
    pub initial_text: String,
}
impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            prompt: ">".into(),
            placeholder: String::new(),
            initial_text: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptEvent {
    /// Enter: the text as typed.
    Submitted(String),
    /// Escape.
    Cancelled,
}

pub struct PromptOutput {
    pub event: Option<PromptEvent>,
    pub wants_keyboard: bool,
    pub rect: egui::Rect,
}

pub struct Prompt {
    id: Id,
    /// Holds `id` for as long as the prompt lives.
    _id_slot: IdSlot,
    pub config: PromptConfig,
    text: String,
    error: Option<String>,
    /// Take keyboard focus when next drawn.
    focus: bool,
}

/// What a prompt showing no error is taller than its row of text by, in
/// points: for a host that sizes a window before there is a frame to ask
/// `prompt_height` in.
pub const PROMPT_FRAME_HEIGHT: f32 = PANEL_FRAME_HEIGHT + QUERY_HEIGHT_ALLOWANCE;

/// The height, in points, of a prompt showing no error. Hosts that size a
/// window to a prompt add the placement's margins.
pub fn prompt_height(ctx: &Context, appearance: &Appearance) -> f32 {
    appearance.typography.preview_row_height(ctx) + PROMPT_FRAME_HEIGHT
}

impl Prompt {
    /// Prompts and pickers made with the same `id` may be alive together;
    /// each keeps its own cursor and undo history.
    pub fn new(id: impl egui::AsId, config: PromptConfig) -> Self {
        let id_slot = IdSlot::take(Id::new(id));
        Self {
            id: id_slot.id(),
            _id_slot: id_slot,
            text: config.initial_text.clone(),
            config,
            error: None,
            focus: true,
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }
    /// Show `error` above the text until it is set to `None`.
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }
    /// The height, in points, the prompt takes as it is now: taller while it
    /// shows an error. Hosts that size a window to it add the placement's
    /// margins.
    pub fn height(&self, ctx: &Context, appearance: &Appearance) -> f32 {
        prompt_height(ctx, appearance)
            + if self.error.is_some() {
                ERROR_ROWS * appearance.typography.preview_row_height(ctx) + DIVIDER_HEIGHT
            } else {
                0.0
            }
    }
    /// Draw within an existing host egui pass. A disabled prompt is drawn
    /// and takes no input or keyboard focus. A full-pane placement is shown
    /// at the bottom of its bounds: a prompt has nothing to fill a pane with.
    pub fn show(
        &mut self,
        ctx: &Context,
        mut placement: Placement,
        appearance: &Appearance,
        disabled: bool,
    ) -> PromptOutput {
        let interactive = !disabled;
        let input_id = self.id.with("query");
        ctx.memory_mut(|memory| {
            if interactive {
                memory.request_focus(input_id);
            } else if memory.has_focus(input_id) {
                memory.surrender_focus(input_id);
            }
        });
        let focused = ctx.memory(|m| {
            m.has_focus(input_id) || (m.focused().is_none() && m.had_focus_last_frame(input_id))
        }) || self.focus;
        let mut event = None;
        // Enter is reported once the box has this frame's typing.
        let mut submit = false;
        if interactive && focused {
            ctx.input_mut(|input| {
                if input.consume_key(Modifiers::NONE, Key::Escape) {
                    event = Some(PromptEvent::Cancelled);
                }
                if input.consume_key(Modifiers::NONE, Key::Enter) {
                    submit = true;
                }
            });
        }
        if placement.position == Position::FullPane {
            placement.position = Position::Bottom;
        }
        let row_height = appearance.typography.preview_row_height(ctx);
        let error_height = if self.error.is_some() {
            ERROR_ROWS * row_height
        } else {
            0.0
        };
        let rect = placement.rect(self.height(ctx, appearance));
        if rect.is_positive() {
            let previous_size =
                ctx.memory(|memory| memory.area_rect(self.id).map(|rect| rect.size()));
            let area = egui::Area::new(self.id)
                .fade_in(false)
                .order(if interactive {
                    egui::Order::Foreground
                } else {
                    egui::Order::Middle
                })
                .interactable(interactive)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    ui.set_clip_rect(rect.intersect(placement.bounds).intersect(ui.clip_rect()));
                    panel_frame(appearance).show(ui, |ui| {
                        ui.set_width((rect.width() - PANEL_FRAME_WIDTH).max(1.0));
                        panel_style(ui, appearance);
                        if let Some(error) = &self.error {
                            self.error_ui(ui, appearance, error, error_height);
                            divider(ui, appearance);
                        }
                        ui.horizontal(|ui| {
                            let edit = QueryBox {
                                id: input_id,
                                prompt: &self.config.prompt,
                                placeholder: &self.config.placeholder,
                                interactive,
                                reserve: 0.0,
                            }
                            .show(ui, appearance, row_height, &mut self.text);
                            if interactive && self.focus {
                                edit.response.request_focus();
                                self.focus = false;
                            }
                        });
                    });
                });
            // The first frame lays the panel out; show the next one.
            if event.is_none()
                && !submit
                && previous_size.is_none_or(|size| {
                    (size.x - area.response.rect.width()).abs() > 0.5
                        || (size.y - area.response.rect.height()).abs() > 0.5
                })
            {
                ctx.request_discard("resolve prompt size before presentation");
            }
        }
        if submit && event.is_none() {
            event = Some(PromptEvent::Submitted(self.text.clone()));
        }
        PromptOutput {
            event,
            wants_keyboard: interactive && ctx.memory(|m| m.has_focus(input_id)),
            rect,
        }
    }
    fn error_ui(&self, ui: &mut egui::Ui, appearance: &Appearance, error: &str, height: f32) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::hover(),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            egui::ScrollArea::vertical()
                .id_salt(self.id.with("error_message"))
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(error)
                                .font(appearance.typography.normal.clone())
                                .color(appearance.palette.error),
                        )
                        .wrap(),
                    );
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(
        ctx: &Context,
        prompt: &mut Prompt,
        events: Vec<egui::Event>,
        disabled: bool,
    ) -> (egui::FullOutput, PromptOutput) {
        let mut output = None;
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let shown = prompt.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    disabled,
                );
                // A discarded pass is run again; keep the event of either.
                let event = output
                    .take()
                    .and_then(|earlier: PromptOutput| earlier.event)
                    .or(shown.event.clone());
                output = Some(PromptOutput { event, ..shown });
            },
        );
        frame.textures_delta.clear();
        (frame, output.expect("the prompt was drawn"))
    }
    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    fn shows_text(frame: &egui::FullOutput, needle: &str) -> bool {
        frame.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains(needle))
        })
    }
    fn grep() -> Prompt {
        Prompt::new(
            "prompt-tests",
            PromptConfig {
                prompt: "Grep".into(),
                placeholder: "<search text>".into(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_prompt_reports_its_text_as_typed_and_leaves_what_comes_next_to_its_host() {
        let ctx = Context::default();
        let mut prompt = grep();
        draw(&ctx, &mut prompt, vec![], false);
        let (frame, output) = draw(&ctx, &mut prompt, vec![], false);
        assert!(output.event.is_none());
        assert!(output.wants_keyboard);
        assert!(shows_text(&frame, "Grep"), "the prompt");
        assert!(shows_text(&frame, "<search text>"), "the placeholder");
        // One row: the text and the panel around it.
        let appearance = Appearance::default();
        assert_eq!(output.rect.height(), prompt_height(&ctx, &appearance));
        assert!(output.rect.height() < 3.0 * appearance.typography.preview_row_height(&ctx));

        // Text typed in the same frame as Enter is part of what is submitted.
        let (_, output) = draw(
            &ctx,
            &mut prompt,
            vec![egui::Event::Text("fn \"main\" ".into()), key(Key::Enter)],
            false,
        );
        assert_eq!(
            output.event,
            Some(PromptEvent::Submitted("fn \"main\" ".into()))
        );
        // It stays as it was, for a page shown on top to come back to.
        let (frame, output) = draw(&ctx, &mut prompt, vec![], false);
        assert!(output.event.is_none());
        assert_eq!(prompt.text(), "fn \"main\" ");
        assert!(!shows_text(&frame, "<search text>"));

        let (_, output) = draw(&ctx, &mut prompt, vec![key(Key::Escape)], false);
        assert_eq!(output.event, Some(PromptEvent::Cancelled));
        assert_eq!(prompt.text(), "fn \"main\" ");
    }

    #[test]
    fn a_disabled_prompt_is_drawn_and_takes_no_keys() {
        let ctx = Context::default();
        let mut prompt = grep();
        draw(&ctx, &mut prompt, vec![], false);
        let (frame, output) = draw(
            &ctx,
            &mut prompt,
            vec![egui::Event::Text("x".into()), key(Key::Enter)],
            true,
        );
        assert!(output.event.is_none() && !output.wants_keyboard);
        assert_eq!(prompt.text(), "");
        assert!(shows_text(&frame, "Grep"));
    }

    #[test]
    fn an_error_shows_above_the_text_until_it_is_cleared() {
        let ctx = Context::default();
        let mut prompt = grep();
        draw(&ctx, &mut prompt, vec![], false);
        let plain = draw(&ctx, &mut prompt, vec![], false).1.rect.height();
        prompt.set_error(Some("rg was not found".into()));
        draw(&ctx, &mut prompt, vec![], false);
        let (frame, output) = draw(&ctx, &mut prompt, vec![], false);
        assert!(shows_text(&frame, "rg was not found"));
        assert!(output.rect.height() > plain);
        prompt.set_error(None);
        draw(&ctx, &mut prompt, vec![], false);
        let (frame, output) = draw(&ctx, &mut prompt, vec![], false);
        assert!(!shows_text(&frame, "rg was not found"));
        assert_eq!(output.rect.height(), plain);
    }

    #[test]
    fn prompts_of_one_name_alive_together_keep_their_own_text() {
        let ctx = Context::default();
        let mut first = grep();
        draw(&ctx, &mut first, vec![egui::Event::Text("one".into())], false);
        // A second of the same name, on top; the first is not drawn.
        let mut second = grep();
        assert_ne!(first.id, second.id);
        draw(&ctx, &mut second, vec![], false);
        draw(&ctx, &mut second, vec![egui::Event::Text("two".into())], false);
        assert_eq!((first.text(), second.text()), ("one", "two"));
        drop(second);
        let (_, output) = draw(
            &ctx,
            &mut first,
            vec![egui::Event::Text("!".into()), key(Key::Enter)],
            false,
        );
        assert_eq!(output.event, Some(PromptEvent::Submitted("one!".into())));
    }
}
