//! Picker adapter for the shared copy-mode state machine.
use crate::copy_document::DocumentRow;
use crate::copy_mode::{
    CellPosition, CellRange, CopyAction, CopyMode, CopySelection, Motion, View, YankCommand,
    yank_pulse_strength, yank_pulse_timing,
};
use crate::{Appearance, Preview, preview};
use egui::{Context, Event, Key, Modifiers};
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
mod yank_paint_tests {
    use super::*;
    use crate::copy_mode::{VisualMode, YANK_HIGHLIGHT_DURATION};

    /// A viewport on ANSI text laid out `columns` wide.
    fn ansi(text: &str, columns: usize) -> PreviewCopyMode {
        let layout =
            crate::styled_document::StyledDocument::from_ansi(text.as_bytes(), 1 << 20, 100)
                .unwrap()
                .layout(columns, &crate::Cancellation::test())
                .unwrap();
        PreviewCopyMode::host(layout, 0, 4).unwrap()
    }

    #[test]
    fn preview_selection_preserves_syntax_colors_and_cursor_contrast() {
        let red = egui::Color32::from_rgb(220, 80, 90);
        let green = egui::Color32::from_rgb(120, 190, 100);
        for visual in [
            VisualMode::Characterwise,
            VisualMode::Linewise,
            VisualMode::Blockwise,
        ] {
            let ctx = Context::default();
            let appearance = Appearance::default();
            // Bold red "a", inverse "b" on green, red "c".
            let mut copy = ansi(
                "\x1b[1;38;2;220;80;90ma\x1b[0m\x1b[7;30;48;2;120;190;100mb\x1b[0m\
                 \x1b[38;2;220;80;90mc\x1b[0m",
                3,
            );
            copy.mode.toggle_visual(visual);
            copy.mode.cursor = CellPosition { x: 2, y: 0 };
            let output = paint(&ctx, &mut copy, &[]);
            let glyph_color = |glyph: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == glyph => {
                            Some(text.galley.job.sections[0].format.color)
                        }
                        _ => None,
                    })
                    .expect("glyph must render")
            };
            assert_eq!(glyph_color("a"), red);
            assert_eq!(glyph_color("b"), green);
            assert_eq!(glyph_color("c"), appearance.palette.background);
            assert!(colored_cells(&output, appearance.palette.preview_selection_background) >= 2);
            assert!(colored_cells(&output, appearance.palette.accent) >= 1);
            let yanked = paint(&ctx, &mut copy, &[key(Key::Y)]);
            assert!(
                yanked
                    .platform_output
                    .commands
                    .contains(&egui::OutputCommand::CopyText(
                        if visual == VisualMode::Linewise {
                            "abc\n"
                        } else {
                            "abc"
                        }
                        .into()
                    ))
            );
        }
    }

    #[test]
    fn preview_motion_adapter_preserves_logical_columns_and_hard_line_breaks() {
        let ctx = Context::default();
        let preview = Preview::Text("abcd\nefgh\nij\nklmn\nopqr".into());
        let mut plain = PreviewCopyMode::text(&preview, 0, 4).unwrap();
        plain.mode.cursor = CellPosition { x: 2, y: 1 };
        plain.input(&ctx, &[key(Key::K)], 4);
        assert_eq!(plain.mode.cursor, CellPosition { x: 2, y: 0 });

        // The same rows, with the first and fourth soft-wrapping.
        let mut wrapped = ansi("abcdefgh\nij\nklmnopqr", 4);
        wrapped.mode.cursor = CellPosition { x: 2, y: 1 };
        wrapped.input(&ctx, &[key(Key::J)], 4);
        assert_eq!(wrapped.mode.cursor, CellPosition { x: 1, y: 2 });
        wrapped.input(&ctx, &[key(Key::J)], 4);
        assert_eq!(wrapped.mode.cursor, CellPosition { x: 2, y: 4 });
        wrapped.input(&ctx, &[key(Key::Num2), key(Key::K)], 4);
        assert_eq!(wrapped.mode.cursor, CellPosition { x: 2, y: 1 });
    }

    #[test]
    fn host_documents_start_the_cursor_on_their_anchor_while_it_is_visible() {
        use crate::copy_document::{CopyDocument, DocumentMetadata, DocumentRow};
        use std::sync::Arc;
        struct Anchored(PinnedDocument, CellPosition);
        impl CopyDocument for Anchored {
            fn metadata(&self) -> Result<DocumentMetadata, String> {
                Ok(DocumentMetadata {
                    revision: 1,
                    columns: self.0.columns(),
                    total_rows: self.0.rows(),
                })
            }
            fn text_row(&self, y: u64) -> Result<Option<DocumentRow>, String> {
                Ok(self.0.row(y))
            }
            fn resolve_cell(
                &self,
                cell: CellPosition,
                right: bool,
            ) -> Result<CellPosition, String> {
                self.0.resolve(cell, right).ok_or("cell".into())
            }
            fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
                self.0.selection_text(selection).ok_or("selection".into())
            }
            fn viewport(
                &self,
                top: u64,
                rows: usize,
                appearance: &Appearance,
            ) -> Result<Preview, String> {
                self.0.grid(top, rows, appearance).ok_or("viewport".into())
            }
            fn line_number(&self, row: u64) -> Option<u64> {
                self.0.handle.line_number(row)
            }
            fn anchor(&self) -> Option<CellPosition> {
                Some(self.1)
            }
        }
        let text = Preview::Text(vec!["line"; 40].join("\n"));
        let anchored = |anchor| {
            let document = PreviewCopyMode::text(&text, 0, 4).unwrap().document;
            Arc::new(Anchored(document, anchor)) as Arc<dyn CopyDocument>
        };
        let anchor = CellPosition { x: 2, y: 12 };
        let copy = PreviewCopyMode::host(anchored(anchor), 10, 5).unwrap();
        assert_eq!(copy.mode.cursor, anchor);
        // Scrolled away from the anchor, the cursor starts at the viewport's top.
        let copy = PreviewCopyMode::host(anchored(anchor), 20, 5).unwrap();
        assert_eq!(copy.mode.cursor, CellPosition { x: 0, y: 20 });
    }

    #[test]
    fn host_document_navigates_searches_and_yanks_beyond_the_visible_page() {
        use crate::copy_document::{CopyDocument, DocumentMetadata, DocumentRow};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Host(
            PinnedDocument,
            Arc<AtomicBool>,
            std::thread::ThreadId,
            Arc<AtomicBool>,
        );
        impl CopyDocument for Host {
            fn metadata(&self) -> Result<DocumentMetadata, String> {
                if self.1.load(Ordering::Relaxed) {
                    return Err("closed".into());
                }
                Ok(DocumentMetadata {
                    revision: 7,
                    columns: self.0.columns(),
                    total_rows: self.0.rows(),
                })
            }
            fn text_row(&self, y: u64) -> Result<Option<DocumentRow>, String> {
                self.metadata()?;
                if std::thread::current().id() != self.2 {
                    self.3.store(true, Ordering::Relaxed);
                }
                Ok(self.0.row(y))
            }
            fn resolve_cell(
                &self,
                cell: CellPosition,
                right: bool,
            ) -> Result<CellPosition, String> {
                self.metadata()?;
                self.0.resolve(cell, right).ok_or("cell".into())
            }
            fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
                self.metadata()?;
                self.0.selection_text(selection).ok_or("selection".into())
            }
            fn viewport(
                &self,
                top: u64,
                rows: usize,
                appearance: &Appearance,
            ) -> Result<Preview, String> {
                self.metadata()?;
                self.0.grid(top, rows, appearance).ok_or("viewport".into())
            }
            fn line_number(&self, row: u64) -> Option<u64> {
                self.0.handle.line_number(row)
            }
        }
        let mut lines = vec!["short"; 200];
        lines[180] = "needle 猫";
        let original = PreviewCopyMode::text(&Preview::Text(lines.join("\n")), 0, 4).unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let searched_on_worker = Arc::new(AtomicBool::new(false));
        let host = Arc::new(Host(
            original.document,
            closed.clone(),
            std::thread::current().id(),
            searched_on_worker.clone(),
        ));
        let mut copy = PreviewCopyMode::host(host, 0, 4).unwrap();
        let ctx = Context::default();
        copy.input(&ctx, &[key(Key::Slash), Event::Text("short".into())], 4);
        let superseded = copy.search_job.as_ref().unwrap().cancel.clone();
        copy.input(
            &ctx,
            &[
                key(Key::Backspace),
                key(Key::Backspace),
                key(Key::Backspace),
                key(Key::Backspace),
                key(Key::Backspace),
                Event::Paste("needle".into()),
                key(Key::Enter),
            ],
            4,
        );
        assert!(superseded.load(Ordering::Relaxed));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while copy.search_job.is_some() {
            copy.poll_search();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(searched_on_worker.load(Ordering::Relaxed));
        assert_eq!(copy.mode.cursor, CellPosition { x: 0, y: 180 });
        copy.reveal(4);
        let origin = (copy.mode.cursor, copy.top, copy.left);
        copy.input(&ctx, &[key(Key::Slash), Event::Text("short".into())], 4);
        while copy.search_job.is_some() {
            copy.poll_search();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        copy.reveal(4);
        let displayed = (copy.mode.cursor, copy.top, copy.left);
        assert_ne!(displayed.0, origin.0);
        let displayed_matches = copy.mode.search_matches().to_vec();
        assert!(!displayed_matches.is_empty());
        copy.input(&ctx, &[Event::Text(".*".into())], 4);
        assert_eq!(
            copy.pending_search_highlights.as_deref(),
            Some(displayed_matches.as_slice()),
            "old highlights must remain visible until replacement results arrive"
        );
        assert_eq!(
            (copy.mode.cursor, copy.top, copy.left),
            displayed,
            "editing a live query must not jump back to the search origin"
        );
        let cancelled = copy.search_job.as_ref().unwrap().cancel.clone();
        copy.input(&ctx, &[key(Key::Escape)], 4);
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!((copy.mode.cursor, copy.top, copy.left), origin);
        assert!(copy.search_job.is_none());
        assert!(copy.pending_search_highlights.is_none());
        let output = paint(&ctx, &mut copy, &[key(Key::Y), key(Key::Y)]);
        assert!(copy.top > 0);
        assert!(
            output
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("needle 猫\n".into()))
        );
        copy.mode.cursor = CellPosition { x: 7, y: 180 };
        copy.mode.toggle_visual(VisualMode::Blockwise);
        let output = paint(&ctx, &mut copy, &[key(Key::Y)]);
        assert!(
            output
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("猫".into()))
        );
        assert!(copy.open_quick_select(true, 4));
        let (label, target) = copy.first_quick_hint().unwrap();
        let _ = paint(&ctx, &mut copy, &[Event::Text(label)]);
        assert_eq!(copy.mode.cursor, target);
        assert!(!copy.quick_select_active());
        closed.store(true, Ordering::Relaxed);
        assert!(!copy.valid());
        assert!(copy.input(&ctx, &[], 4));
    }

    #[test]
    fn preview_cursor_uses_font_line_height_instead_of_picker_row_spacing() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = Context::default();
            ctx.set_pixels_per_point(scale);
            let mut appearance = Appearance::default();
            appearance.typography.normal = egui::FontId::monospace(15.0);
            appearance.typography.bold = egui::FontId::monospace(15.0);
            appearance.typography.row_height = 30.0;
            appearance.typography.cell_width = 9.0;
            let mut copy = PreviewCopyMode::text(&Preview::Text("M\nsecond".into()), 0, 4).unwrap();
            let mut measured = 0.0;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                measured = ui.fonts_mut(|fonts| fonts.row_height(&appearance.typography.normal));
                copy.paint(ui, &appearance, 4);
            });
            output.textures_delta.clear();
            let cursor = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect) if rect.fill == appearance.palette.accent => {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("painted copy cursor");
            assert!((cursor.height() - (measured + 6.0)).abs() <= 1.01 / scale);
            assert!(cursor.height() < 26.0);
            assert_eq!(cursor.width(), 9.0);
            assert!(
                (appearance.typography.preview_row_height(&ctx) - cursor.height()).abs() < 0.01
            );
            assert_eq!(appearance.typography.row_height, 30.0);
        }
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn preview_reveal_uses_default_and_configured_scroll_margin() {
        let mut copy = PreviewCopyMode::text(
            &Preview::Text((0..100).map(|row| format!("row{row}\n")).collect()),
            0,
            14,
        )
        .unwrap();
        copy.top = 20;
        copy.mode.cursor.y = 20;
        copy.reveal(14);
        assert_eq!(copy.top, 18);
        copy.top = 20;
        copy.set_scroll_padding(0);
        copy.reveal(14);
        assert_eq!(copy.top, 20);
        copy.set_scroll_padding(5);
        copy.reveal(14);
        assert_eq!(copy.top, 15);
        copy.top = 20;
        copy.set_scroll_padding(u32::MAX);
        copy.reveal(4);
        assert_eq!(copy.top, 19);
    }

    #[test]
    fn cursor_reveal_keeps_three_row_margin_and_clamps_at_document_edges() {
        let mut copy = PreviewCopyMode::text(
            &Preview::Text((0..100).map(|row| format!("row{row}\n")).collect()),
            0,
            14,
        )
        .unwrap();
        assert_eq!(copy.mode.scroll_padding(), 2);
        copy.set_scroll_padding(3);
        copy.top = 20;
        copy.mode.cursor.y = 23;
        copy.reveal(14);
        assert_eq!(copy.top, 20);
        copy.mode.cursor.y = 22;
        copy.reveal(14);
        assert_eq!(copy.top, 19);
        copy.mode.cursor.y = 30;
        copy.reveal(14);
        assert_eq!(copy.top, 20);
        copy.mode.cursor.y = 0;
        copy.reveal(14);
        assert_eq!(copy.top, 0);
        // 100 rows: the final line break adds no row.
        copy.mode.cursor.y = 100;
        copy.reveal(14);
        assert_eq!(copy.top, 86);
        copy.top = 20;
        copy.mode.cursor.y = 20;
        copy.reveal(2);
        assert_eq!(copy.top, 20); // Short viewports reduce the margin.
        copy.mode.cursor.y = 21;
        copy.reveal(1);
        assert_eq!(copy.top, 21);
        copy.top = 20;
        copy.mode.cursor.y = 20;
        copy.scroll(14, 14);
        assert_eq!(copy.top, 34);
        assert_eq!(copy.mode.cursor.y, 37);
        copy.reveal(14);
        assert_eq!(copy.top, 34); // Returning to focus must not undo paging.
    }

    #[test]
    fn quick_select_preserves_styled_foreground_background_and_inverse() {
        let ctx = Context::default();
        let appearance = Appearance::default();
        let syntax = egui::Color32::from_rgb(220, 80, 90);
        // "word" in syntax red on black, with an inverse "r".
        let mut copy = ansi(
            "\x1b[38;2;220;80;90;48;2;0;0;0mwo\x1b[7mr\x1b[27md\x1b[0m",
            4,
        );
        let _ = paint(&ctx, &mut copy, &[]);
        assert!(copy.open_quick_select(true, 4));
        let output = paint(&ctx, &mut copy, &[]);
        assert_eq!(
            colored_cells(&output, appearance.palette.preview_selection_background),
            0
        );
        assert!(colored_cells(&output, syntax) >= 1); // Inverse target's displayed background.
        assert!(colored_cells(&output, egui::Color32::BLACK) >= 1);
        for (glyph, expected) in [("r", egui::Color32::BLACK), ("d", syntax)] {
            let color = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == glyph => {
                        Some(text.galley.job.sections[0].format.color)
                    }
                    _ => None,
                })
                .expect("target glyph must render");
            assert_eq!(color, expected);
        }
        copy.cancel_quick_select();
        assert!(!copy.quick_select_active());
        let restored = paint(&ctx, &mut copy, &[]);
        assert!(restored.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "d"
                && text.galley.job.sections[0].format.color == syntax)));
    }

    #[test]
    fn quick_select_draws_labels_pages_without_cursor_snap_and_yanks_lines() {
        let ctx = Context::default();
        let appearance = Appearance::default();
        let mut copy = PreviewCopyMode::text(
            &Preview::Text((0..20).map(|row| format!("word{row} tail\n")).collect()),
            0,
            4,
        )
        .unwrap();
        let _ = paint(&ctx, &mut copy, &[]);
        assert!(copy.open_quick_select(true, 4));
        let output = paint(&ctx, &mut copy, &[]);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "A")));
        // Labels stand out through text styling without adding background fills.
        let hint_color = egui::Color32::from_rgb(0xfa, 0xbd, 0x2f);
        assert_eq!(colored_cells(&output, appearance.palette.accent), 0);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "A"
                && text.galley.job.sections[0].format.color == hint_color)));
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::LineSegment { stroke, .. } if stroke.color == hint_color)));
        let _ = paint(&ctx, &mut copy, &[key(Key::PageDown)]);
        assert_eq!(copy.top, 4);
        let _ = paint(&ctx, &mut copy, &[]);
        assert_eq!(copy.top, 4);
        let origin = copy.mode.cursor;
        copy.quick_select
            .as_mut()
            .unwrap()
            .set_purpose(crate::quick_select::Purpose::YankLines { origin });
        let label = copy.first_quick_hint().unwrap().0;
        let output = paint(&ctx, &mut copy, &[Event::Text(label)]);
        assert!(!copy.quick_select_active());
        assert!(output.platform_output.commands.iter().any(|command| matches!(command,
            egui::OutputCommand::CopyText(text) if text.contains("word0") && text.contains("word7"))));
        assert_eq!(copy.mode.cursor, origin);
    }

    #[test]
    fn quick_line_yank_tints_the_origin_row_and_labels_rows() {
        let ctx = Context::default();
        let appearance = Appearance::default();
        let mut copy = PreviewCopyMode::text(
            &Preview::Text((0..20).map(|row| format!("word{row} tail\n")).collect()),
            0,
            4,
        )
        .unwrap();
        let _ = paint(&ctx, &mut copy, &[]);
        assert!(copy.open_quick_select(true, 4));
        let tint = appearance.palette.cursor_line_colors().0;
        assert_eq!(colored_cells(&paint(&ctx, &mut copy, &[]), tint), 0);
        let origin = copy.mode.cursor;
        copy.quick_select
            .as_mut()
            .unwrap()
            .set_purpose(crate::quick_select::Purpose::YankLines { origin });
        assert!(colored_cells(&paint(&ctx, &mut copy, &[]), tint) >= 1);
        // The nearest row below the origin gets the first label.
        let quick = copy.quick_select.as_ref().unwrap();
        let nearest = quick.hints.iter().find(|hint| hint.label == "a").unwrap();
        assert_eq!(nearest.range.start.y, origin.y + 1);
    }

    #[test]
    fn search_prompt_toggles_regex_navigates_and_reports_invalid_patterns() {
        fn ctrl(key: Key) -> Event {
            Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }
        }
        let ctx = Context::default();
        let mut copy = PreviewCopyMode::text(&Preview::Text("a.b axb a.b".into()), 0, 4).unwrap();
        let _ = paint(
            &ctx,
            &mut copy,
            &[key(Key::Slash), Event::Text("a.b".into())],
        );
        assert_eq!(copy.mode.search_matches().len(), 2);
        assert!(!copy.search_regex);
        let _ = paint(&ctx, &mut copy, &[ctrl(Key::R)]);
        assert!(copy.search_regex);
        assert_eq!(copy.mode.search_matches().len(), 3);
        assert_eq!(copy.mode.cursor.x, 4);
        let _ = paint(&ctx, &mut copy, &[ctrl(Key::N)]);
        assert_eq!(copy.mode.cursor.x, 8);
        let _ = paint(&ctx, &mut copy, &[ctrl(Key::P)]);
        assert_eq!(copy.mode.cursor.x, 4);
        let _ = paint(&ctx, &mut copy, &[Event::Text("[".into())]);
        assert!(copy.search_error.is_some());
        assert!(copy.mode.search_matches().is_empty());
        let _ = paint(&ctx, &mut copy, &[key(Key::Backspace)]);
        assert!(copy.search_error.is_none());
        assert_eq!(copy.mode.search_matches().len(), 3);
        let _ = paint(&ctx, &mut copy, &[key(Key::Escape)]);
        assert_eq!(copy.mode.cursor.x, 0);
    }

    #[test]
    fn preview_search_matches_terminal_literal_and_regex_semantics() {
        let copy =
            PreviewCopyMode::text(&Preview::Text("foo FOO Foo a.b axb é É".into()), 0, 4).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            search_matches(&copy.document, "Foo", false, &cancel)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            search_matches(&copy.document, "Foo", true, &cancel)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_matches(&copy.document, "a.b", false, &cancel)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_matches(&copy.document, "a.b", true, &cancel)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            search_matches(&copy.document, "é", false, &cancel)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            search_matches(&copy.document, "(?i)FOO", true, &cancel)
                .unwrap()
                .len(),
            3
        );
        assert!(search_matches(&copy.document, "[", true, &cancel).is_err());
    }

    #[test]
    fn live_search_hides_cursor_and_colors_entire_active_match() {
        let ctx = Context::default();
        let mut appearance = Appearance::default();
        appearance.palette.search_active_background = Some(egui::Color32::BLUE);
        appearance.palette.search_active_text = Some(egui::Color32::WHITE);
        let mut copy = PreviewCopyMode::text(&Preview::Text("foo foo".into()), 0, 4).unwrap();
        let output = paint(
            &ctx,
            &mut copy,
            &[
                key(Key::Slash),
                Event::Text("/".into()),
                Event::Text("foo".into()),
            ],
        );
        assert!(copy.search_input.is_some());
        drop(output);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            copy.paint(ui, &appearance, 4)
        });
        output.textures_delta.clear();
        assert_eq!(colored_cells(&output, egui::Color32::BLUE), 3);
        assert_eq!(colored_cells(&output, appearance.palette.accent), 0);
        assert_eq!(copy.mode.cursor.x, 4);
        let output = paint(&ctx, &mut copy, &[key(Key::Enter)]);
        assert_eq!(colored_cells(&output, appearance.palette.accent), 1);
        assert_eq!(copy.mode.cursor.x, 4);
        drop(output);
        let output = paint(
            &ctx,
            &mut copy,
            &[
                key(Key::Slash),
                Event::Text("/".into()),
                Event::Text("foo".into()),
                key(Key::Escape),
            ],
        );
        assert!(copy.search_input.is_none());
        assert_eq!(copy.mode.cursor.x, 4);
        assert_eq!(colored_cells(&output, appearance.palette.accent), 1);
    }

    #[test]
    fn preview_uses_explicit_search_and_yank_colors() {
        let ctx = Context::default();
        let mut appearance = Appearance::default();
        appearance.palette.search_match_background = Some(egui::Color32::BLUE);
        appearance.palette.search_match_text = Some(egui::Color32::WHITE);
        appearance.palette.yank_background = Some(egui::Color32::RED);
        let mut copy = PreviewCopyMode::text(&Preview::Text("ab".into()), 0, 4).unwrap();
        let range = CellRange {
            start: CellPosition { x: 1, y: 0 },
            end: CellPosition { x: 1, y: 0 },
        };
        copy.mode.adopt_search(1, vec![range]);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            copy.paint(ui, &appearance, 4)
        });
        output.textures_delta.clear();
        assert_eq!(colored_cells(&output, egui::Color32::BLUE), 1);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "b"
                && text.galley.job.sections[0].format.color == egui::Color32::WHITE)));
        copy.mode.cursor = range.start;
        copy.mode
            .toggle_visual(crate::copy_mode::VisualMode::Characterwise);
        copy.mode.visual_trim = false;
        copy.input(&ctx, &[key(Key::Y)], 4);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            copy.paint(ui, &appearance, 4)
        });
        output.textures_delta.clear();
        assert_eq!(colored_cells(&output, egui::Color32::RED), 1);
        // The yank only changes the background, so "b" keeps the same color
        // as the untouched "a".
        let color = |glyph: &str| {
            output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == glyph => {
                    Some(text.galley.job.sections[0].format.color)
                }
                _ => None,
            })
        };
        assert_eq!(color("b"), color("a"));
    }

    #[test]
    fn live_search_scrolls_on_edits_accepts_without_advancing_and_restores_on_escape() {
        let ctx = Context::default();
        let mut lines = vec!["unrelated"; 40];
        lines[10] = "alpha";
        lines[30] = "alpine";
        let mut copy = PreviewCopyMode::text(&Preview::Text(lines.join("\n")), 0, 4).unwrap();
        copy.input(
            &ctx,
            &[
                key(Key::Slash),
                Event::Text("/".into()),
                Event::Text("alp".into()),
            ],
            4,
        );
        assert_eq!(copy.mode.cursor.y, 10);
        assert!(copy.top > 0);
        copy.input(&ctx, &[Event::Paste("ine".into())], 4);
        assert_eq!(copy.mode.cursor.y, 30);
        copy.input(
            &ctx,
            &[
                key(Key::Backspace),
                key(Key::Backspace),
                key(Key::Backspace),
            ],
            4,
        );
        assert_eq!(copy.mode.cursor.y, 10);
        copy.input(&ctx, &[Event::Text("[".into())], 4);
        assert!(copy.mode.search_matches().is_empty());
        assert_eq!(copy.mode.cursor.y, 0);
        copy.input(
            &ctx,
            &[
                key(Key::Backspace),
                Event::Text("ine".into()),
                key(Key::Enter),
            ],
            4,
        );
        assert_eq!(copy.mode.cursor.y, 30);
        assert!(copy.search_input.is_none());
        let accepted = (copy.mode.cursor, copy.top, copy.left);
        copy.input(
            &ctx,
            &[key(Key::Questionmark), Event::Text("alp".into())],
            4,
        );
        assert_eq!(copy.mode.cursor.y, 10);
        copy.input(&ctx, &[key(Key::Escape)], 4);
        assert_eq!((copy.mode.cursor, copy.top, copy.left), accepted);
        assert!(copy.search_input.is_none());
        copy.input(
            &ctx,
            &[
                key(Key::Slash),
                Event::Text("alp".into()),
                key(Key::Backspace),
                key(Key::Backspace),
                key(Key::Backspace),
            ],
            4,
        );
        assert_eq!((copy.mode.cursor, copy.top, copy.left), accepted);
        assert!(copy.mode.search_matches().is_empty());
    }

    #[test]
    fn focus_preserves_preview_glyph_positions_and_sizes() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = Context::default();
            ctx.set_pixels_per_point(scale);
            let appearance = Appearance::default();
            for mut copy in [
                PreviewCopyMode::text(&Preview::Text("one 猫\nsecond".into()), 0, 4).unwrap(),
                ansi("\x1b[1mstyled 猫\x1b[0m", 9),
            ] {
                // The unfocused preview paints the document's page.
                let page = copy.viewport(4, &appearance).unwrap();
                let mut before = ctx.run_ui(egui::RawInput::default(), |ui| {
                    preview::paint(ui, &page, &appearance, 4);
                });
                let mut after = ctx.run_ui(egui::RawInput::default(), |ui| {
                    copy.paint(ui, &appearance, 4);
                });
                before.textures_delta.clear();
                after.textures_delta.clear();
                let glyphs = |frame: &egui::FullOutput| -> Vec<_> {
                    frame
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) if !text.galley.text().is_empty() => {
                                Some((text.galley.text().to_owned(), text.pos, text.galley.size()))
                            }
                            _ => None,
                        })
                        .collect()
                };
                assert_eq!(glyphs(&before), glyphs(&after), "DPI scale {scale}");
            }
        }
    }
    #[test]
    fn unfocused_viewport_preserves_scroll_without_cursor_overlay() {
        let appearance = Appearance::default();
        let mut copy =
            PreviewCopyMode::text(&Preview::Text("first\nsecond\nthird\nfourth".into()), 0, 2)
                .unwrap();
        copy.top = 2;
        copy.mode.cursor = CellPosition { x: 0, y: 2 };
        let Preview::Grid { columns, cells, .. } = copy.viewport(2, &appearance).unwrap() else {
            panic!("expected a grid");
        };
        let text: String = cells[..columns]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(text.trim_end(), "third");
        // The plain document background: no cursor cell is drawn.
        assert_eq!(cells[0].background, appearance.palette.background);
    }

    fn paint(ctx: &Context, copy: &mut PreviewCopyMode, events: &[Event]) -> egui::FullOutput {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            copy.input(ui.ctx(), events, 4);
            copy.paint(ui, &Appearance::default(), 4);
        });
        output.textures_delta.clear();
        output
    }
    fn colored_cells(output: &egui::FullOutput, color: egui::Color32) -> usize {
        fn count(shape: &egui::Shape, color: egui::Color32) -> usize {
            match shape {
                egui::Shape::Rect(rect) => usize::from(rect.fill == color),
                egui::Shape::Vec(shapes) => shapes.iter().map(|shape| count(shape, color)).sum(),
                _ => 0,
            }
        }
        output
            .shapes
            .iter()
            .map(|shape| count(&shape.shape, color))
            .sum()
    }

    #[test]
    fn single_character_visual_yank_is_visible_under_cursor_then_expires() {
        let ctx = Context::default();
        let palette = Appearance::default().palette;
        let mut copy = PreviewCopyMode::text(&Preview::Text("x".into()), 0, 4).unwrap();
        let before = paint(&ctx, &mut copy, &[key(Key::V)]);
        assert_eq!(
            colored_cells(&before, palette.yank_pulse_background(1.0)),
            0
        );
        let yanked = paint(&ctx, &mut copy, &[key(Key::Y)]);
        assert!(
            yanked
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("x".into()))
        );
        assert_eq!(
            colored_cells(&yanked, palette.yank_pulse_background(1.0)),
            1
        );
        // An idle frame keeps the visible feedback and schedules its removal.
        let idle = paint(&ctx, &mut copy, &[]);
        assert_eq!(colored_cells(&idle, palette.yank_pulse_background(1.0)), 1);
        assert!(
            idle.viewport_output[&egui::ViewportId::ROOT].repaint_delay <= YANK_HIGHLIGHT_DURATION
        );
        // It softens once just before it clears.
        let range = copy.mode.yank_highlight_range().unwrap();
        copy.mode.highlight_yank(
            range,
            Instant::now() - YANK_HIGHLIGHT_DURATION + YANK_HIGHLIGHT_DURATION / 10,
        );
        let fading = paint(&ctx, &mut copy, &[]);
        let faded = palette.yank_pulse_background(crate::copy_mode::YANK_PULSE[4]);
        assert_ne!(faded, palette.yank_pulse_background(1.0));
        assert_eq!(colored_cells(&fading, faded), 1);
        let range = copy.mode.yank_highlight_range().unwrap();
        copy.mode
            .highlight_yank(range, Instant::now() - YANK_HIGHLIGHT_DURATION);
        assert_eq!(
            colored_cells(
                &paint(&ctx, &mut copy, &[]),
                palette.yank_pulse_background(1.0)
            ),
            0
        );
    }

    #[test]
    fn rectangle_yank_paints_wide_glyph_and_only_selected_columns() {
        let ctx = Context::default();
        let mut copy = PreviewCopyMode::text(&Preview::Text("a猫b\n1234".into()), 0, 4).unwrap();
        copy.mode.cursor = CellPosition { x: 1, y: 0 };
        copy.mode.toggle_visual(VisualMode::Blockwise);
        copy.mode.cursor = CellPosition { x: 2, y: 1 };
        let output = paint(&ctx, &mut copy, &[key(Key::Y)]);
        assert!(
            output
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("猫\n23".into()))
        );
        assert_eq!(
            colored_cells(
                &output,
                Appearance::default().palette.yank_pulse_background(1.0)
            ),
            4
        );
    }

    #[test]
    fn linewise_selection_fills_viewport_without_copying_padding() {
        let ctx = Context::default();
        let appearance = Appearance::default();
        for visual in [
            VisualMode::Linewise,
            VisualMode::Characterwise,
            VisualMode::Blockwise,
        ] {
            let mut copy =
                PreviewCopyMode::text(&Preview::Text("one\ntwo\nthree".into()), 0, 4).unwrap();
            copy.mode.toggle_visual(visual);
            copy.mode.cursor = CellPosition { x: 0, y: 1 };
            let mut viewport_width = 0.0;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                viewport_width = ui.available_width();
                copy.paint(ui, &appearance, 4);
            });
            output.textures_delta.clear();
            let full_rows = output.shapes.iter().filter(|shape| matches!(
                &shape.shape,
                egui::Shape::Rect(rect) if rect.fill == appearance.palette.preview_selection_background
                    && (rect.rect.width() - viewport_width).abs() < 0.01
            )).count();
            assert_eq!(
                full_rows,
                match visual {
                    VisualMode::Linewise => 2,
                    VisualMode::Characterwise => 1,
                    VisualMode::Blockwise => 0,
                }
            );
            if visual == VisualMode::Linewise {
                let yanked = paint(&ctx, &mut copy, &[key(Key::Y)]);
                assert_eq!(
                    yanked
                        .shapes
                        .iter()
                        .filter(|shape| matches!(
                            &shape.shape,
                            egui::Shape::Rect(rect) if rect.fill == appearance.palette.yank_pulse_background(1.0)
                                && (rect.rect.width() - viewport_width).abs() < 0.01
                        ))
                        .count(),
                    2
                );
                assert!(
                    yanked
                        .platform_output
                        .commands
                        .contains(&egui::OutputCommand::CopyText("one\ntwo\n".into()))
                );
            }
        }
    }

    #[test]
    fn multiline_character_selection_extends_first_and_middle_rows_only() {
        let ctx = Context::default();
        let appearance = Appearance::default();
        let mut copy =
            PreviewCopyMode::text(&Preview::Text("one\ntwo\nthree".into()), 0, 4).unwrap();
        copy.mode.cursor = CellPosition { x: 1, y: 0 };
        copy.mode.toggle_visual(VisualMode::Characterwise);
        copy.mode.cursor = CellPosition { x: 1, y: 2 };
        let mut width = 0.0;
        let mut origin = egui::Pos2::ZERO;
        let mut height = 0.0;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            width = ui.available_width();
            origin = ui.cursor().min;
            height = appearance.typography.preview_row_height(ui.ctx());
            copy.paint(ui, &appearance, 4);
        });
        output.textures_delta.clear();
        let extensions: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect)
                    if rect.fill == appearance.palette.preview_selection_background
                        && rect.rect.width() > width / 2.0 =>
                {
                    Some(rect.rect)
                }
                _ => None,
            })
            .collect();
        assert_eq!(extensions.len(), 2);
        assert!((extensions[0].left() - origin.x - appearance.typography.cell_width).abs() < 0.01);
        assert!((extensions[0].right() - origin.x - width).abs() < 0.01);
        assert!((extensions[1].width() - width).abs() < 0.01);
        assert!((extensions[1].top() - origin.y - height).abs() < 0.01);
        let yanked = paint(&ctx, &mut copy, &[key(Key::Y)]);
        let yank_extensions: Vec<_> = yanked
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect)
                    if rect.fill == appearance.palette.yank_pulse_background(1.0)
                        && rect.rect.width() > width / 2.0 =>
                {
                    Some(rect.rect)
                }
                _ => None,
            })
            .collect();
        assert_eq!(yank_extensions, extensions);
        assert!(
            yanked
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("ne\ntwo\nth".into()))
        );
    }
}

