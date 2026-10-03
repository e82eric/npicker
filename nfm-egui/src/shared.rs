//! Retain one picker UI while replacing sources with different typed payloads.
//! Source search plans, headers, completion, streaming and wake semantics are delegated.
use nfm_search_core::{
    fuzzy_search_session::SearchSnapshotProvider,
    store::{ItemsSource, SearchCompletion, SearchPlan},
};
use std::{any::Any, sync::Arc};

/// Owned typed payload used by a shared picker. Downcast only at workflow boundaries.
#[derive(Clone)]
pub struct SharedItem(Arc<dyn Any + Send + Sync>);
impl SharedItem {
    pub fn new<T: Send + Sync + 'static>(item: T) -> Self {
        Self(Arc::new(item))
    }
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}
impl std::fmt::Debug for SharedItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedItem").finish_non_exhaustive()
    }
}
pub type SharedPicker = crate::Picker<SharedSource>;
pub struct SharedSource(Arc<dyn ItemsSource<Item = SharedItem> + Send + Sync>);
struct Mapped<S: ItemsSource> {
    source: Arc<S>,
}

// Delegate search plans, headers and completions instead of flattening the source.
macro_rules! delegate {
    ($field:tt) => {
        fn version(&self) -> u64 {
            self.$field.version()
        }
        fn len(&self) -> usize {
            self.$field.len()
        }
        fn is_empty(&self) -> bool {
            self.$field.is_empty()
        }
        fn get_string<'a>(
            &'a self,
            index: usize,
            stack: &'a mut [u8],
            heap: &'a mut Vec<u8>,
        ) -> &'a [u8] {
            self.$field.get_string(index, stack, heap)
        }
        fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
            self.$field.get_string_lossy(index, out)
        }
        fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
            self.$field.create_search_plan(query)
        }
        fn header(&self, query: &str) -> Option<String> {
            self.$field.header(query)
        }
        fn display_text(&self, index: usize, query: &str) -> Option<String> {
            self.$field.display_text(index, query)
        }
        fn completions(&self, input: &str, cursor: usize) -> Vec<SearchCompletion> {
            self.$field.completions(input, cursor)
        }
        fn effective_query(&self, input: &str) -> String {
            self.$field.effective_query(input)
        }
    };
}
impl<S: ItemsSource + Send + Sync> ItemsSource for Mapped<S>
where
    S::Item: Send + Sync + 'static,
{
    type Item = SharedItem;
    delegate!(source);
    fn item(&self, index: usize) -> Option<SharedItem> {
        self.source.item(index).map(SharedItem::new)
    }
}
impl ItemsSource for SharedSource {
    type Item = SharedItem;
    delegate!(0);
    fn item(&self, index: usize) -> Option<SharedItem> {
        self.0.item(index)
    }
}

struct Provider<S: ItemsSource, P> {
    source: Arc<P>,
    marker: std::marker::PhantomData<fn() -> S>,
}
impl<S, P> SearchSnapshotProvider<SharedSource> for Provider<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    fn snapshot(&self) -> Option<Arc<SharedSource>> {
        self.source
            .snapshot()
            .map(|source| Arc::new(SharedSource(Arc::new(Mapped { source }))))
    }
    fn snapshot_version(&self) -> u64 {
        self.source.snapshot_version()
    }
    fn is_done(&self) -> bool {
        self.source.is_done()
    }
    fn is_append_only(&self) -> bool {
        self.source.is_append_only()
    }
    fn subscribe_updates(&self, wake: crossbeam_channel::Sender<()>) -> bool {
        self.source.subscribe_updates(wake)
    }
}
pub fn provider<S, P>(source: Arc<P>) -> Arc<impl SearchSnapshotProvider<SharedSource>>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    Arc::new(Provider {
        source,
        marker: std::marker::PhantomData,
    })
}

pub fn replace<S, P>(
    ctx: &crate::egui::Context,
    source: Arc<P>,
    config: crate::PickerConfig,
    previous: Option<crate::Picker<SharedSource>>,
) -> crate::Picker<SharedSource>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    let source = provider(source);
    if let Some(mut picker) = previous {
        picker.replace_source(ctx, source, config.initial_query);
        picker.set_presentation(
            config.result_rows,
            config.preview_rows,
            config.preview_visible,
        );
        picker.set_external_preview(false);
        picker.set_bindings(config.bindings);
        picker.set_keybinding_help(Vec::new());
        picker.set_preview_provider(
            ctx,
            Arc::new(
                |_: crate::PreviewRequest<SharedItem>, _: crate::Cancellation| {
                    Ok(crate::Preview::Empty)
                },
            ),
        );
        picker
    } else {
        crate::Picker::new("nfm-shared-picker", ctx, source, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Appearance, PickerConfig, Placement, egui};
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    #[test]
    fn cross_type_replacement_retains_rows_and_rejects_stale_selection() {
        let ctx = egui::Context::default();
        let old = Arc::new(SnapshotStore::new());
        old.publish(Arc::new(FlatSnapshot::from_items([(
            "retained-command",
            "command".to_owned(),
        )])));
        old.complete();
        let mut picker = replace(&ctx, old, PickerConfig::default(), None);
        let frame = |picker: &mut SharedPicker| {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                },
            )
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while picker.selection().is_none() {
            frame(&mut picker).drop_without_applying_deltas();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let original = picker.selection().unwrap();
        let next = Arc::new(SnapshotStore::<FlatSnapshot<u32>>::new());
        next.publish(Arc::new(FlatSnapshot::from_items(std::iter::empty::<(
            &str,
            u32,
        )>())));
        picker = replace(&ctx, next.clone(), PickerConfig::default(), Some(picker));
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                let output = picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
                assert!(
                    output.event.is_none(),
                    "retained old rows must not be accepted"
                );
            },
        )
        .drop_without_applying_deltas();
        for _ in 0..5 {
            let output = frame(&mut picker);
            assert!(picker.selection().is_none());
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains("retained-command"))));
            output.drop_without_applying_deltas();
        }
        next.publish(Arc::new(FlatSnapshot::from_items([(
            "new-process",
            1234u32,
        )])));
        next.complete();
        while picker.selection().is_none() {
            frame(&mut picker).drop_without_applying_deltas();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            picker.selection().unwrap().item.downcast_ref::<u32>(),
            Some(&1234)
        );
        assert!(
            picker
                .selection()
                .unwrap()
                .item
                .downcast_ref::<String>()
                .is_none()
        );
        assert_eq!(original.item.downcast_ref::<String>().unwrap(), "command");
    }
}
