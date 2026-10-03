//! Embedded window picker; hosts own windows and handle the selected HWND.
#![cfg(windows)]
use nfm_egui::{
    Appearance, ItemsSource, Picker, PickerConfig, PickerEvent, Placement, SearchSnapshotProvider,
    egui,
};
use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
use nfm_win32::{
    WindowPickerItem,
    list_windows::{WindowListItem, list_windows},
};
use std::sync::Arc;
use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
/// Immutable window candidates, excluding the host popup.
pub type WindowSnapshot = FlatSnapshot<WindowPickerItem>;
/// Map native window snapshots to the host's shared picker type.
pub trait WindowSourceAdapter<S: ItemsSource>: Send + Sync + 'static {
    fn source(
        &self,
        store: Arc<SnapshotStore<WindowSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<S>>;
    fn window<'a>(&self, item: &'a S::Item) -> Option<&'a WindowPickerItem>;
}
pub struct WindowItems;
impl WindowSourceAdapter<WindowSnapshot> for WindowItems {
    fn source(
        &self,
        store: Arc<SnapshotStore<WindowSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<WindowSnapshot>> {
        store
    }
    fn window<'a>(&self, item: &'a WindowPickerItem) -> Option<&'a WindowPickerItem> {
        Some(item)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum WindowEvent {
    Selected(isize),
    Cancelled,
}
/// A picker with a live thumbnail rendered into the supplied host window.
/// Drop it before destroying the destination HWND to release the DWM thumbnail.
pub struct WindowPicker<S: ItemsSource + Send + Sync + 'static = WindowSnapshot> {
    picker: Picker<S>,
    thumbnail: WindowThumbnail,
    destination: isize,
    adapter: Arc<dyn WindowSourceAdapter<S>>,
}
impl WindowPicker<WindowSnapshot> {
    pub fn new(ctx: &egui::Context, destination: isize) -> Result<Self, String> {
        Self::with_picker(
            ctx,
            destination,
            PickerConfig::default(),
            Arc::new(WindowItems),
            None,
        )
    }
}
impl<S: ItemsSource + Send + Sync + 'static> WindowPicker<S>
where
    S::Item: Send + 'static,
{
    /// Enumerate fresh candidates and optionally retain a host's existing UI control.
    pub fn with_picker(
        ctx: &egui::Context,
        destination: isize,
        config: PickerConfig,
        adapter: Arc<dyn WindowSourceAdapter<S>>,
        previous: Option<Picker<S>>,
    ) -> Result<Self, String> {
        let items = list_windows().map_err(|e| e.to_string())?;
        Ok(Self::from_items(
            ctx,
            destination,
            items,
            config,
            adapter,
            previous,
        ))
    }
    fn from_items(
        ctx: &egui::Context,
        destination: isize,
        items: Vec<WindowListItem>,
        mut config: PickerConfig,
        adapter: Arc<dyn WindowSourceAdapter<S>>,
        previous: Option<Picker<S>>,
    ) -> Self {
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(window_snapshot(items, destination)));
        store.complete();
        config.title = "Windows".into();
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
            Picker::new("nfm-windows", ctx, source, config)
        };
        picker.set_external_preview(true);
        picker.set_bindings(Vec::new());
        picker.set_keybinding_help(Vec::new());
        Self {
            picker,
            thumbnail: WindowThumbnail::default(),
            destination,
            adapter,
        }
    }
    /// Return the reusable UI control and release the current thumbnail.
    pub fn into_picker(self) -> Picker<S> {
        self.picker
    }
    pub fn picker(&self) -> &Picker<S> {
        &self.picker
    }
    pub fn picker_mut(&mut self) -> &mut Picker<S> {
        &mut self.picker
    }
    pub fn set_rows(&mut self, result_rows: usize, preview_rows: usize) {
        self.picker
            .set_presentation(result_rows, preview_rows, self.picker.preview_visible());
    }
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        placement: Placement,
        appearance: &Appearance,
    ) -> Option<WindowEvent> {
        let output = self.picker.show(ctx, placement, appearance, false);
        let target = output.preview_rect.and_then(|rect| {
            self.picker.selection().and_then(|s| {
                self.adapter
                    .window(&s.item)
                    .map(|item| (item.native_window, rect))
            })
        });
        if let Err(error) = self
            .thumbnail
            .update(self.destination, target, ctx.pixels_per_point())
        {
            self.picker.set_error(Some(error));
        }
        match output.event {
            Some(PickerEvent::Accepted(selection)) => self
                .adapter
                .window(&selection.item)
                .map(|item| WindowEvent::Selected(item.native_window)),
            Some(PickerEvent::Cancelled) => Some(WindowEvent::Cancelled),
            Some(PickerEvent::CopyRequested(selection)) => {
                ctx.copy_text(selection.text);
                self.picker.notify_copied(ctx);
                None
            }
            Some(PickerEvent::CustomAction { .. }) | None => None,
        }
    }
}
fn window_snapshot(items: Vec<WindowListItem>, destination: isize) -> WindowSnapshot {
    FlatSnapshot::from_items(
        items
            .into_iter()
            .filter(|item| item.hwnd != destination)
            .map(|item| {
                let payload = WindowPickerItem {
                    title: item.text.clone(),
                    native_window: item.hwnd,
                };
                (item.text, payload)
            }),
    )
}
/// DWM composites only the selected window into the popup; no screen capture.
#[derive(Default)]
struct WindowThumbnail {
    handle: isize,
    source: isize,
}
impl WindowThumbnail {
    fn clear(&mut self) {
        if self.handle != 0 {
            unsafe {
                windows_sys::Win32::Graphics::Dwm::DwmUnregisterThumbnail(self.handle);
            }
        }
        self.handle = 0;
        self.source = 0;
    }
    fn update(
        &mut self,
        destination: isize,
        target: Option<(isize, egui::Rect)>,
        scale: f32,
    ) -> Result<(), String> {
        use windows_sys::Win32::{
            Foundation::{RECT, SIZE},
            Graphics::Dwm::*,
        };
        let Some((source, rect)) = target.filter(|(_, rect)| rect.is_positive()) else {
            self.clear();
            return Ok(());
        };
        unsafe {
            if IsWindow(source as _) == 0 {
                self.clear();
                return Ok(());
            }
            if self.source != source {
                self.clear();
                let result = DwmRegisterThumbnail(destination as _, source as _, &mut self.handle);
                if result < 0 {
                    self.clear();
                    return Err(format!("Window preview unavailable ({result:#x})"));
                }
                self.source = source;
            }
            let mut size = SIZE::default();
            let result = DwmQueryThumbnailSourceSize(self.handle, &mut size);
            if result < 0 || size.cx <= 0 || size.cy <= 0 {
                self.clear();
                return Ok(());
            }
            let fitted = fit_window_thumbnail(rect, [size.cx, size.cy]);
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION
                    | DWM_TNP_VISIBLE
                    | DWM_TNP_OPACITY
                    | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: (fitted.left() * scale).round() as i32,
                    top: (fitted.top() * scale).round() as i32,
                    right: (fitted.right() * scale).round() as i32,
                    bottom: (fitted.bottom() * scale).round() as i32,
                },
                opacity: 255,
                fVisible: 1,
                fSourceClientAreaOnly: 0,
                ..Default::default()
            };
            let result = DwmUpdateThumbnailProperties(self.handle, &props);
            if result < 0 {
                self.clear();
                return Err(format!("Window preview unavailable ({result:#x})"));
            }
        }
        Ok(())
    }
}
impl Drop for WindowThumbnail {
    fn drop(&mut self) {
        self.clear();
    }
}
fn fit_window_thumbnail(bounds: egui::Rect, size: [i32; 2]) -> egui::Rect {
    let ratio = (bounds.width() / size[0] as f32).min(bounds.height() / size[1] as f32);
    egui::Rect::from_center_size(
        bounds.center(),
        egui::vec2(size[0] as f32 * ratio, size[1] as f32 * ratio),
    )
}
#[cfg(test)]
mod window_preview_tests {
    use super::*;
    #[test]
    fn thumbnail_preserves_aspect_ratio_and_stays_inside_preview() {
        let bounds = egui::Rect::from_min_size(egui::pos2(20.0, 30.0), egui::vec2(800.0, 300.0));
        for size in [[1920, 1080], [600, 1200], [800, 300]] {
            let fitted = fit_window_thumbnail(bounds, size);
            assert_eq!(fitted.center(), bounds.center());
            assert!(bounds.contains_rect(fitted));
            assert!(
                (fitted.width() / fitted.height() - size[0] as f32 / size[1] as f32).abs() < 0.001
            );
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn items() -> Vec<WindowListItem> {
        vec![
            WindowListItem {
                text: "host popup".into(),
                hwnd: 1,
            },
            WindowListItem {
                text: "selected window".into(),
                hwnd: 2,
            },
        ]
    }
    fn draw(
        ctx: &egui::Context,
        menu: &mut WindowPicker,
        key: Option<egui::Key>,
    ) -> Option<WindowEvent> {
        let mut event = None;
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 900.0),
                )),
                events: key
                    .map(|key| egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            |ui| {
                if let Some(value) = menu.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                ) {
                    event = Some(value);
                }
            },
        )
        .drop_without_applying_deltas();
        event
    }
    fn wait(ctx: &egui::Context, menu: &mut WindowPicker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(ctx, menu, None);
            if menu
                .picker()
                .selection()
                .is_some_and(|s| s.item.native_window == 2)
            {
                return;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn candidates_exclude_host_and_keep_native_payloads() {
        let snapshot = window_snapshot(items(), 1);
        assert_eq!(snapshot.len(), 1);
        let item = snapshot.item(0).unwrap();
        assert_eq!(item.native_window, 2);
        assert_eq!(item.title, "selected window");
    }
    #[test]
    fn selection_cancel_and_reuse_preserve_window_behavior() {
        let ctx = egui::Context::default();
        let config = PickerConfig {
            preview_visible: false,
            ..Default::default()
        };
        let mut menu = WindowPicker::from_items(
            &ctx,
            1,
            items(),
            config.clone(),
            Arc::new(WindowItems),
            None,
        );
        wait(&ctx, &mut menu);
        assert_eq!(
            draw(&ctx, &mut menu, Some(egui::Key::Enter)),
            Some(WindowEvent::Selected(2))
        );
        let picker = menu.into_picker();
        let mut menu = WindowPicker::from_items(
            &ctx,
            1,
            items(),
            config,
            Arc::new(WindowItems),
            Some(picker),
        );
        assert!(menu.picker().selection().is_none());
        wait(&ctx, &mut menu);
        assert_eq!(
            draw(&ctx, &mut menu, Some(egui::Key::Escape)),
            Some(WindowEvent::Cancelled)
        );
    }
}

struct SharedAdapter;
impl WindowSourceAdapter<nfm_egui::shared::SharedSource> for SharedAdapter {
    fn source(
        &self,
        store: Arc<SnapshotStore<WindowSnapshot>>,
    ) -> Arc<dyn SearchSnapshotProvider<nfm_egui::shared::SharedSource>> {
        nfm_egui::shared::provider(store)
    }
    fn window<'a>(&self, item: &'a nfm_egui::shared::SharedItem) -> Option<&'a WindowPickerItem> {
        item.downcast_ref::<WindowPickerItem>()
    }
}
impl WindowPicker<nfm_egui::shared::SharedSource> {
    /// Open this workflow using NFM's reusable picker across payload types.
    pub fn with_shared_picker(
        ctx: &egui::Context,
        destination: isize,
        config: PickerConfig,
        previous: Option<nfm_egui::shared::SharedPicker>,
    ) -> Result<Self, String> {
        Self::with_picker(ctx, destination, config, Arc::new(SharedAdapter), previous)
    }
}