/// The copy document behind a viewport, pinned to the metadata it was opened
/// with: `valid` compares it against the live document, and searches walk the
/// pinned row count. Reads used for painting treat errors as nothing to draw;
/// navigation goes through `handle` so `CopyController` records them.
#[derive(Clone)]
struct PinnedDocument {
    handle: std::sync::Arc<dyn crate::copy_document::CopyDocument>,
    metadata: crate::copy_document::DocumentMetadata,
}

impl PinnedDocument {
    fn columns(&self) -> u32 {
        self.metadata.columns
    }
    fn rows(&self) -> u64 {
        self.metadata.total_rows
    }
    fn row(&self, y: u64) -> Option<DocumentRow> {
        self.handle.text_row(y).ok().flatten()
    }
    fn resolve(&self, cell: CellPosition, right: bool) -> Option<CellPosition> {
        self.handle.resolve_cell(cell, right).ok()
    }
    fn selection_text(&self, selection: &CopySelection) -> Option<String> {
        self.handle.selection_text(selection).ok()
    }
    fn grid(&self, top: u64, rows: usize, appearance: &Appearance) -> Option<Preview> {
        self.handle.viewport(top, rows, appearance).ok()
    }
}

/// A line-number gutter entry; `current` marks the focused cursor's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GutterNumber {
    pub(crate) text: String,
    pub(crate) current: bool,
    /// Hybrid numbering left-aligns the cursor line's absolute number, as
    /// Vim does with `number` and `relativenumber`, so it stands apart from
    /// the right-aligned distances.
    pub(crate) left_aligned: bool,
}

