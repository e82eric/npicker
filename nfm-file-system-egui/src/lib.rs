//! Host-owned filesystem picker. NFM owns scanning, navigation and previews.
#![cfg(windows)]
use nfm_egui::{
    Appearance, Cancellation, ItemsSource, Picker, PickerConfig, PickerEvent, Placement, Preview,
    PreviewRequest, SearchSnapshotProvider, egui,
};
pub use nfm_file_system::FileSystemPickerOptions;
use nfm_file_system::{
    walker::{OwnedFileWalkerScan, PublishedSnapshot, ScanOptions, start_scan},
    walker_search_store::FileSystemSearchStore,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Adapt filesystem snapshots to a host's shared picker item type.
/// Navigation and preview behavior remain inside NFM.
pub trait FileSourceAdapter<S: ItemsSource>: Send + Sync + 'static {
    fn source(&self, store: Arc<FileSystemSearchStore>) -> Arc<dyn SearchSnapshotProvider<S>>;
    fn path<'a>(&self, item: &'a S::Item) -> Option<&'a str>;
}
/// The standalone picker uses filesystem snapshots directly.
pub struct FilePaths;
impl FileSourceAdapter<PublishedSnapshot> for FilePaths {
    fn source(
        &self,
        store: Arc<FileSystemSearchStore>,
    ) -> Arc<dyn SearchSnapshotProvider<PublishedSnapshot>> {
        store
    }
    fn path<'a>(&self, item: &'a String) -> Option<&'a str> {
        Some(item)
    }
}
/// A completed file selection or user cancellation. Directories navigate internally.
#[derive(Debug, PartialEq, Eq)]
pub enum FileEvent {
    Selected(String),
    Cancelled,
}
/// Embedded filesystem picker; the host supplies the egui context and placement.
pub struct FilePicker<S: ItemsSource + Send + Sync + 'static = PublishedSnapshot> {
    picker: Picker<S>,
    scan: OwnedFileWalkerScan,
    options: FileSystemPickerOptions,
    navigation_frame: u64,
    adapter: Arc<dyn FileSourceAdapter<S>>,
}
impl FilePicker<PublishedSnapshot> {
    /// Create a standalone picker with default presentation.
    pub fn new(ctx: &egui::Context, options: FileSystemPickerOptions) -> Result<Self, String> {
        Self::with_picker(
            ctx,
            options,
            PickerConfig::default(),
            Arc::new(FilePaths),
            None,
        )
    }
}
impl<S: ItemsSource + Send + Sync + 'static> FilePicker<S>
where
    S::Item: Send + 'static,
{
    /// Return the reusable UI control; the filesystem scan is cancelled.
    pub fn into_picker(self) -> Picker<S> {
        self.picker
    }
    pub fn picker(&self) -> &Picker<S> {
        &self.picker
    }
    pub fn picker_mut(&mut self) -> &mut Picker<S> {
        &mut self.picker
    }
    pub fn options(&self) -> &FileSystemPickerOptions {
        &self.options
    }
    pub fn set_rows(&mut self, result_rows: usize, preview_rows: usize) {
        self.picker
            .set_presentation(result_rows, preview_rows, self.picker.preview_visible());
    }
    /// Create a filesystem picker, optionally retaining an existing picker UI.
    pub fn with_picker(
        ctx: &egui::Context,
        mut options: FileSystemPickerOptions,
        mut config: PickerConfig,
        adapter: Arc<dyn FileSourceAdapter<S>>,
        previous: Option<Picker<S>>,
    ) -> Result<Self, String> {
        if options.roots.is_empty() {
            options.roots = nfm_win32::logical_drive_roots();
        }
        if options.roots.is_empty() {
            return Err("no file-system roots are available".into());
        }
        let (store, scan) = scan_source(&options);
        config.title = "Files".into();
        config.initial_query = options.search_string.clone().unwrap_or_default();
        config.preview_visible = options.preview_visible;
        let source = adapter.source(store);
        let mut picker = if let Some(mut picker) = previous {
            picker.replace_source(ctx, source, config.initial_query);
            picker.set_presentation(
                config.result_rows,
                config.preview_rows,
                config.preview_visible,
            );
            picker
        } else {
            Picker::new("nfm-files", ctx, source, config)
        };
        picker.set_external_preview(false);
        picker.set_bindings(Vec::new());
        picker.set_keybinding_help(vec![(
            "Ctrl+U".into(),
            "Parent directory / drive list".into(),
        )]);
        let native_provider =
            nfm_preview::egui::file_preview_provider_with_bat_theme(options.bat_theme.clone());
        let preview_adapter = adapter.clone();
        picker.set_preview_provider(
            ctx,
            Arc::new(
                move |request: PreviewRequest<S::Item>, cancel: Cancellation| {
                    let Some(path) = preview_adapter.path(&request.selection.item) else {
                        return Ok(Preview::Empty);
                    };
                    let path = path.to_owned();
                    let request = PreviewRequest {
                        selection: nfm_egui::Selection {
                            item: path,
                            text: request.selection.text,
                            index: request.selection.index,
                            source_version: request.selection.source_version,
                            snapshot: request.selection.snapshot,
                        },
                        document_version: request.document_version,
                        initial_page: request.initial_page,
                        columns: request.columns,
                        rows: request.rows,
                        scroll_offset: request.scroll_offset,
                        foreground: request.foreground,
                        background: request.background,
                    };
                    native_provider.preview(request, cancel)
                },
            ),
        );
        Ok(Self {
            picker,
            scan,
            options,
            adapter,
            navigation_frame: ctx.cumulative_frame_nr(),
        })
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        placement: Placement,
        appearance: &Appearance,
    ) -> Result<Option<FileEvent>, String> {
        if self.navigation_frame == ctx.cumulative_frame_nr() {
            // Suppress the previous directory's accept key on repeated egui
            // layout passes, but still paint the retained control this frame.
            ctx.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::Enter,
                            ..
                        }
                    )
                });
            });
        }
        if !self.picker.keybinding_help_visible()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::U))
        {
            let roots = nfm_file_system::parent_roots(
                &self.options.roots,
                nfm_win32::logical_drive_roots(),
            );
            self.navigate(ctx, roots)?;
        }
        let output = self.picker.show(ctx, placement, appearance, false);
        match output.event {
            Some(PickerEvent::Accepted(selection)) => {
                let Some(path) = self.adapter.path(&selection.item) else {
                    return Ok(None);
                };
                let path = path.to_owned();
                if Path::new(&path).is_dir() {
                    self.navigate(ctx, vec![path])?;
                    Ok(None)
                } else {
                    Ok(Some(FileEvent::Selected(path)))
                }
            }
            Some(PickerEvent::Cancelled) => Ok(Some(FileEvent::Cancelled)),
            Some(PickerEvent::CopyRequested(selection)) => {
                if let Some(path) = self.adapter.path(&selection.item) {
                    ctx.copy_text(path.to_owned());
                }
                self.picker.notify_copied(ctx);
                Ok(None)
            }
            Some(PickerEvent::CustomAction { .. }) | None => Ok(None),
        }
    }
    pub fn navigate(&mut self, ctx: &egui::Context, roots: Vec<String>) -> Result<(), String> {
        let options = self
            .options
            .navigate_to(roots, self.picker.preview_visible());
        let (store, scan) = scan_source(&options);
        self.scan = scan;
        self.picker
            .replace_source(ctx, self.adapter.source(store), String::new());
        self.options = options;
        self.navigation_frame = ctx.cumulative_frame_nr();
        ctx.request_repaint();
        Ok(())
    }
}

