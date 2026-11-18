//! A picker whose rows are found by a search it only exchanges messages
//! with, each of which takes a while to arrive: what a picker shown on one
//! machine and searched on another is like. `--delay MS` is how long a
//! message takes each way; `--query TEXT` is what the picker starts with.
use nfm_egui::{
    Appearance, Picker, PickerConfig, PickerEvent, Placement, egui,
    fed::{Asked, Feed, Found, Page, Row, Rows},
};
use nfm_search_core::{
    fuzzy_search_session::FuzzySearchSession,
    search::SearchSortMode,
    snapshot_store::SnapshotStore,
    store::{PublishingStreamingItemStoreWithPayload, StreamingItemSnapshotWithPayload},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

type Source = StreamingItemSnapshotWithPayload<u64>;
type Search = FuzzySearchSession<Source, SnapshotStore<Source>>;

/// One way between the picker and the search: what is sent arrives after
/// the delay, in the order it was sent.
#[derive(Clone)]
struct Link(mpsc::Sender<(Instant, Box<dyn FnOnce() + Send>)>, Duration);
impl Link {
    fn new(delay: Duration) -> Self {
        let (sender, receiver) = mpsc::channel::<(Instant, Box<dyn FnOnce() + Send>)>();
        std::thread::spawn(move || {
            for (due, arrive) in receiver {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                arrive();
            }
        });
        Self(sender, delay)
    }
    fn send(&self, arrive: impl FnOnce() + Send + 'static) {
        let _ = self.0.send((Instant::now() + self.1, Box::new(arrive)));
    }
}

/// The far end: a source that is still being added to, and a search of it
/// that answers with the rows asked for and no more.
struct Engine {
    store: Arc<SnapshotStore<Source>>,
    search: Mutex<Option<Search>>,
    rows: AtomicUsize,
    back: Link,
    feed: Feed<Rows>,
}
impl Engine {
    fn start(back: Link, feed: Feed<Rows>) -> Arc<Self> {
        let store = Arc::new(SnapshotStore::new_append_only());
        let mut writer = PublishingStreamingItemStoreWithPayload::new(store.clone());
        writer.publish();
        std::thread::spawn(move || {
            for batch in 0..60u64 {
                for i in batch * 1000..(batch + 1) * 1000 {
                    let line = format!("src/module_{:03}/file_{:05}.rs", i % 173, i);
                    writer.add_item(line.as_bytes(), i);
                }
                writer.publish();
                std::thread::sleep(Duration::from_millis(100));
            }
            writer.complete();
        });
        Arc::new(Self { store, search: Mutex::new(None), rows: AtomicUsize::new(0), back, feed })
    }
    /// The picker's revisions count its queries from none, as the search
    /// counts the ones it is given: the two numbers are the same.
    fn asked(self: &Arc<Self>, asked: Asked) {
        self.rows.store(asked.rows, Ordering::Release);
        let mut search = self.search.lock().unwrap();
        if let Some(search) = &*search {
            search.set_query(asked.query);
            return;
        }
        let engine = self.clone();
        let started = FuzzySearchSession::new_with_snapshot_observer(
            0,
            self.store.clone(),
            asked.query,
            SearchSortMode::Score,
            move |update, _snapshot| {
                let rows = engine.rows.load(Ordering::Acquire);
                let page = Page {
                    version: update.source_version,
                    rows: update
                        .results
                        .into_iter()
                        .take(rows)
                        .map(|result| Row {
                            index: result.node_index,
                            text: result.path,
                            fields: None,
                        })
                        .collect(),
                    matched_against: update.query.clone(),
                    ..Default::default()
                };
                let feed = engine.feed.clone();
                engine.back.send(move || {
                    let (rows, results) = page.into_rows();
                    feed.found(Found {
                        revision: update.generation,
                        query: update.query,
                        source: Arc::new(rows),
                        epoch: 1,
                        append_only: true,
                        results,
                        matched: update.matched,
                        total: update.total,
                        skipped: 0,
                        done: update.source_done,
                    });
                });
            },
        );
        started.start();
        *search = Some(started);
    }
}

struct Demo {
    picker: Option<Picker<Rows>>,
    appearance: Appearance,
    status: String,
    delay: Duration,
    query: String,
}
impl Demo {
    fn new(ctx: &egui::Context) -> Self {
        let argument = |name: &str| {
            let mut args = std::env::args();
            args.find(|arg| arg == name).and_then(|_| args.next())
        };
        let delay = argument("--delay").map_or(150, |ms| ms.parse().expect("--delay MS"));
        let mut demo = Self {
            picker: None,
            appearance: Appearance::default(),
            status: String::new(),
            delay: Duration::from_millis(delay),
            query: argument("--query").unwrap_or_default(),
        };
        demo.open(ctx);
        demo
    }
    fn open(&mut self, ctx: &egui::Context) {
        let (out, back) = (Link::new(self.delay), Link::new(self.delay));
        // The search needs the feed, which there is once the picker is made,
        // and the picker asks for its first query while it is being made.
        let engine: Arc<Mutex<Option<Arc<Engine>>>> = Arc::default();
        let waiting: Arc<Mutex<Vec<Asked>>> = Arc::default();
        let (asked_engine, asked_waiting) = (engine.clone(), waiting.clone());
        let config = PickerConfig { initial_query: self.query.clone(), ..Default::default() };
        let (picker, feed) = Picker::new_fed("fed-demo", ctx, config, move |asked| {
            let Some(engine) = asked_engine.lock().unwrap().clone() else {
                asked_waiting.lock().unwrap().push(asked);
                return;
            };
            out.send(move || engine.asked(asked));
        });
        let started = Engine::start(back, feed);
        *engine.lock().unwrap() = Some(started.clone());
        for asked in waiting.lock().unwrap().drain(..) {
            started.asked(asked);
        }
        self.picker = Some(picker);
        self.status = format!(
            "Searched elsewhere, {} ms each way: 60,000 rows arrive over six seconds.",
            self.delay.as_millis()
        );
    }
}
impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        ui.horizontal(|ui| {
            if ui.button("Open picker").clicked() {
                self.open(ui.ctx());
            }
            ui.label(&self.status);
        });
        let ctx = ui.ctx();
        let mut bounds = ctx.content_rect();
        bounds.min.y += 40.0;
        let Some(picker) = &mut self.picker else { return };
        let output = picker.show(ctx, Placement::new(bounds), &self.appearance, false);
        match output.event {
            Some(PickerEvent::Accepted(selection)) => {
                self.status = format!("Accepted row {}: {}", selection.item, selection.text);
                self.picker = None;
            }
            Some(PickerEvent::Cancelled) => {
                self.status = "Cancelled".into();
                self.picker = None;
            }
            Some(PickerEvent::CopyRequested(selection)) => {
                ctx.copy_text(selection.text);
                picker.notify_copied(ctx);
            }
            Some(PickerEvent::CustomAction { .. }) | None => {}
        }
    }
}
fn main() -> eframe::Result {
    eframe::run_native(
        "NFM picker searched elsewhere",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(Demo::new(&cc.egui_ctx)))),
    )
}