pub(crate) struct PreviewCopyMode {
    pub(crate) mode: CopyMode,
    document: PinnedDocument,
    pub(crate) top: u64,
    left: u32,
    quick_select: Option<crate::quick_select::QuickSelect>,
    quick_origin: Option<(CopyMode, u64, u32)>,
    viewport_columns: u32,
    search_input: Option<(String, bool)>,
    search_regex: bool,
    search_error: Option<String>,
    search_origin: Option<(CopyMode, u64, u32)>,
    search_revision: u64,
    search_anchor: CellPosition,
    pending_search_highlights: Option<Vec<CellRange>>,
    yank_linewise: bool,
    search_job: Option<SearchJob>,
    search_service: Option<crate::search_service::SearchService>,
}

impl PreviewCopyMode {
    pub(crate) fn viewport(&self, rows: usize, appearance: &Appearance) -> Option<Preview> {
        let Preview::Grid {
            columns,
            cells,
            total_rows,
            lines,
        } = self.document.grid(self.top, rows, appearance)?
        else {
            return None;
        };
        let left = (self.left as usize).min(columns.saturating_sub(1));
        Some(Preview::Grid {
            columns: columns - left,
            cells: cells
                .chunks(columns)
                .flat_map(|row| row.iter().skip(left).cloned())
                .collect(),
            total_rows,
            lines,
        })
    }