fn scan_source(
    options: &FileSystemPickerOptions,
) -> (Arc<FileSystemSearchStore>, OwnedFileWalkerScan) {
    // Append-only snapshot indices remain immutable as the walker publishes.
    let store = Arc::new(FileSystemSearchStore::new_append_only());
    store.publish(Arc::new(PublishedSnapshot::empty()));
    let scan = start_scan(
        ScanOptions {
            roots: options.roots.iter().map(PathBuf::from).collect(),
            max_depth: options.max_depth,
            directories_only: options.directories_only,
            files_only: options.files_only,
        },
        store.clone(),
    )
    .cancel_on_drop();
    (store, scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn draw(
        ctx: &egui::Context,
        menu: &mut FilePicker,
        key: Option<(egui::Key, egui::Modifiers)>,
    ) -> Option<FileEvent> {
        let mut result = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 900.0),
                )),
                events: key
                    .map(|(key, modifiers)| egui::Event::Key {
                        key,
                        modifiers,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            |ui| {
                let event = menu
                    .show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                    )
                    .unwrap();
                if event.is_some() {
                    result = event;
                }
            },
        );
        output.drop_without_applying_deltas();
        result
    }
    fn select(ctx: &egui::Context, menu: &mut FilePicker, path: &Path) {
        menu.picker
            .set_query(path.file_name().unwrap().to_str().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            draw(ctx, menu, None);
            if menu
                .picker
                .selection()
                .is_some_and(|selection| Path::new(selection.item.as_str()) == path)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "scan/search did not select {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn ctrl_w_toggles_native_file_preview_focus() {
        let root = std::env::temp_dir().join(format!("nfm-copy-focus-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("preview-focus.txt");
        fs::write(&path, "first row\nsecond row\n").unwrap();
        let ctx = egui::Context::default();
        let mut menu = FilePicker::new(
            &ctx,
            FileSystemPickerOptions {
                roots: vec![root.to_string_lossy().into_owned()],
                preview_visible: true,
                ..Default::default()
            },
        )
        .unwrap();
        select(&ctx, &mut menu, &path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            draw(
                &ctx,
                &mut menu,
                Some((
                    egui::Key::W,
                    (egui::Modifiers::CTRL | egui::Modifiers::COMMAND),
                )),
            );
            if menu.picker.preview_focused() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Ctrl+W never focused the file preview"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        draw(&ctx, &mut menu, None);
        assert!(menu.picker.preview_focused());
        draw(
            &ctx,
            &mut menu,
            Some((
                egui::Key::W,
                (egui::Modifiers::CTRL | egui::Modifiers::COMMAND),
            )),
        );
        assert!(!menu.picker.preview_focused());
        drop(menu);
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn help_blocks_parent_navigation_and_subtree_transition_matches_skia() {
        let root = std::env::temp_dir().join(format!("nfm-help-navigation-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let ctx = egui::Context::default();
        let mut menu = FilePicker::new(
            &ctx,
            FileSystemPickerOptions {
                roots: vec![root.to_string_lossy().into_owned()],
                files_only: true,
                preview_visible: false,
                ..Default::default()
            },
        )
        .unwrap();
        draw(&ctx, &mut menu, None);
        draw(
            &ctx,
            &mut menu,
            Some((
                egui::Key::Slash,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            )),
        );
        assert!(menu.picker.keybinding_help_visible());
        draw(&ctx, &mut menu, Some((egui::Key::U, egui::Modifiers::CTRL)));
        assert_eq!(Path::new(&menu.options.roots[0]), root);
        assert!(
            draw(
                &ctx,
                &mut menu,
                Some((egui::Key::Escape, egui::Modifiers::NONE))
            )
            .is_none()
        );
        menu.navigate(&ctx, vec![root.to_string_lossy().into_owned()])
            .unwrap();
        assert!(!menu.options.files_only);
        assert!(!menu.options.directories_only);
        assert!(!menu.options.preview_visible);
        assert_eq!(menu.options.max_depth, i32::MAX);
        drop(menu);
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn scanner_search_directory_enter_parent_and_file_accept_work_together() {
        let root = std::env::temp_dir().join(format!("nfm-navigation-{}", std::process::id()));
        let child = root.join("unique-child-directory");
        fs::create_dir_all(&child).unwrap();
        let file = child.join("unique-preview-document.txt");
        fs::write(&file, "preview contents").unwrap();
        let ctx = egui::Context::default();
        let mut menu = FilePicker::new(
            &ctx,
            FileSystemPickerOptions {
                roots: vec![root.to_string_lossy().into_owned()],
                max_depth: 5,
                preview_visible: true,
                ..Default::default()
            },
        )
        .unwrap();
        select(&ctx, &mut menu, &child);
        assert!(
            draw(
                &ctx,
                &mut menu,
                Some((egui::Key::Enter, egui::Modifiers::NONE))
            )
            .is_none()
        );
        assert_eq!(
            menu.options.roots,
            vec![child.to_string_lossy().into_owned()]
        );
        draw(&ctx, &mut menu, None);
        draw(&ctx, &mut menu, Some((egui::Key::U, egui::Modifiers::CTRL)));
        assert_eq!(
            menu.options.roots,
            vec![root.to_string_lossy().into_owned()]
        );
        select(&ctx, &mut menu, &file);
        assert!(
            matches!(draw(&ctx, &mut menu, Some((egui::Key::Enter, egui::Modifiers::NONE))), Some(FileEvent::Selected(path)) if Path::new(&path) == file)
        );
        drop(menu);
        fs::remove_file(file).unwrap();
        fs::remove_dir(child).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn parent_navigation_preserves_the_live_preview_toggle_in_both_directions() {
        let root =
            std::env::temp_dir().join(format!("nfm-preview-navigation-{}", std::process::id()));
        let child = root.join("child");
        fs::create_dir_all(&child).unwrap();
        let ctx = egui::Context::default();
        let mut menu = FilePicker::new(
            &ctx,
            FileSystemPickerOptions {
                roots: vec![child.to_string_lossy().into_owned()],
                preview_visible: true,
                ..Default::default()
            },
        )
        .unwrap();
        draw(&ctx, &mut menu, None);
        draw(&ctx, &mut menu, Some((egui::Key::P, egui::Modifiers::CTRL)));
        assert!(!menu.picker.preview_visible());
        draw(
            &ctx,
            &mut menu,
            Some((egui::Key::U, egui::Modifiers::CTRL | egui::Modifiers::SHIFT)),
        );
        assert_eq!(Path::new(&menu.options.roots[0]), root);
        assert!(!menu.options.preview_visible);
        assert!(!menu.picker.preview_visible());
        draw(&ctx, &mut menu, None);
        draw(&ctx, &mut menu, Some((egui::Key::P, egui::Modifiers::CTRL)));
        assert!(menu.picker.preview_visible());
        menu.navigate(&ctx, vec![child.to_string_lossy().into_owned()])
            .unwrap();
        assert!(menu.options.preview_visible);
        assert!(menu.picker.preview_visible());
        drop(menu);
        fs::remove_dir(child).unwrap();
        fs::remove_dir(root).unwrap();
    }
}

struct SharedAdapter;
impl FileSourceAdapter<nfm_egui::shared::SharedSource> for SharedAdapter {
    fn source(
        &self,
        store: Arc<FileSystemSearchStore>,
    ) -> Arc<dyn SearchSnapshotProvider<nfm_egui::shared::SharedSource>> {
        nfm_egui::shared::provider(store)
    }
    fn path<'a>(&self, item: &'a nfm_egui::shared::SharedItem) -> Option<&'a str> {
        item.downcast_ref::<String>().map(String::as_str)
    }
}
impl FilePicker<nfm_egui::shared::SharedSource> {
    /// Open this workflow using NFM's reusable picker across payload types.
    pub fn with_shared_picker(
        ctx: &egui::Context,
        options: FileSystemPickerOptions,
        config: PickerConfig,
        previous: Option<nfm_egui::shared::SharedPicker>,
    ) -> Result<Self, String> {
        Self::with_picker(ctx, options, config, Arc::new(SharedAdapter), previous)
    }
}
