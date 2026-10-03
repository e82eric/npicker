//! Process picker for host-owned egui windows. No legacy picker runtime required.
#![cfg(windows)]
use nfm_egui::{
    Appearance, ItemsSource, Picker, PickerConfig, PickerEvent, Placement, Preview,
    SearchSnapshotProvider, egui,
};
use nfm_search_core::snapshot_store::SnapshotStore;
use nfm_win32::{
    ProcessPickerItem, ProcessPickerSnapshot,
    list_processes::{ProcessInfo, list_processes, terminate_process},
};
use std::sync::{Arc, mpsc};
/// Adapt process snapshots to the item type of a host's retained picker.
pub trait ProcessSourceAdapter<S: ItemsSource>: Send + Sync + 'static {
    fn source(
        &self,
        store: Arc<SnapshotStore<ProcessPickerSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<S>>;
    fn process<'a>(&self, item: &'a S::Item) -> Option<&'a ProcessPickerItem>;
}
/// Direct process items for standalone embedding.
pub struct ProcessItems;
impl ProcessSourceAdapter<ProcessPickerSnapshot> for ProcessItems {
    fn source(
        &self,
        store: Arc<SnapshotStore<ProcessPickerSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<ProcessPickerSnapshot>> {
        store
    }
    fn process<'a>(&self, item: &'a ProcessPickerItem) -> Option<&'a ProcessPickerItem> {
        Some(item)
    }
}
pub struct ProcessPicker<S: ItemsSource + Send + Sync + 'static = ProcessPickerSnapshot> {
    picker: Picker<S>,
    store: Arc<SnapshotStore<ProcessPickerSnapshot>>,
    pending: Option<mpsc::Receiver<Result<Vec<ProcessInfo>, String>>>,
    pending_kill: Option<u32>,
    status: Option<(String, bool, std::time::Instant)>,
    adapter: Arc<dyn ProcessSourceAdapter<S>>,
}
impl<S: ItemsSource + Send + Sync + 'static> ProcessPicker<S>
where
    S::Item: Send + 'static,
{
    pub fn picker(&self) -> &Picker<S> {
        &self.picker
    }
    pub fn picker_mut(&mut self) -> &mut Picker<S> {
        &mut self.picker
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn set_rows(&mut self, result_rows: usize, preview_rows: usize) {
        let visible = self.picker.preview_visible();
        self.picker
            .set_presentation(result_rows, preview_rows, visible);
    }
    pub fn into_picker(self) -> Picker<S> {
        self.picker
    }
    pub fn with_picker(
        ctx: &egui::Context,
        config: PickerConfig,
        adapter: Arc<dyn ProcessSourceAdapter<S>>,
        previous: Option<Picker<S>>,
    ) -> Self {
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(ProcessPickerSnapshot::from_items(&[])));
        let mut menu = Self::from_store_with_picker(ctx, store, config, adapter, previous);
        menu.refresh(ctx, None);
        menu
    }
    fn from_store_with_picker(
        ctx: &egui::Context,
        store: Arc<SnapshotStore<ProcessPickerSnapshot>>,
        mut config: PickerConfig,
        adapter: Arc<dyn ProcessSourceAdapter<S>>,
        previous: Option<Picker<S>>,
    ) -> Self {
        config.title = "Processes".into();
        let source = adapter.source(store.clone());
        let mut picker = if let Some(mut picker) = previous {
            picker.replace_source(ctx, source, config.initial_query);
            picker.set_presentation(
                config.result_rows,
                config.preview_rows,
                config.preview_visible,
            );
            picker
        } else {
            Picker::new("nfm-processes", ctx, source, config)
        };
        picker.set_external_preview(false);
        picker.set_bindings(Vec::new());
        picker.set_keybinding_help(vec![
            ("Ctrl+R".into(), "Refresh processes".into()),
            ("Ctrl+K".into(), "Kill selected process".into()),
        ]);
        let preview_adapter = adapter.clone();
        let formatter = nfm_win32::format_process_preview;
        picker.set_preview_provider(
            ctx,
            Arc::new(
                move |request: nfm_egui::PreviewRequest<S::Item>, _: nfm_egui::Cancellation| {
                    let Some(item) = preview_adapter.process(&request.selection.item) else {
                        return Ok(Preview::Empty);
                    };
                    Ok(Preview::Text(formatter(item)))
                },
            ),
        );
        Self {
            picker,
            store,
            pending: None,
            pending_kill: None,
            status: None,
            adapter,
        }
    }
    fn refresh(&mut self, ctx: &egui::Context, kill: Option<u32>) {
        if self.pending.is_some() {
            return;
        }
        self.picker.set_error(None);
        let (send, receive) = mpsc::channel();
        self.pending = Some(receive);
        self.pending_kill = kill;
        self.status = Some((
            "Refreshing processes…".into(),
            false,
            std::time::Instant::now(),
        ));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                if let Some(pid) = kill {
                    terminate_process(pid)
                        .map_err(|error| format!("Failed to terminate process {pid}: {error}"))?;
                }
                list_processes().map_err(|error| format!("Failed to list processes: {error}"))
            })();
            let _ = send.send(result);
            ctx.request_repaint();
        });
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        placement: Placement,
        appearance: &Appearance,
    ) -> bool {
        if let Some(result) = self
            .pending
            .as_ref()
            .and_then(|receive| match receive.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("process worker stopped".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            })
        {
            self.pending = None;
            match result {
                Ok(items) => {
                    self.store
                        .publish(Arc::new(ProcessPickerSnapshot::from_items(&items)));
                    self.store.complete();
                    self.status = self.pending_kill.take().map(|pid| {
                        (
                            format!("Terminated process {pid}"),
                            false,
                            std::time::Instant::now(),
                        )
                    });
                }
                Err(error) => {
                    self.pending_kill = None;
                    self.picker.set_error(Some(error.clone()));
                    self.status = Some((error, true, std::time::Instant::now()));
                }
            }
        }
        let refresh = ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::R));
        let kill = ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::K));
        if !self.picker.keybinding_help_visible() && self.pending.is_none() {
            if refresh {
                self.refresh(ctx, None);
            } else if kill {
                if let Some(selection) = self.picker.selection() {
                    if let Some(item) = self.adapter.process(&selection.item) {
                        self.refresh(ctx, Some(item.pid));
                    }
                } else {
                    self.picker.set_error(Some("No process selected".into()));
                }
            }
        }
        let output = self.picker.show(ctx, placement, appearance, false);
        if let Some((message, error, started)) = &self.status {
            if *error
                || self.pending.is_some()
                || started.elapsed() < std::time::Duration::from_secs(3)
            {
                egui::Area::new(egui::Id::new("nfm-process-status"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(output.rect.left_top() + egui::vec2(18.0, 8.0))
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(appearance.palette.background)
                            .inner_margin(4)
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(message).color(if *error {
                                    appearance.palette.error
                                } else {
                                    appearance.palette.muted
                                }));
                            });
                    });
            }
        }
        match output.event {
            Some(PickerEvent::Accepted(_)) | Some(PickerEvent::Cancelled) => true,
            Some(PickerEvent::CopyRequested(selection)) => {
                ctx.copy_text(selection.text);
                self.picker.notify_copied(ctx);
                false
            }
            Some(PickerEvent::CustomAction { .. }) | None => false,
        }
    }
}