    #[cfg(test)]
    pub(crate) fn first_quick_hint(&self) -> Option<(String, CellPosition)> {
        self.quick_select
            .as_ref()?
            .hints
            .first()
            .map(|hint| (hint.label.clone(), hint.range.start))
    }

    pub(crate) fn set_scroll_padding(&mut self, rows: u32) {
        self.mode.set_scroll_padding(rows);
        if let Some((mode, _, _)) = &mut self.search_origin {
            mode.set_scroll_padding(rows);
        }
        if let Some((mode, _, _)) = &mut self.quick_origin {
            mode.set_scroll_padding(rows);
        }
    }

    pub(crate) fn quick_select_active(&self) -> bool {
        self.quick_select.is_some()
    }

    /// Whether `/` or `?` search input is open; it reads Ctrl+N and Ctrl+P.
    pub(crate) fn search_input_active(&self) -> bool {
        self.search_input.is_some()
    }

    pub(crate) fn open_quick_select(&mut self, from_copy: bool, rows: usize) -> bool {
        self.hide_decorations();
        self.quick_origin = Some((self.mode.clone(), self.top, self.left));
        self.mode.clear_pending_for_navigation();
        self.rebuild_quick_select(from_copy, rows)
    }

    fn rebuild_quick_select(&mut self, from_copy: bool, rows: usize) -> bool {
        let view = crate::copy_mode::View {
            cursor: self.mode.cursor,
            columns: self.document.columns(),
            total_rows: self.document.rows(),
            viewport_top: self.top,
            viewport_rows: rows.max(1) as u32,
        };
        self.quick_select = crate::quick_select::QuickSelect::new(
            self.mode.generation.unwrap_or(0),
            from_copy,
            view,
            |row| self.document.row(row).map(|row| row.text),
        );
        if let Some(quick) = &mut self.quick_select {
            quick.hints.retain(|hint| {
                hint.range.start.x >= self.left
                    && hint.range.start.x < self.left.saturating_add(self.viewport_columns)
            });
            true
        } else {
            self.quick_origin = None;
            false
        }
    }

