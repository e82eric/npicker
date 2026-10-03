//! The example host owns its window/GL context and publishes a live candidate stream.
use nfm_egui::{
    Appearance, Cancellation, Picker, PickerConfig, PickerEvent, Placement, Position, Preview,
    PreviewRequest, egui,
};
use nfm_search_core::{
    snapshot_store::SnapshotStore,
    store::{PublishingStreamingItemStoreWithPayload, StreamingItemSnapshotWithPayload},
};
use std::{
    io::BufRead,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

type Source = StreamingItemSnapshotWithPayload<u64>;
struct Demo {
    source: Arc<SnapshotStore<Source>>,
    cancelled: Arc<AtomicBool>,
    picker: Option<Picker<Source>>,
    appearance: Appearance,
    position: Position,
    status: String,
    screenshot: Option<PathBuf>,
    screenshot_requested: bool,
}
impl Demo {
    fn new(ctx: &egui::Context) -> Self {
        let source = Arc::new(SnapshotStore::new_append_only());
        let mut writer = PublishingStreamingItemStoreWithPayload::new(source.clone());
        writer.publish();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let stdin = std::env::args().any(|arg| arg == "--stdin");
        std::thread::spawn(move || {
            if stdin {
                for (i, line) in std::io::stdin().lock().lines().enumerate() {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(line) = line else {
                        break;
                    };
                    writer.add_item(line.as_bytes(), i as u64);
                    // Publish immediately: do not wait for 1000 lines or EOF.
                    writer.publish();
                }
            } else {
                for batch in 0..50 {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    for i in batch * 100..(batch + 1) * 100 {
                        writer.add_item(format!("Candidate {i:04} — café 猫").as_bytes(), i);
                    }
                    writer.publish();
                    std::thread::sleep(Duration::from_millis(80));
                }
            }
            writer.complete();
        });
        let mut args = std::env::args();
        let screenshot = args
            .find(|arg| arg == "--screenshot")
            .and_then(|_| args.next())
            .map(PathBuf::from);
        let mut demo = Self {
            source,
            cancelled,
            picker: None,
            appearance: Appearance::default(),
            position: Position::Center,
            status: "Live stream: search or navigate while candidates arrive.".into(),
            screenshot,
            screenshot_requested: false,
        };
        demo.open(ctx);
        demo
    }
    fn open(&mut self, ctx: &egui::Context) {
        let mut picker = Picker::new(
            "demo-picker",
            ctx,
            self.source.clone(),
            PickerConfig::default(),
        );
        picker.set_preview_provider(
            ctx,
            Arc::new(|request: PreviewRequest<u64>, cancel: Cancellation| {
                if cancel.is_cancelled() {
                    return Ok(Preview::Empty);
                }
                {
                    let text = format!(
                        "\x1b[1;33m{}\x1b[0m\r\n\x1b[32mPayload: {}\x1b[0m\r\n{}",
                        request.selection.text,
                        request.selection.item,
                        (0..50)
                            .map(|i| format!("Preview row {i}\r\n"))
                            .collect::<String>()
                    );
                    nfm_egui::preview::parse_ansi(text.as_bytes(), &request)
                }
            }),
        );
        self.picker = Some(picker);
    }
}
impl Drop for Demo {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let capture = ui.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = capture
            && let Some(path) = &self.screenshot
        {
            let bytes: Vec<u8> = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            image::save_buffer(
                path,
                &bytes,
                image.width() as u32,
                image.height() as u32,
                image::ColorType::Rgba8,
            )
            .expect("write demo screenshot");
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.horizontal(|ui| {
            if ui.button("Open picker").clicked() {
                self.open(ui.ctx());
            }
            ui.label("Placement:");
            for (position, label) in [
                (Position::Top, "Top"),
                (Position::Center, "Center"),
                (Position::Bottom, "Bottom"),
                (Position::Cursor, "Anchor"),
            ] {
                ui.selectable_value(&mut self.position, position, label);
            }
        });
        ui.label(&self.status);
        ui.label("↑/↓ navigate · Enter accepts · Esc cancels · Ctrl+C copies · Ctrl+P toggles preview · Ctrl+PageUp/Down scrolls preview");
        let ctx = ui.ctx();
        let mut bounds = ctx.content_rect();
        bounds.min.y += 90.0;
        let mut placement = Placement::new(bounds);
        placement.position = self.position;
        placement.anchor = Some(egui::pos2(bounds.center().x, bounds.bottom() - 80.0));
        if let Some(picker) = &mut self.picker {
            let output = picker.show(ctx, placement, &self.appearance, false);
            match output.event {
                Some(PickerEvent::Accepted(selection)) => {
                    self.status = format!("Accepted payload {}: {}", selection.item, selection.text)
                }
                Some(PickerEvent::Cancelled) => self.status = "Cancelled".into(),
                Some(PickerEvent::CopyRequested(selection)) => {
                    ctx.copy_text(selection.text);
                    picker.notify_copied(ctx);
                }
                Some(PickerEvent::CustomAction { .. }) | None => {}
            }
            if !output.busy
                && self.source.is_done()
                && self.screenshot.is_some()
                && !self.screenshot_requested
            {
                self.screenshot_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            if picker.is_closed() {
                self.picker = None;
            }
        }
    }
}
fn main() -> eframe::Result {
    eframe::run_native(
        "NFM embedded control",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 850.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(Demo::new(&cc.egui_ctx)))),
    )
}