impl ProcessPicker<ProcessPickerSnapshot> {
    /// Start background enumeration with default presentation (preview initially hidden).
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_picker(
            ctx,
            PickerConfig {
                preview_visible: false,
                ..Default::default()
            },
            Arc::new(ProcessItems),
            None,
        )
    }
    #[cfg(test)]
    fn from_store(ctx: &egui::Context, store: Arc<SnapshotStore<ProcessPickerSnapshot>>) -> Self {
        Self::from_store_with_picker(
            ctx,
            store,
            PickerConfig {
                preview_visible: false,
                ..Default::default()
            },
            Arc::new(ProcessItems),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(
        ctx: &egui::Context,
        menu: &mut ProcessPicker,
        key: Option<egui::Key>,
    ) -> egui::FullOutput {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 1000.0),
                )),
                events: key
                    .map(|key| egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::CTRL,
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            |ui| {
                menu.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                );
            },
        )
    }
    fn wait(ctx: &egui::Context, menu: &mut ProcessPicker, expected_cpu: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            frame(ctx, menu, None).drop_without_applying_deltas();
            if menu
                .picker
                .selection()
                .is_some_and(|s| s.item.cpu_seconds == expected_cpu)
            {
                return;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn structured_header_filter_preview_and_same_count_refresh() {
        let ctx = egui::Context::default();
        let store = Arc::new(SnapshotStore::new());
        let mut item = ProcessInfo {
            name: "fixture.exe".into(),
            pid: 1234,
            cpu_seconds: 1,
            ..Default::default()
        };
        store.publish(Arc::new(ProcessPickerSnapshot::from_items(&[item.clone()])));
        store.complete();
        let mut menu = ProcessPicker::from_store(&ctx, store.clone());
        menu.picker.set_query("/:PID==1234");
        wait(&ctx, &mut menu, 1);
        let output = frame(&ctx, &mut menu, None);
        assert!(output.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Text(text) if text.galley.job.text.contains("WorkingSet") && text.galley.job.text.contains("PID"))));
        output.drop_without_applying_deltas();
        let formatter = nfm_win32::format_process_preview;
        assert!(formatter(&menu.picker.selection().unwrap().item).contains("PID: 1234"));
        item.cpu_seconds = 2;
        store.publish(Arc::new(ProcessPickerSnapshot::from_items(&[item])));
        wait(&ctx, &mut menu, 2);
        assert_eq!(menu.picker.query(), "/:PID==1234");
        frame(&ctx, &mut menu, Some(egui::Key::P)).drop_without_applying_deltas();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let output = frame(&ctx, &mut menu, None);
            // Text previews are painted as cells to support selection and copying.
            let painted: String = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                    _ => None,
                })
                .collect();
            let visible = painted.contains("PID: 1234");
            output.drop_without_applying_deltas();
            if visible {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process preview never appeared"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        frame(&ctx, &mut menu, Some(egui::Key::R)).drop_without_applying_deltas();
        assert!(menu.pending.is_some());
        while menu.pending.is_some() {
            frame(&ctx, &mut menu, None).drop_without_applying_deltas();
            assert!(
                std::time::Instant::now() < deadline,
                "Ctrl+R did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(menu.picker.query(), "/:PID==1234");
    }
    #[test]
    fn ctrl_k_terminates_only_the_spawned_fixture_and_refreshes() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let ctx = egui::Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(ProcessPickerSnapshot::from_items(&[
            ProcessInfo {
                name: "owned-test-process".into(),
                pid: child.id(),
                cpu_seconds: 77,
                ..Default::default()
            },
        ])));
        store.complete();
        let mut menu = ProcessPicker::from_store(&ctx, store);
        wait(&ctx, &mut menu, 77);
        frame(&ctx, &mut menu, Some(egui::Key::K)).drop_without_applying_deltas();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while menu.pending.is_some() {
            frame(&ctx, &mut menu, None).drop_without_applying_deltas();
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("termination did not complete");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(child.try_wait().unwrap().is_some());
        assert!(
            menu.status
                .as_ref()
                .is_some_and(|(text, error, _)| !error && text.contains(&child.id().to_string()))
        );
    }
    #[test]
    fn help_blocks_actions_and_worker_errors_preserve_existing_source() {
        let ctx = egui::Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(ProcessPickerSnapshot::from_items(&[
            ProcessInfo {
                name: "fixture".into(),
                pid: 1234,
                cpu_seconds: 88,
                ..Default::default()
            },
        ])));
        store.complete();
        let mut menu = ProcessPicker::from_store(&ctx, store);
        wait(&ctx, &mut menu, 88);
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Slash,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                }],
                ..Default::default()
            },
            |ui| {
                menu.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                );
            },
        )
        .drop_without_applying_deltas();
        assert!(menu.picker.keybinding_help_visible());
        frame(&ctx, &mut menu, Some(egui::Key::R)).drop_without_applying_deltas();
        frame(&ctx, &mut menu, Some(egui::Key::K)).drop_without_applying_deltas();
        assert!(!menu.busy());
        let (send, receive) = mpsc::channel();
        menu.pending = Some(receive);
        send.send(Err("fixture worker error".into())).unwrap();
        frame(&ctx, &mut menu, None).drop_without_applying_deltas();
        assert!(!menu.busy());
        assert!(
            menu.status
                .as_ref()
                .is_some_and(|(message, error, _)| *error && message == "fixture worker error")
        );
        assert_eq!(menu.picker.selection().unwrap().item.pid, 1234);
    }
}

struct SharedAdapter;
impl ProcessSourceAdapter<nfm_egui::shared::SharedSource> for SharedAdapter {
    fn source(
        &self,
        store: Arc<SnapshotStore<ProcessPickerSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<nfm_egui::shared::SharedSource>> {
        nfm_egui::shared::provider(store)
    }
    fn process<'a>(&self, item: &'a nfm_egui::shared::SharedItem) -> Option<&'a ProcessPickerItem> {
        item.downcast_ref::<ProcessPickerItem>()
    }
}
impl ProcessPicker<nfm_egui::shared::SharedSource> {
    /// Open this workflow using NFM's reusable picker across payload types.
    pub fn with_shared_picker(
        ctx: &egui::Context,
        config: PickerConfig,
        previous: Option<nfm_egui::shared::SharedPicker>,
    ) -> Self {
        Self::with_picker(ctx, config, Arc::new(SharedAdapter), previous)
    }
}