    /// Cancel returns whether the picker should return to results/query focus.
    pub(crate) fn cancel_quick_select(&mut self) -> bool {
        let Some(quick) = self.quick_select.take() else {
            return false;
        };
        if let Some((mode, top, left)) = self.quick_origin.take() {
            self.mode = mode;
            self.top = top;
            self.left = left;
        }
        !quick.from_copy
    }

    /// Zero-based logical line containing `row`.
    /// Rows in the laid-out document.
    pub(crate) fn total_rows(&self) -> u64 {
        self.document.rows()
    }

    fn line_of(&self, row: u64) -> Option<u64> {
        if row >= self.document.rows() {
            return None;
        }
        self.document.handle.line_number(row)
    }

    /// Logical lines in the document.
    pub(crate) fn line_count(&self) -> u64 {
        self.document
            .rows()
            .checked_sub(1)
            .and_then(|last| self.line_of(last))
            .map_or(0, |line| line + 1)
    }

    /// Gutter numbers for each of the `rows` viewport rows. Only a line's first
    /// row is numbered; continuations and rows past the end are None. Relative
    /// numbers follow the cursor only while `focused`.
    pub(crate) fn line_numbers(
        &self,
        rows: usize,
        style: crate::picker::PreviewLineNumbers,
        focused: bool,
    ) -> Vec<Option<GutterNumber>> {
        use crate::picker::PreviewLineNumbers as Style;
        let cursor = if focused {
            self.line_of(self.mode.cursor.y)
        } else {
            None
        };
        let mut previous = self.top.checked_sub(1).and_then(|y| self.line_of(y));
        let mut numbers = Vec::with_capacity(rows);
        for y in self.top..self.top.saturating_add(rows as u64) {
            let line = self.line_of(y);
            let starts = line != previous;
            previous = line;
            numbers.push(line.filter(|_| starts).map(|line| {
                let current = cursor == Some(line);
                let number = match (style, cursor) {
                    (Style::Relative, Some(cursor)) => line.abs_diff(cursor),
                    (Style::Hybrid, Some(cursor)) if !current => line.abs_diff(cursor),
                    _ => line + 1,
                };
                GutterNumber {
                    text: number.to_string(),
                    current,
                    left_aligned: current && style == Style::Hybrid,
                }
            }));
        }
        numbers
    }

    pub(crate) fn valid(&self) -> bool {
        let PinnedDocument { handle, metadata } = &self.document;
        handle.metadata().is_ok_and(|current| current == *metadata)
    }

    pub(crate) fn copy_all(&self, ctx: &Context) {
        if let Some(last_row) = self.document.rows().checked_sub(1) {
            if let Some(text) = self.document.selection_text(&CopySelection {
                range: CellRange {
                    start: CellPosition { x: 0, y: 0 },
                    end: CellPosition {
                        x: self.document.columns() - 1,
                        y: last_row,
                    },
                },
                linewise: false,
                trim: true,
                rectangle: false,
            }) {
                ctx.copy_text(text);
            }
        }
    }
    pub(crate) fn host(
        handle: std::sync::Arc<dyn crate::copy_document::CopyDocument>,
        top: usize,
        rows: usize,
    ) -> Option<Self> {
        let metadata = handle.metadata().ok()?;
        if metadata.columns == 0 || metadata.total_rows == 0 {
            return None;
        }
        let anchor = handle.anchor();
        let mut copy = Self::new(
            PinnedDocument { handle, metadata },
            metadata.revision,
            top,
            rows,
        );
        // An anchor the viewport has scrolled away from leaves the cursor at its top.
        let visible = copy.top..copy.top + rows.max(1) as u64;
        if let Some(anchor) = anchor.filter(|anchor| visible.contains(&anchor.y)) {
            let anchor = CellPosition {
                x: anchor.x.min(metadata.columns - 1),
                y: anchor.y,
            };
            copy.mode.cursor = anchor;
            copy.search_anchor = anchor;
        }
        Some(copy)
    }

    /// A viewport on text, paged as the preview worker pages `Preview::Text`.
    #[cfg(test)]
    pub(crate) fn text(preview: &Preview, top: usize, rows: usize) -> Option<Self> {
        let Preview::Text(text) = preview else {
            return None;
        };
        let layout = crate::styled_document::StyledDocument::from_text(text)
            .text_layout(preview::MAX_TEXT_COLUMNS, &crate::Cancellation::never())
            .ok()?;
        Self::host(layout, top, rows)
    }

    fn new(document: PinnedDocument, revision: u64, top: usize, rows: usize) -> Self {
        let top = (top as u64).min(document.rows().saturating_sub(rows as u64));
        let view = View {
            cursor: CellPosition { x: 0, y: top },
            viewport_top: top,
            columns: document.columns(),
            total_rows: document.rows(),
            viewport_rows: rows.max(1) as u32,
        };
        let mut mode = CopyMode::default();
        mode.start(revision, view);
        Self {
            mode,
            document,
            top,
            left: 0,
            quick_select: None,
            quick_origin: None,
            viewport_columns: view.columns,
            search_input: None,
            search_regex: false,
            search_error: None,
            search_origin: None,
            search_revision: 0,
            search_anchor: view.cursor,
            pending_search_highlights: None,
            yank_linewise: false,
            search_job: None,
            search_service: None,
        }
    }
    fn view(&self, rows: usize) -> View {
        View {
            cursor: self.mode.cursor,
            viewport_top: self.top,
            viewport_rows: rows.max(1) as u32,
            columns: self.document.columns(),
            total_rows: self.document.rows(),
        }
    }
    fn reveal(&mut self, rows: usize) {
        self.top = crate::copy_controller::revealed_viewport_top(
            self.mode.cursor,
            self.view(rows),
            self.mode.scroll_padding(),
        );
    }
    fn search(&mut self, ctx: &Context, pattern: &str, forward: bool, count: u32, regex: bool) {
        self.search_job = None;
        self.search_revision += 1;
        self.search_anchor = self
            .search_origin
            .as_ref()
            .map_or(self.mode.cursor, |(mode, _, _)| mode.cursor);
        self.search_error = crate::copy_mode::search_regex(pattern, regex)
            .err()
            .map(|error| error.to_string());
        if pattern.is_empty() || self.search_error.is_some() {
            self.pending_search_highlights = None;
            self.mode.adopt_search(self.search_revision, Vec::new());
            return;
        }
        if self.document.handle.search_synchronously() {
            self.mode.begin_search(self.search_revision, forward, count);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let result = search_matches(&self.document, pattern, regex, &cancel);
            self.finish_search_result(self.search_revision, result);
            return;
        }
        if self.pending_search_highlights.is_none() {
            self.pending_search_highlights = Some(self.mode.search_matches().to_vec());
        }
        self.mode.begin_search(self.search_revision, forward, count);
        let service = self.search_service.get_or_insert_with(|| {
            let ctx = ctx.clone();
            crate::search_service::SearchService::new(move || ctx.request_repaint())
        });
        match service.submit(
            self.mode.generation.unwrap_or(0),
            std::sync::Arc::new(self.document.clone()),
            pattern.to_owned(),
            regex,
        ) {
            Ok(ticket) => {
                self.search_revision = ticket.query_revision;
                self.mode.begin_search(self.search_revision, forward, count);
                self.search_job = Some(SearchJob {
                    revision: ticket.query_revision,
                    cancel: ticket.cancellation(),
                    _ticket: ticket,
                });
            }
            Err(error) => self.finish_search_result(self.search_revision, Err(error)),
        }
    }
    fn finish_search_result(&mut self, revision: u64, result: Result<Vec<CellRange>, String>) {
        if !self.mode.search_accepts_result(revision) {
            return;
        }
        match result {
            Ok(matches) => self.finish_preview_search(revision, matches),
            Err(error) => {
                self.search_error = Some(error);
                self.finish_preview_search(revision, Vec::new());
            }
        }
    }
    fn finish_preview_search(&mut self, revision: u64, matches: Vec<CellRange>) {
        if !self.mode.search_accepts_result(revision) {
            return;
        }
        self.pending_search_highlights = None;
        let view = self.view(1); // The next paint/input reveals using its actual height.
        match crate::copy_controller::CopyController::new(
            &mut self.mode,
            &*self.document.handle,
            view,
        )
        .finish_search(revision, matches, Some(self.search_anchor))
        {
            Ok(_) => {}
            Err(error) => self.search_error = Some(error),
        }
    }
    fn poll_search(&mut self) {
        let Some(job) = &self.search_job else {
            return;
        };
        let revision = job.revision;
        let Some(service) = &mut self.search_service else {
            return;
        };
        match service.take_latest_result() {
            Ok(Some(result))
                if result.query_revision == revision
                    && Some(result.document_revision) == self.mode.generation =>
            {
                self.search_job = None;
                self.finish_search_result(revision, result.error.map_or(Ok(result.matches), Err));
            }
            Err(error) => {
                self.search_job = None;
                self.finish_search_result(revision, Err(error));
            }
            _ => {}
        }
    }

    /// Returns true when copy-mode focus should return to the query.
    pub(crate) fn input(&mut self, ctx: &Context, events: &[Event], rows: usize) -> bool {
        if !self.valid() {
            return true;
        }
        if self.quick_select.is_some() {
            for index in 0..events.len() {
                let action = self.quick_select.as_mut().unwrap().input(events, index);
                match action {
                    crate::quick_select::InputAction::Cancel => return self.cancel_quick_select(),
                    crate::quick_select::InputAction::Page(down) => {
                        let quick = self.quick_select.as_ref().unwrap();
                        let from_copy = quick.from_copy;
                        let purpose = quick.purpose();
                        self.top = crate::quick_select::QuickSelect::paged_view(quick.view, down)
                            .viewport_top;
                        if !self.rebuild_quick_select(from_copy, rows) {
                            return !from_copy;
                        }
                        self.quick_select.as_mut().unwrap().set_purpose(purpose);
                    }
                    crate::quick_select::InputAction::Select { cell, .. } => {
                        let quick = self.quick_select.take().unwrap();
                        self.quick_origin = None;
                        let view = self.view(rows);
                        let update = match crate::copy_controller::CopyController::new(
                            &mut self.mode,
                            &*self.document.handle,
                            view,
                        )
                        .select_quick(quick.purpose(), cell)
                        {
                            Ok(update) => update,
                            Err(error) => {
                                self.search_error = Some(error);
                                return true;
                            }
                        };
                        if let Some(top) = update.viewport_top {
                            self.top = top;
                        }
                        if let Some(delay) = update.repaint_after {
                            ctx.request_repaint_after(delay);
                        }
                        for effect in update.effects {
                            if let crate::copy_controller::CopyEffect::CopyText {
                                text,
                                selection,
                            } = effect
                            {
                                self.yank_linewise = selection.linewise;
                                ctx.copy_text(text);
                            }
                        }
                        ctx.request_repaint();
                        return false;
                    }
                    _ => {}
                }
            }
            ctx.request_repaint();
            return false;
        }
        self.poll_search();
        self.mode.begin_input_batch();
        let mut search_trigger = false;
        let mut copied = false;
        for event in events {
            if let Some((query, forward)) = &mut self.search_input {
                if search_trigger
                    && matches!(event, Event::Text(text) if text == "/" || text == "?")
                {
                    search_trigger = false;
                    continue;
                }
                search_trigger = false;
                let mut submit = false;
                let mut cancel = false;
                let mut changed = false;
                let mut navigate = None;
                match event {
                    Event::Key {
                        key: Key::R,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.ctrl => {
                        self.search_regex = !self.search_regex;
                        changed = true;
                    }
                    Event::Key {
                        key: key @ (Key::N | Key::P),
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.ctrl => {
                        navigate = Some(*key == Key::P);
                    }
                    Event::Text(text) | Event::Paste(text) => {
                        query.push_str(text);
                        changed = true;
                    }
                    Event::Key {
                        key: Key::Backspace,
                        pressed: true,
                        ..
                    } => {
                        if let Some((start, _)) = query.grapheme_indices(true).next_back() {
                            query.truncate(start);
                            changed = true;
                        }
                    }
                    Event::Key {
                        key: Key::Escape,
                        pressed: true,
                        ..
                    } => cancel = true,
                    Event::Key {
                        key: Key::Enter,
                        pressed: true,
                        ..
                    } => submit = true,
                    _ => {}
                }
                if let Some(reverse) = navigate {
                    if let Some(target) = self.mode.navigate_search(reverse, 1) {
                        if let Some(target) = self.document.resolve(target, false) {
                            self.mode.moved(Motion::CharacterJump, target);
                        }
                    }
                }
                if submit {
                    self.search_input = None;
                    self.search_origin = None;
                    self.reveal(rows);
                } else if cancel {
                    self.search_input = None;
                    self.search_job = None;
                    self.pending_search_highlights = None;
                    if let Some((mode, top, left)) = self.search_origin.take() {
                        self.mode = mode;
                        self.top = top;
                        self.left = left;
                    }
                } else if changed {
                    let query = query.clone();
                    let forward = *forward;
                    // Host-backed searches complete asynchronously. Keep the
                    // displayed match until the replacement result arrives.
                    if query.is_empty() || self.document.handle.search_synchronously() {
                        if let Some((mode, top, left)) = &self.search_origin {
                            self.mode = mode.clone();
                            self.top = *top;
                            self.left = *left;
                        }
                    }
                    self.search(ctx, &query, forward, 1, self.search_regex);
                    self.reveal(rows);
                }
                self.reveal(rows);
                continue;
            }
            // Egui can emit both Ctrl+C and Copy in one batch. Only Copy yanks.
            let copy = matches!(event, Event::Copy)
                || matches!(event,
                Event::Key { key: Key::C, pressed: true, modifiers, .. } if modifiers.ctrl);
            let action = if matches!(event, Event::Key { key: Key::PageUp | Key::PageDown,
                pressed: true, modifiers, .. } if *modifiers == Modifiers::CTRL)
            {
                CopyAction::Motion {
                    motion: if matches!(
                        event,
                        Event::Key {
                            key: Key::PageDown,
                            ..
                        }
                    ) {
                        Motion::PageDown
                    } else {
                        Motion::PageUp
                    },
                    count: 1,
                    explicit_count: false,
                }
            } else if copy && !copied {
                copied = true;
                if self.mode.visual_anchor.is_some() {
                    CopyAction::YankVisual
                } else {
                    CopyAction::Yank(YankCommand::Lines(1))
                }
            } else {
                self.mode.input(event)
            };
            let view = self.view(rows);
            let update = match crate::copy_controller::CopyController::new(
                &mut self.mode,
                &*self.document.handle,
                view,
            )
            .execute(action)
            {
                Ok(update) => update,
                Err(error) => {
                    self.search_error = Some(error);
                    ctx.request_repaint();
                    return true;
                }
            };
            if let Some(top) = update.viewport_top {
                self.top = top;
            }
            if let Some(delay) = update.repaint_after {
                ctx.request_repaint_after(delay);
            }
            for effect in update.effects {
                use crate::copy_controller::CopyEffect;
                match effect {
                    CopyEffect::BeginSearch { forward } => {
                        self.search_job = None;
                        self.pending_search_highlights = None;
                        self.mode.cancel_pending_search();
                        self.search_origin = Some((self.mode.clone(), self.top, self.left));
                        self.search_regex = false;
                        self.search_error = None;
                        self.search_input = Some((String::new(), forward));
                        search_trigger = matches!(event, Event::Key { .. });
                    }
                    CopyEffect::Exit => return true,
                    CopyEffect::CopyText { text, selection } => {
                        self.yank_linewise = selection.linewise;
                        ctx.copy_text(text);
                    }
                    CopyEffect::Search {
                        pattern,
                        forward,
                        count,
                        regex,
                    } => {
                        self.search(ctx, &pattern, forward, count, regex);
                        self.reveal(rows);
                    }
                    CopyEffect::QuickSelect { origin } => {
                        if self.open_quick_select(true, rows) {
                            self.quick_select
                                .as_mut()
                                .unwrap()
                                .set_purpose(crate::quick_select::Purpose::YankLines { origin });
                        }
                        return false;
                    }
                    CopyEffect::OpenLineSplit { .. } => {}
                    CopyEffect::ClearSearch { cancel_pending } => {
                        if cancel_pending {
                            self.search_job = None;
                        }
                        self.pending_search_highlights = None;
                    }
                }
            }
        }
        if !events.is_empty() {
            ctx.request_repaint();
        }
        false
    }

    pub(crate) fn hide_decorations(&mut self) {
        self.quick_select = None;
        self.quick_origin = None;
        self.mode.cancel_pending_search();
        self.search_job = None;
        self.search_input = None;
        self.search_origin = None;
        self.pending_search_highlights = None;
    }

    pub(crate) fn scroll(&mut self, delta: isize, rows: usize) {
        let view = self.view(rows);
        match crate::copy_controller::CopyController::new(
            &mut self.mode,
            &*self.document.handle,
            view,
        )
        .scroll(delta)
        {
            Ok(update) => {
                if let Some(top) = update.viewport_top {
                    self.top = top;
                }
            }
            Err(error) => self.search_error = Some(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn paint(&mut self, ui: &mut egui::Ui, appearance: &Appearance, rows: usize) {
        self.paint_layers(ui, appearance, rows, true);
    }

    pub(crate) fn paint_layers(
        &mut self,
        ui: &mut egui::Ui,
        appearance: &Appearance,
        rows: usize,
        decorations: bool,
    ) {
        let previous_columns = self.viewport_columns;
        self.viewport_columns = (ui.available_width() / appearance.typography.cell_width.max(1.0))
            .floor()
            .max(1.0) as u32;
        if !decorations {
            if let Some(document) = self.viewport(rows, appearance) {
                preview::paint(ui, &document, appearance, rows);
            }
            return;
        }
        self.poll_search();
        if self.quick_select.is_none() {
            self.reveal(rows);
        }
        let width = (ui.available_width() / appearance.typography.cell_width.max(1.0))
            .floor()
            .max(1.0) as u32;
        if self.quick_select.is_none() && self.mode.cursor.x < self.left {
            self.left = self.mode.cursor.x;
        }
        if self.quick_select.is_none() && self.mode.cursor.x >= self.left + width {
            self.left = self.mode.cursor.x + 1 - width;
        }
        self.mode.clear_expired_yank_highlight(Instant::now());
        let mut yank_background = appearance.palette.yank_pulse_background(1.0);
        if let Some(remaining) = self.mode.yank_highlight_remaining(Instant::now()) {
            let (step, next) = yank_pulse_timing(remaining);
            yank_background = appearance
                .palette
                .yank_pulse_background(yank_pulse_strength(step));
            ui.ctx().request_repaint_after(next);
        }
        let Some(Preview::Grid {
            columns,
            mut cells,
            total_rows,
            ..
        }) = self.document.grid(self.top, rows, appearance)
        else {
            return;
        };
        if let Some(quick) = &self.quick_select {
            if quick.view.viewport_rows != rows.max(1) as u32
                || previous_columns != self.viewport_columns
            {
                let from_copy = quick.from_copy;
                let purpose = quick.purpose();
                if self.rebuild_quick_select(from_copy, rows) {
                    self.quick_select.as_mut().unwrap().set_purpose(purpose);
                }
            }
        }
        if let Some(quick) = &self.quick_select {
            // Linewise yanks start from the origin row; mark it with a
            // cursorline bar.
            if let Some(y) = quick.origin_row().and_then(|y| y.checked_sub(self.top)) {
                let (background, text) = appearance.palette.cursor_line_colors();
                let start = y as usize * columns;
                for cell in cells.iter_mut().skip(start).take(columns) {
                    cell.inverse = false;
                    cell.background = background;
                    cell.foreground = text;
                }
            }
            for hint in quick.cell_text() {
                if let Some(y) = hint.cell.y.checked_sub(self.top) {
                    if let Some(cell) = cells.get_mut(y as usize * columns + hint.cell.x as usize) {
                        cell.text = char::from_u32(hint.codepoint).unwrap_or(' ').to_string();
                        if cell.inverse {
                            // Preserve the displayed background before styling the label.
                            std::mem::swap(&mut cell.foreground, &mut cell.background);
                        }
                        // Match Ghostty's Gruvbox yellow for Quick Select labels.
                        cell.foreground = egui::Color32::from_rgb(0xfa, 0xbd, 0x2f);
                        cell.bold = true;
                        cell.underline = true;
                        cell.underline_color = cell.foreground;
                        cell.italic = false;
                        cell.strikethrough = false;
                        cell.invisible = false;
                        cell.faint = false;
                        cell.inverse = false;
                    }
                }
            }
            let left = self.left as usize;
            let visible = columns.saturating_sub(left).min(width as usize).max(1);
            let cells = cells
                .chunks(columns)
                .flat_map(|row| row.iter().skip(left).take(visible).cloned())
                .collect();
            let status = format!("QUICK SELECT  {} · Esc", quick.status_text());
            let status_rect = egui::Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(
                    ui.available_width(),
                    rows as f32 * appearance.typography.preview_row_height(ui.ctx()),
                ),
            );
            preview::paint(
                ui,
                &Preview::Grid {
                    columns: visible,
                    cells,
                    total_rows,
                    lines: None,
                },
                appearance,
                rows,
            );
            let galley = ui.painter().layout_no_wrap(
                status,
                appearance.typography.normal.clone(),
                appearance.palette.accent,
            );
            let pos = status_rect.right_bottom() - galley.size() - egui::vec2(3.0, 1.0);
            let painter = ui
                .painter()
                .with_clip_rect(status_rect.intersect(ui.clip_rect()));
            painter.rect_filled(
                egui::Rect::from_min_size(pos, galley.size()),
                0,
                appearance.palette.background,
            );
            painter.galley(pos, galley, appearance.palette.accent);
            return;
        }
        let searching = self.search_input.is_some();
        let displayed_matches = self
            .pending_search_highlights
            .as_deref()
            .unwrap_or_else(|| self.mode.search_matches());
        let active_match = searching
            .then(|| {
                displayed_matches
                    .iter()
                    .find(|range| {
                        (range.start.y, range.start.x) <= (self.mode.cursor.y, self.mode.cursor.x)
                            && (self.mode.cursor.y, self.mode.cursor.x)
                                <= (range.end.y, range.end.x)
                    })
                    .copied()
            })
            .flatten();
        let selection = self.mode.visual_selection_to(self.mode.cursor, |y| {
            self.document.row(y).map(|row| row.text)
        });
        let selected = selection.as_ref().map(|s| (s.range, s.rectangle));
        let yank = self
            .mode
            .yank_highlight_range()
            .map(|range| (range, self.mode.yank_highlight_is_rectangle()));
        let yank_selection = yank.map(|(range, rectangle)| CopySelection {
            range,
            rectangle,
            linewise: self.yank_linewise,
            trim: false,
        });
        // Extend linewise rows and multiline character selections through
        // unused viewport space without extending the extracted text.
        for (selection, color) in selection
            .as_ref()
            .map(|selection| (selection, appearance.palette.preview_selection_background))
            .into_iter()
            .chain(
                yank_selection
                    .as_ref()
                    .map(|selection| (selection, yank_background)),
            )
            .filter(|(selection, _)| {
                selection.linewise
                    || (!selection.rectangle && selection.range.start.y != selection.range.end.y)
            })
        {
            let origin = ui.cursor().min;
            let row_height = appearance.typography.preview_row_height(ui.ctx());
            for y in 0..cells.len() / columns {
                let absolute = self.top + y as u64;
                if absolute >= selection.range.start.y && absolute <= selection.range.end.y {
                    // The final characterwise row ends at its selected cell;
                    // the cell painter already paints that bounded range.
                    if !selection.linewise && absolute == selection.range.end.y {
                        continue;
                    }
                    let left = if !selection.linewise && absolute == selection.range.start.y {
                        selection.range.start.x.saturating_sub(self.left) as f32
                            * appearance.typography.cell_width.max(1.0)
                    } else {
                        0.0
                    }
                    .min(ui.available_width());
                    let rect = egui::Rect::from_min_size(
                        origin + egui::vec2(left, y as f32 * row_height),
                        egui::vec2(ui.available_width() - left, row_height),
                    );
                    ui.painter().rect_filled(rect, 0.0, color);
                }
            }
        }
        let contains = |range: CellRange, rectangle: bool, x: u32, y: u64| {
            if rectangle {
                y >= range.start.y && y <= range.end.y && x >= range.start.x && x <= range.end.x
            } else {
                (y, x) >= (range.start.y, range.start.x) && (y, x) <= (range.end.y, range.end.x)
            }
        };
        for (y, row) in cells.chunks_mut(columns).enumerate() {
            let absolute = self.top + y as u64;
            // Preserve full graphemes for painting, including clusters longer
            // than the compatibility viewport's fixed codepoint buffer.
            let source = self.document.row(absolute);
            if let Some(source) = &source {
                for cell in row.iter_mut() {
                    cell.text.clear();
                }
                for (byte, text) in source.text.text.grapheme_indices(true) {
                    if let Some(cell) = row.get_mut(source.text.columns[byte] as usize) {
                        cell.text = text.into();
                    }
                }
            }
            for (x, cell) in row.iter_mut().enumerate() {
                let mut glyph_x = x;
                if let Some(source) = &source {
                    while glyph_x > 0 && source.widths.get(glyph_x) == Some(&0) {
                        glyph_x -= 1;
                    }
                }
                if cell.inverse {
                    std::mem::swap(&mut cell.foreground, &mut cell.background);
                    cell.inverse = false;
                }
                let syntax_foreground = cell.foreground;
                if cell.background == appearance.palette.background {
                    cell.background = egui::Color32::TRANSPARENT;
                }
                if self
                    .pending_search_highlights
                    .as_deref()
                    .unwrap_or_else(|| self.mode.search_matches())
                    .iter()
                    .any(|range| contains(*range, false, glyph_x as u32, absolute))
                {
                    cell.background = appearance
                        .palette
                        .search_match_background
                        .unwrap_or_else(|| appearance.palette.match_highlight.gamma_multiply(0.35));
                    if let Some(text) = appearance.palette.search_match_text {
                        cell.foreground = text;
                    }
                }
                if selected.is_some_and(|(range, rectangle)| {
                    contains(range, rectangle, glyph_x as u32, absolute)
                }) {
                    cell.background = appearance.palette.preview_selection_background;
                    cell.foreground = syntax_foreground;
                }
                let yanked = yank.is_some_and(|(range, rectangle)| {
                    contains(range, rectangle, glyph_x as u32, absolute)
                });
                if yanked {
                    cell.background = yank_background;
                    cell.foreground = syntax_foreground;
                }
                if active_match
                    .is_some_and(|range| contains(range, false, glyph_x as u32, absolute))
                {
                    cell.background = appearance
                        .palette
                        .search_active_background
                        .unwrap_or(appearance.palette.match_highlight);
                    cell.foreground = appearance
                        .palette
                        .search_active_text
                        .unwrap_or(appearance.palette.background);
                }
                if !searching
                    && self.mode.cursor
                        == (CellPosition {
                            x: glyph_x as u32,
                            y: absolute,
                        })
                    && !yanked
                {
                    cell.background = appearance
                        .palette
                        .copy_cursor_background
                        .unwrap_or(appearance.palette.accent);
                    cell.foreground = appearance.palette.background;
                }
            }
        }
        let left = self.left as usize;
        let visible = (columns.saturating_sub(left)).min(width as usize).max(1);
        let cells = cells
            .chunks(columns)
            .flat_map(|row| row.iter().skip(left).take(visible).cloned())
            .collect();
        let start = ui.cursor().min;
        preview::paint(
            ui,
            &Preview::Grid {
                columns: visible,
                cells,
                total_rows,
                lines: None,
            },
            appearance,
            rows,
        );
        let rect = egui::Rect::from_min_size(
            start,
            egui::vec2(
                ui.available_width(),
                rows as f32 * appearance.typography.preview_row_height(ui.ctx()),
            ),
        );
        let Some((query, forward)) = &self.search_input else {
            return;
        };
        let mode = if self.search_regex {
            "REGEX"
        } else {
            "LITERAL"
        };
        let status = if let Some(error) = &self.search_error {
            format!(
                "{}{query}  {mode} · {error}",
                if *forward { '/' } else { '?' }
            )
        } else {
            format!("{}{query}  {mode}", if *forward { '/' } else { '?' })
        };
        let galley = ui.painter().layout_no_wrap(
            status,
            appearance.typography.normal.clone(),
            appearance.palette.accent,
        );
        let pos = egui::pos2(
            rect.right() - galley.size().x - 3.0,
            rect.bottom() - galley.size().y - 1.0,
        );
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        painter.rect_filled(
            egui::Rect::from_min_size(pos, galley.size()),
            0,
            appearance.palette.background,
        );
        painter.galley(pos, galley, appearance.palette.accent);
    }
}
struct SearchJob {
    revision: u64,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _ticket: crate::search_service::SearchTicket,
}
impl Drop for SearchJob {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
impl crate::search_service::SearchDocument for PinnedDocument {
    fn visit_text(
        &self,
        visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        for y in 0..self.rows() {
            if cancelled() {
                return Err("Search cancelled".into());
            }
            let row = self.row(y).ok_or("Search document row is unavailable")?;
            let positions: Vec<_> = row
                .text
                .columns
                .iter()
                .map(|&x| CellPosition { x, y })
                .collect();
            visitor(
                row.text.text.as_bytes(),
                &positions,
                !row.text.wraps_to_next,
            )?;
        }
        Ok(())
    }
}

fn search_matches(
    document: &PinnedDocument,
    pattern: &str,
    regex: bool,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<CellRange>, String> {
    crate::search_service::search_document(document, pattern, regex, Default::default(), &|| {
        cancel.load(std::sync::atomic::Ordering::Relaxed)
    })
}

#[cfg(test)]
mod line_number_tests {
    use super::*;
    use crate::picker::PreviewLineNumbers as Style;
    use crate::styled_document::{DocumentLayout, StyledDocument};
    use std::sync::Arc;

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn texts(numbers: &[Option<GutterNumber>]) -> Vec<Option<&str>> {
        numbers
            .iter()
            .map(|number| number.as_ref().map(|number| number.text.as_str()))
            .collect()
    }

    /// Rows at four columns: one | abcd efgh ij | thre e | four
    fn layout() -> Arc<DocumentLayout> {
        StyledDocument::from_ansi(b"one\nabcdefghij\nthree\nfour", 1 << 20, 100)
            .unwrap()
            .layout(4, &crate::Cancellation::test())
            .unwrap()
    }

    #[test]
    fn numbers_logical_lines_and_relative_distances_match_line_motions() {
        let ctx = Context::default();
        let mut copy = PreviewCopyMode::host(layout(), 0, 7).unwrap();
        assert_eq!(copy.line_count(), 4);
        let absolute = copy.line_numbers(7, Style::Absolute, false);
        assert_eq!(
            texts(&absolute),
            [Some("1"), Some("2"), None, None, Some("3"), None, Some("4")]
        );
        assert!(absolute.iter().flatten().all(|number| !number.current));
        // Unfocused previews have no visible cursor to measure from.
        assert_eq!(copy.line_numbers(7, Style::Relative, false), absolute);
        assert_eq!(copy.line_numbers(7, Style::Hybrid, false), absolute);

        assert_eq!(copy.mode.cursor, CellPosition { x: 0, y: 0 });
        let relative = copy.line_numbers(7, Style::Relative, true);
        assert_eq!(
            texts(&relative),
            [Some("0"), Some("1"), None, None, Some("2"), None, Some("3")]
        );
        assert!(relative[0].as_ref().unwrap().current);
        // The relative number is the count that reaches the line.
        copy.input(&ctx, &[key(Key::Num2), key(Key::J)], 7);
        assert_eq!(copy.mode.cursor.y, 4);
        assert_eq!(copy.top, 0);
        let hybrid = copy.line_numbers(7, Style::Hybrid, true);
        assert_eq!(
            texts(&hybrid),
            [Some("2"), Some("1"), None, None, Some("3"), None, Some("1")]
        );
        let current: Vec<bool> = hybrid
            .iter()
            .map(|number| number.as_ref().is_some_and(|number| number.current))
            .collect();
        assert_eq!(current, [false, false, false, false, true, false, false]);
        // Only hybrid's absolute cursor-line number is left-aligned.
        let left: Vec<bool> = hybrid
            .iter()
            .map(|number| number.as_ref().is_some_and(|number| number.left_aligned))
            .collect();
        assert_eq!(left, current);
        for style in [Style::Absolute, Style::Relative] {
            assert!(
                copy.line_numbers(7, style, true)
                    .iter()
                    .flatten()
                    .all(|number| !number.left_aligned)
            );
        }
        // A focused absolute gutter still marks the cursor line.
        assert!(
            copy.line_numbers(7, Style::Absolute, true)[4]
                .as_ref()
                .unwrap()
                .current
        );
    }

    #[test]
    fn viewport_starting_inside_a_wrapped_line_and_past_the_end_is_unnumbered() {
        let mut copy = PreviewCopyMode::host(layout(), 0, 7).unwrap();
        copy.top = 2;
        assert_eq!(
            texts(&copy.line_numbers(7, Style::Absolute, false)),
            [None, None, Some("3"), None, Some("4"), None, None]
        );
    }

    #[test]
    fn text_previews_number_every_row() {
        let preview = Preview::Text("a\nb\nc".into());
        let copy = PreviewCopyMode::text(&preview, 1, 2).unwrap();
        assert_eq!(copy.line_count(), 3);
        assert_eq!(
            texts(&copy.line_numbers(2, Style::Absolute, false)),
            [Some("2"), Some("3")]
        );
    }
}
