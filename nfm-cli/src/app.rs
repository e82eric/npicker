//! The picker window: a stack of pages, each one view of an input kind with
//! its keys bound to actions, and the outcome handed back to `main` when the
//! window closes. An action may push a page on top of the one showing; Esc
//! goes back a page and closes the window from the first.
use crate::{
    actions::{self, ActionState, PickerState, Resolution},
    keys::{self, Chord},
    options::{self, Action, Command, Layer, Options, Program},
    output::{Chosen, Outcome},
    preview::{PreviewSpec, Target},
    source::{self, Feed, LineSnapshot, Repaint, Source},
};
use nfm_egui::{
    Appearance, Layout, PageStack, Picker, PickerConfig, PickerEvent, Placement, Position,
    QueryRole, SearchSortMode, Selection, egui,
};
use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
use nfm_file_system_egui::{FileEvent, FilePaths, FilePicker};
use nfm_picker_sources::{
    delimited::{DelimitedPickerItem, DelimitedStreamingSnapshot},
    records::StreamValues,
    structured::{StructuredPickerItem, StructuredStreamingSnapshot},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

pub type SharedOutcome = Arc<Mutex<Option<Outcome>>>;
pub const DEFAULT_ROWS: usize = 10;
pub const DEFAULT_PREVIEW_ROWS: usize = 12;
pub const DEFAULT_WIDTH: f32 = 1000.0;
const ACCEPT_RESOLVER: &str = "__nfm_accept";

#[derive(Clone, Debug, PartialEq)]
enum Binding {
    Accept,
    Cancel,
    Parent,
    Action(String),
}

enum Shown {
    Nothing,
    Accepted(Option<Chosen>),
    Cancelled,
}

enum View {
    Lines {
        picker: Picker<LineSnapshot>,
        feed: Feed<LineSnapshot>,
        values: StreamValues,
    },
    Delimited {
        picker: Picker<DelimitedStreamingSnapshot>,
        feed: Feed<DelimitedStreamingSnapshot>,
    },
    Csv {
        picker: Picker<StructuredStreamingSnapshot>,
        feed: Feed<StructuredStreamingSnapshot>,
    },
    Files(Box<FilePicker>),
    #[cfg(windows)]
    Processes(Box<nfm_process_egui::ProcessPicker>),
    #[cfg(windows)]
    Windows(Box<nfm_window_egui::WindowPicker>),
    /// Windows with a command preview instead of the live thumbnail.
    #[cfg(windows)]
    WindowList(Picker<nfm_window_egui::WindowSnapshot>),
    /// Nothing to pick from: the query box takes text as typed.
    Argument(Picker<FlatSnapshot<()>>),
}

/// Run `$body` with `$p` bound to the view's `&mut Picker<_>`.
macro_rules! with_picker {
    ($view:expr, $p:ident => $body:expr) => {
        match $view {
            View::Lines { picker: $p, .. } => $body,
            View::Delimited { picker: $p, .. } => $body,
            View::Csv { picker: $p, .. } => $body,
            View::Files(menu) => {
                let $p = menu.picker_mut();
                $body
            }
            #[cfg(windows)]
            View::Processes(menu) => {
                let $p = menu.picker_mut();
                $body
            }
            #[cfg(windows)]
            View::Windows(menu) => {
                let $p = menu.picker_mut();
                $body
            }
            #[cfg(windows)]
            View::WindowList($p) => $body,
            View::Argument($p) => $body,
        }
    };
}

fn item_json<T: serde::Serialize>(item: &T) -> serde_json::Value {
    serde_json::to_value(item).unwrap_or(serde_json::Value::Null)
}
fn line_chosen(values: &StreamValues, index: usize, text: String) -> Chosen {
    let value = values
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&index)
        .cloned()
        .unwrap_or_else(|| text.clone());
    Chosen {
        item: value.clone().into(),
        value,
        text,
        index,
    }
}
fn delimited_chosen(item: &DelimitedPickerItem, text: String, index: usize) -> Chosen {
    Chosen {
        value: item.value.clone(),
        text,
        index,
        item: item_json(item),
    }
}
fn csv_chosen(item: &StructuredPickerItem, text: String, index: usize) -> Chosen {
    Chosen {
        value: item.value.clone(),
        text,
        index,
        item: item_json(item),
    }
}
fn path_chosen(path: String, index: usize) -> Chosen {
    Chosen {
        item: path.clone().into(),
        text: path.clone(),
        value: path,
        index,
    }
}
/// What an argument page selects: the text as typed.
fn argument_chosen(text: String) -> Chosen {
    Chosen {
        item: text.clone().into(),
        text: text.clone(),
        value: text,
        index: 0,
    }
}
#[cfg(windows)]
fn window_chosen(item: &nfm_win32::WindowPickerItem, index: usize) -> Chosen {
    Chosen {
        value: item.title.clone(),
        text: item.title.clone(),
        index,
        item: item_json(item),
    }
}

/// The candidate at `index` of a snapshot, for `--filter` and `--select-1`.
pub fn chosen_at(source: &Source, index: usize) -> Option<Chosen> {
    use nfm_search_core::store::ItemsSource;
    let text = |s: &dyn Fn(&mut Vec<u8>) -> String| s(&mut Vec::new());
    match source {
        Source::Lines(feed, values) => {
            let snapshot = feed.store.snapshot()?;
            let line = text(&|buffer| snapshot.get_string_lossy(index, buffer));
            Some(line_chosen(values, index, line))
        }
        Source::Delimited(feed) => {
            let snapshot = feed.store.snapshot()?;
            let item = snapshot.item(index)?;
            let line = text(&|buffer| snapshot.get_string_lossy(index, buffer));
            Some(delimited_chosen(&item, line, index))
        }
        Source::Csv(feed) => {
            let snapshot = feed.store.snapshot()?;
            let item = snapshot.item(index)?;
            let line = text(&|buffer| snapshot.get_string_lossy(index, buffer));
            Some(csv_chosen(&item, line, index))
        }
    }
}

impl View {
    fn chosen(&mut self) -> Option<Chosen> {
        match self {
            View::Lines { picker, values, .. } => picker
                .selection()
                .map(|s| line_chosen(values, s.index, s.text)),
            View::Delimited { picker, .. } => picker
                .selection()
                .map(|s| delimited_chosen(&s.item, s.text, s.index)),
            View::Csv { picker, .. } => picker
                .selection()
                .map(|s| csv_chosen(&s.item, s.text, s.index)),
            View::Files(menu) => menu
                .picker()
                .selection()
                .map(|s| path_chosen(s.item, s.index)),
            #[cfg(windows)]
            View::Processes(menu) => menu.picker().selection().map(|s| Chosen {
                value: s.item.value.clone(),
                text: s.text,
                index: s.index,
                item: item_json(&s.item),
            }),
            #[cfg(windows)]
            View::Windows(menu) => menu
                .picker()
                .selection()
                .map(|s| window_chosen(&s.item, s.index)),
            #[cfg(windows)]
            View::WindowList(picker) => picker
                .selection()
                .map(|s| window_chosen(&s.item, s.index)),
            View::Argument(picker) => Some(argument_chosen(picker.query().to_owned())),
        }
    }

    fn state(&mut self) -> PickerState {
        match self {
            View::Lines { .. } => PickerState::Stdin,
            View::Delimited { .. } => PickerState::DelimitedStdin,
            View::Csv { .. } => PickerState::StructuredStdin,
            View::Files(menu) => PickerState::Filewalker {
                roots: menu.options().roots.clone(),
            },
            #[cfg(windows)]
            View::Processes(_) => PickerState::Processes,
            #[cfg(windows)]
            View::Windows(_) | View::WindowList(_) => PickerState::Windows,
            View::Argument(_) => PickerState::Argument,
        }
    }

    fn source_error(&self) -> Option<String> {
        match self {
            View::Lines { feed, .. } => feed.error(),
            View::Delimited { feed, .. } => feed.error(),
            View::Csv { feed, .. } => feed.error(),
            _ => None,
        }
    }

    fn show(
        &mut self,
        ctx: &egui::Context,
        placement: Placement,
        appearance: &Appearance,
    ) -> Result<Shown, String> {
        fn plain<T>(
            ctx: &egui::Context,
            picker: &mut Picker<impl nfm_egui::ItemsSource<Item = T> + Send + Sync + 'static>,
            placement: Placement,
            appearance: &Appearance,
            chosen: impl Fn(Selection<T>) -> Chosen,
        ) -> Shown
        where
            T: Clone + Send + 'static,
        {
            match picker.show(ctx, placement, appearance, false).event {
                Some(PickerEvent::Accepted(selection)) => Shown::Accepted(Some(chosen(selection))),
                Some(PickerEvent::Cancelled) => Shown::Cancelled,
                Some(PickerEvent::CopyRequested(selection)) => {
                    let value = chosen(selection).value;
                    picker.notify_copied_value(ctx, &value);
                    ctx.copy_text(value);
                    Shown::Nothing
                }
                Some(PickerEvent::CustomAction { .. } | PickerEvent::Submitted(_)) | None => Shown::Nothing,
            }
        }
        Ok(match self {
            View::Lines { picker, values, .. } => {
                let values = values.clone();
                plain(ctx, picker, placement, appearance, move |s| {
                    line_chosen(&values, s.index, s.text)
                })
            }
            View::Delimited { picker, .. } => plain(ctx, picker, placement, appearance, |s| {
                delimited_chosen(&s.item, s.text, s.index)
            }),
            View::Csv { picker, .. } => plain(ctx, picker, placement, appearance, |s| {
                csv_chosen(&s.item, s.text, s.index)
            }),
            View::Files(menu) => {
                let index = menu.picker().selection().map_or(0, |s| s.index);
                match menu.show(ctx, placement, appearance)? {
                    Some(FileEvent::Selected(path)) => Shown::Accepted(Some(path_chosen(path, index))),
                    Some(FileEvent::Cancelled) => Shown::Cancelled,
                    None => Shown::Nothing,
                }
            }
            #[cfg(windows)]
            View::Processes(menu) => match menu.show_event(ctx, placement, appearance) {
                Some(PickerEvent::Accepted(s)) => Shown::Accepted(Some(Chosen {
                    value: s.item.value.clone(),
                    text: s.text,
                    index: s.index,
                    item: item_json(&s.item),
                })),
                Some(PickerEvent::Cancelled) => Shown::Cancelled,
                _ => Shown::Nothing,
            },
            #[cfg(windows)]
            View::Windows(menu) => match menu.show(ctx, placement, appearance) {
                Some(nfm_window_egui::WindowEvent::Selected(hwnd)) => {
                    let chosen = menu
                        .picker()
                        .selection()
                        .filter(|s| s.item.native_window == hwnd)
                        .map(|s| window_chosen(&s.item, s.index));
                    Shown::Accepted(chosen.or_else(|| {
                        Some(window_chosen(
                            &nfm_win32::WindowPickerItem {
                                title: String::new(),
                                native_window: hwnd,
                            },
                            0,
                        ))
                    }))
                }
                Some(nfm_window_egui::WindowEvent::Cancelled) => Shown::Cancelled,
                None => Shown::Nothing,
            },
            #[cfg(windows)]
            View::WindowList(picker) => plain(ctx, picker, placement, appearance, |s| {
                window_chosen(&s.item, s.index)
            }),
            View::Argument(picker) => match picker.show(ctx, placement, appearance, false).event {
                Some(PickerEvent::Submitted(text)) => Shown::Accepted(Some(argument_chosen(text))),
                Some(PickerEvent::Cancelled) => Shown::Cancelled,
                _ => Shown::Nothing,
            },
        })
    }
}

struct Running {
    receive: mpsc::Receiver<Result<Resolution, String>>,
    cancel: Arc<AtomicBool>,
    chosen: Option<Chosen>,
    query: String,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// One page of the window: a picker over one input, with its own options,
/// keys and running action. A page beneath the top one keeps all of it, so
/// going back shows it as it was left.
struct Page {
    options: Arc<Options>,
    view: View,
    bindings: Vec<(Chord, Binding)>,
    expect: Vec<Chord>,
    running: Option<Running>,
    preview: PreviewSpec,
    source_error_shown: bool,
}

pub struct App {
    /// The first page's options: the window's font, colors and position.
    root: Arc<Options>,
    /// The defaults under every page's own command line.
    base: Vec<Layer>,
    repaint: Repaint,
    appearance: Appearance,
    pages: PageStack<Page>,
    outcome: SharedOutcome,
    hwnd: isize,
    /// The size asked of the window for the page showing, until it has it.
    resizing: Option<egui::Vec2>,
    first_frame: bool,
}

pub fn picker_config(options: &Options, preview_visible: bool) -> PickerConfig {
    let defaults = PickerConfig::default();
    PickerConfig {
        layout: if options.layout_top {
            Layout::QueryTop
        } else {
            Layout::QueryBottom
        },
        title: options.title.clone().unwrap_or_else(|| "nfm".into()),
        initial_query: options.query.clone(),
        result_rows: options.rows.unwrap_or(DEFAULT_ROWS),
        accept_on_click: false,
        bindings: Vec::new(),
        preview_rows: options.preview_rows.unwrap_or(DEFAULT_PREVIEW_ROWS),
        preview_visible,
        preview_wrap: options.preview_wrap,
        sort: if options.sort_input {
            SearchSortMode::SourceOrder
        } else {
            SearchSortMode::Score
        },
        query_role: if options.command == Command::Argument {
            QueryRole::Argument
        } else {
            QueryRole::Filter
        },
        prompt: options.prompt.clone().unwrap_or(defaults.prompt.clone()),
        placeholder: options.placeholder.clone().unwrap_or_default(),
        ..defaults
    }
}

/// Whether the picker opens with its preview pane.
pub fn preview_initially_visible(options: &Options) -> bool {
    let spec = PreviewSpec::new(options);
    let available = match options.command {
        Command::FileSystem(_) => true,
        Command::Processes => true,
        Command::Windows => true,
        Command::Argument => false,
        _ => !spec.is_empty(),
    };
    let default = !matches!(options.command, Command::Processes) || spec.has_command();
    available && options.preview_visible.unwrap_or(default)
}

/// The window's size in points for a page with these options.
pub fn window_size(options: &Options) -> egui::Vec2 {
    // Picker rows are the font's line height plus 3 points of padding on each
    // side; the panel frame, query and dividers add a fixed overhead.
    let font_size = options.font_size.unwrap_or(14.0);
    let row = (font_size * 1.2 + 6.0).ceil();
    let rows = options.rows.unwrap_or(DEFAULT_ROWS) as f32;
    // Cards (--row-template) are a line per template line, a larger title
    // line, 6 points of padding above and below, and 1 point between lines.
    let result = options.row_template.as_deref().map_or(row, |spec| {
        let lines = spec.split(';').filter(|line| !line.trim().is_empty());
        let line = font_size * 1.2;
        let height: f32 = lines
            .map(|l| {
                if l.contains(":title") {
                    (line * 1.15).ceil() + 1.0
                } else {
                    line + 1.0
                }
            })
            .sum();
        (height - 1.0 + 12.0).ceil()
    });
    let (preview, overhead) = if preview_initially_visible(options) {
        let rows = options.preview_rows.unwrap_or(DEFAULT_PREVIEW_ROWS) as f32;
        (rows * row, 58.0)
    } else {
        (0.0, 43.0)
    };
    // Tables (processes, windows, csv rows without cards) add a header row.
    let header = match options.command {
        Command::Processes | Command::Windows => row,
        _ if options.input_format == options::InputFormat::Csv && options.row_template.is_none() => {
            row
        }
        _ => 0.0,
    };
    let height = options
        .height
        .unwrap_or(row + header + rows * result + preview + overhead);
    egui::vec2(options.width.unwrap_or(DEFAULT_WIDTH), height)
}

/// `args` with `{query}` and `{item}` replaced: the command line of the page
/// a push action shows.
fn fill(args: &[String], query: &str, item: Option<&str>) -> Result<Vec<String>, String> {
    args.iter()
        .map(|arg| {
            let arg = arg.replace("{query}", query);
            if !arg.contains("{item}") {
                return Ok(arg);
            }
            item.map(|item| arg.replace("{item}", item))
                .ok_or_else(|| "{item} requires a selection".to_owned())
        })
        .collect()
}

impl Page {
    fn new(
        ctx: &egui::Context,
        options: Arc<Options>,
        source: Option<Source>,
        hwnd: isize,
    ) -> Result<Self, String> {
        let preview = PreviewSpec::new(&options);
        let view = build_view(ctx, &options, source, hwnd, &preview)?;
        let mut bindings: Vec<(Chord, Binding)> = options
            .bindings
            .iter()
            .map(|(chord, action)| {
                let binding = match action.as_str() {
                    "accept" => Binding::Accept,
                    "cancel" => Binding::Cancel,
                    "parent" => Binding::Parent,
                    name => Binding::Action(name.to_owned()),
                };
                (chord.clone(), binding)
            })
            .collect();
        if options.accept_resolver.is_some() {
            let enter = keys::parse("enter").expect("enter is a key");
            if !bindings.iter().any(|(chord, _)| *chord == enter) {
                bindings.push((enter, Binding::Action(ACCEPT_RESOLVER.into())));
            }
        }
        keys::sort_specific_first(&mut bindings);
        let mut page = Self {
            expect: options.expect.clone(),
            options,
            view,
            bindings,
            running: None,
            preview,
            source_error_shown: false,
        };
        page.install_help();
        Ok(page)
    }

    fn install_help(&mut self) {
        let mut help: Vec<(String, String)> = match &self.view {
            View::Files(_) => vec![("Ctrl+U".into(), "Parent directory".into())],
            #[cfg(windows)]
            View::Processes(_) => vec![
                ("Ctrl+R".into(), "Refresh processes".into()),
                ("Ctrl+K".into(), "Kill selected process".into()),
            ],
            _ => Vec::new(),
        };
        for (chord, binding) in &self.bindings {
            let description = match binding {
                Binding::Accept => "Accept".to_owned(),
                Binding::Cancel => "Cancel".to_owned(),
                Binding::Parent => "Parent directory".to_owned(),
                Binding::Action(name) if name == ACCEPT_RESOLVER => "Accept (resolver)".to_owned(),
                Binding::Action(name) => format!("Run {name}"),
            };
            help.push((chord.label(), description));
        }
        for chord in &self.expect {
            help.push((chord.label(), "Accept and print the key".into()));
        }
        with_picker!(&mut self.view, picker => picker.set_keybinding_help(help));
    }

    fn query(&mut self) -> String {
        with_picker!(&mut self.view, picker => picker.query().to_owned())
    }

    fn set_error(&mut self, error: Option<String>) {
        with_picker!(&mut self.view, picker => picker.set_error(error));
    }

    fn start_action(&mut self, ctx: &egui::Context, program: Program) {
        let chosen = self.view.chosen();
        let query = self.query();
        let state = ActionState {
            selection: chosen.as_ref().map(|c| c.item.clone()),
            picker: self.view.state(),
            query: query.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (send, receive) = mpsc::channel();
        let value = chosen.as_ref().map(|c| c.value.clone());
        let (worker_cancel, repaint) = (cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = actions::run(&program, &state, value.as_deref(), &worker_cancel);
            let _ = send.send(result);
            repaint.request_repaint();
        });
        // A newer action replaces (and cancels) one still running.
        self.running = Some(Running {
            receive,
            cancel,
            chosen,
            query,
        });
    }
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        options: Arc<Options>,
        source: Option<Source>,
        outcome: SharedOutcome,
        base: Vec<Layer>,
        repaint: Repaint,
    ) -> Result<Self, String> {
        let ctx = &cc.egui_ctx;
        let mut appearance = crate::theme::appearance(&options.colors, options.font_size);
        if let Some(font) = &options.font {
            crate::theme::install_font(ctx, &mut appearance, font)?;
        }
        let hwnd = window_handle(cc);
        #[cfg(windows)]
        {
            crate::window::make_popup(hwnd);
            crate::window::place(hwnd, options.position);
        }
        let mut pages = PageStack::new();
        pages.open(Page::new(ctx, options.clone(), source, hwnd)?);
        Ok(Self {
            root: options,
            base,
            repaint,
            appearance,
            pages,
            outcome,
            hwnd,
            resizing: None,
            first_frame: true,
        })
    }

    /// The page showing.
    fn page(&mut self) -> &mut Page {
        self.pages.top_mut().expect("the window always has a page")
    }

    fn finished(&self) -> bool {
        self.outcome.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn finish(&mut self, ctx: &egui::Context, outcome: Outcome) {
        #[cfg(windows)]
        if self.page().options.activate
            && let Outcome::Selected {
                chosen: Some(chosen),
                ..
            } = &outcome
            && let Some(hwnd) = chosen.item.get("native_window").and_then(|v| v.as_i64())
        {
            crate::window::activate(hwnd as isize);
        }
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Show the page `args`, an nfm command line, describes on top of the one
    /// showing.
    fn push(&mut self, ctx: &egui::Context, args: Vec<String>) -> Result<(), String> {
        let mut options = options::parse_page(&self.base, args)?;
        // The font, colors and position are the window's, set by the first page.
        options.font = self.root.font.clone();
        options.font_size = self.root.font_size;
        options.colors = self.root.colors.clone();
        options.position = self.root.position;
        options.width = options.width.or(self.root.width);
        let source = if options.command.is_line_input() {
            Some(source::start(&options, self.repaint.clone())?)
        } else {
            None
        };
        let page = Page::new(ctx, Arc::new(options), source, self.hwnd)?;
        self.pages.push(page);
        self.page_changed(ctx);
        Ok(())
    }

    /// Go back a page; false from the first one.
    fn back(&mut self, ctx: &egui::Context) -> bool {
        let went = self.pages.back();
        if went {
            self.page_changed(ctx);
        }
        went
    }

    /// Another page is showing: the window takes its size and title.
    fn page_changed(&mut self, ctx: &egui::Context) {
        let options = self.page().options.clone();
        let size = window_size(&options);
        if (ctx.content_rect().size() - size).length() >= 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            self.resizing = Some(size);
        } else {
            self.resizing = None;
        }
        let title = options.title.clone().unwrap_or_else(|| "nfm".into());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        ctx.request_repaint();
    }

    fn run(&mut self, ctx: &egui::Context, binding: Binding) -> Result<(), String> {
        match binding {
            Binding::Accept => {
                let page = self.page();
                if let Some(chosen) = page.view.chosen() {
                    let query = page.query();
                    self.finish(
                        ctx,
                        Outcome::Selected {
                            chosen: Some(chosen),
                            key: None,
                            query,
                        },
                    );
                }
            }
            Binding::Cancel => self.finish(ctx, Outcome::Cancelled),
            Binding::Parent => {
                if let View::Files(menu) = &mut self.page().view {
                    let roots = nfm_file_system::parent_roots(
                        &menu.options().roots,
                        nfm_file_system::default_roots(),
                    );
                    menu.navigate(ctx, roots)?;
                }
            }
            Binding::Action(name) => {
                let page = self.page();
                let action = if name == ACCEPT_RESOLVER {
                    page.options.accept_resolver.clone().map(Action::Resolver)
                } else {
                    page.options.actions.get(&name).cloned()
                }
                .ok_or_else(|| format!("undefined action '{name}'"))?;
                match action {
                    Action::Resolver(program) => page.start_action(ctx, program),
                    Action::Push(args) => {
                        let chosen = page.view.chosen();
                        let query = page.query();
                        let args = fill(&args, &query, chosen.as_ref().map(|c| c.value.as_str()))?;
                        self.push(ctx, args)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn poll_action(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let hwnd = self.hwnd;
        let page = self.page();
        let Some(running) = &page.running else {
            return Ok(());
        };
        let result = match running.receive.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => Err("action resolver stopped".into()),
        };
        let running = page.running.take().expect("checked above");
        match result? {
            Resolution::None => {}
            Resolution::Complete => {
                let chosen = running.chosen.clone();
                let query = running.query.clone();
                self.finish(
                    ctx,
                    Outcome::Selected {
                        chosen,
                        key: None,
                        query,
                    },
                );
            }
            Resolution::Filewalker(roots) => {
                if let View::Files(menu) = &mut page.view {
                    menu.navigate(ctx, roots)?;
                } else {
                    let mut options = (*page.options).clone();
                    options.command = Command::FileSystem(roots);
                    options.query.clear();
                    page.view = build_view(ctx, &options, None, hwnd, &page.preview)?;
                    page.install_help();
                }
            }
            Resolution::Push(args) => self.push(ctx, args)?,
        }
        Ok(())
    }

    fn frame(&mut self, ctx: &egui::Context) -> Result<(), String> {
        if std::mem::take(&mut self.first_frame) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Once the window has the size a page asked for, it goes back to
        // where windows of that size open.
        if let Some(size) = self.resizing
            && (ctx.content_rect().size() - size).length() < 1.0
        {
            self.resizing = None;
            #[cfg(windows)]
            crate::window::place_again(self.hwnd, self.root.position);
        }
        self.poll_action(ctx)?;
        if self.finished() {
            return Ok(());
        }
        let page = self.page();
        if !page.source_error_shown
            && let Some(error) = page.view.source_error()
        {
            page.source_error_shown = true;
            page.set_error(Some(error));
        }
        let (focused, help) =
            with_picker!(&mut page.view, picker => (picker.preview_focused(), picker.keybinding_help_visible()));
        if !focused && !help {
            if let Some(chord) = page.expect.iter().find(|chord| chord.consume(ctx)).cloned() {
                let chosen = page.view.chosen();
                let query = page.query();
                self.finish(
                    ctx,
                    Outcome::Selected {
                        chosen,
                        key: Some(chord.name),
                        query,
                    },
                );
                return Ok(());
            }
            let pressed = page
                .bindings
                .iter()
                .find(|(chord, _)| chord.consume(ctx))
                .map(|(_, binding)| binding.clone());
            if let Some(binding) = pressed {
                self.run(ctx, binding)?;
                if self.finished() {
                    return Ok(());
                }
            }
        }
        let mut placement = Placement::new(ctx.content_rect());
        placement.position = Position::FullPane;
        placement.margin = 0.0;
        let page = self.pages.top_mut().expect("the window always has a page");
        match page.view.show(ctx, placement, &self.appearance)? {
            Shown::Nothing => {}
            // Esc goes back a page, and closes the window from the first.
            Shown::Cancelled => {
                if !self.back(ctx) {
                    self.finish(ctx, Outcome::Cancelled);
                }
            }
            Shown::Accepted(chosen) => {
                let query = self.page().query();
                self.finish(
                    ctx,
                    Outcome::Selected {
                        chosen,
                        key: None,
                        query,
                    },
                );
            }
        }
        Ok(())
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Err(error) = self.frame(&ctx) {
            self.page().set_error(Some(error));
        }
    }
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.appearance.palette.background.to_normalized_gamma_f32()
    }
}

fn window_handle(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    cc.window_handle()
        .ok()
        .and_then(|handle| match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        })
        .unwrap_or(0)
}

fn build_view(
    ctx: &egui::Context,
    options: &Options,
    source: Option<Source>,
    hwnd: isize,
    preview: &PreviewSpec,
) -> Result<View, String> {
    let visible = preview_initially_visible(options);
    let config = picker_config(options, visible);
    let view = match (&options.command, source) {
        (Command::FileSystem(roots), _) => {
            let fs = nfm_file_system::FileSystemPickerOptions {
                roots: roots.clone(),
                max_depth: options
                    .max_depth
                    .map_or(i32::MAX, |depth| depth.min(i32::MAX as usize) as i32),
                directories_only: options.directories_only,
                files_only: options.files_only,
                search_string: Some(options.query.clone()).filter(|q| !q.is_empty()),
                preview_visible: visible,
            };
            let mut menu = FilePicker::with_picker(ctx, fs, config, Arc::new(FilePaths), None)?;
            if preview.has_command() {
                menu.picker_mut().set_preview_provider(
                    ctx,
                    preview.provider(|s: &Selection<String>| {
                        Some(Target {
                            item: s.item.clone(),
                            line: None,
                            file: Some(s.item.clone()),
                        })
                    }),
                );
            }
            View::Files(Box::new(menu))
        }
        #[cfg(windows)]
        (Command::Processes, _) => {
            let mut menu = nfm_process_egui::ProcessPicker::with_picker(
                ctx,
                config,
                Arc::new(nfm_process_egui::ProcessItems),
                None,
            );
            if preview.has_command() {
                menu.picker_mut().set_preview_provider(
                    ctx,
                    preview.provider(|s: &Selection<nfm_win32::ProcessPickerItem>| {
                        Some(Target {
                            item: s.item.value.clone(),
                            ..Target::default()
                        })
                    }),
                );
            }
            View::Processes(Box::new(menu))
        }
        #[cfg(windows)]
        (Command::Windows, _) if preview.has_command() && !options.window_preview => {
            use nfm_search_core::snapshot_store::SnapshotStore;
            let windows = nfm_win32::list_windows::list_windows().map_err(|e| e.to_string())?;
            let store = Arc::new(SnapshotStore::new());
            store.publish(Arc::new(nfm_window_egui::window_snapshot(windows, hwnd)));
            store.complete();
            let mut picker = Picker::new("nfm-window-list", ctx, store, config);
            picker.set_preview_provider(
                ctx,
                preview.provider(|s: &Selection<nfm_win32::WindowPickerItem>| {
                    Some(Target {
                        item: s.item.title.clone(),
                        ..Target::default()
                    })
                }),
            );
            View::WindowList(picker)
        }
        #[cfg(windows)]
        (Command::Windows, _) => View::Windows(Box::new(nfm_window_egui::WindowPicker::with_picker(
            ctx,
            hwnd,
            config,
            Arc::new(nfm_window_egui::WindowItems),
            None,
        )?)),
        #[cfg(not(windows))]
        (Command::Processes | Command::Windows, _) => {
            return Err("the processes and windows commands are only available on Windows".into());
        }
        (Command::Argument, _) => {
            // Nothing to pick from: the page is its query box.
            let store = Arc::new(SnapshotStore::new());
            store.publish(Arc::new(FlatSnapshot::from_items(std::iter::empty::<(&str, ())>())));
            store.complete();
            View::Argument(Picker::new("nfm-argument", ctx, store, config))
        }
        (_, Some(Source::Lines(feed, values))) => {
            let mut picker = Picker::new("nfm-lines", ctx, feed.store.clone(), config);
            if !preview.is_empty() {
                let lookup = values.clone();
                picker.set_preview_provider(
                    ctx,
                    preview.provider(move |s: &Selection<u64>| {
                        Some(Target {
                            item: line_chosen(&lookup, s.index, s.text.clone()).value,
                            ..Target::default()
                        })
                    }),
                );
            }
            View::Lines {
                picker,
                feed,
                values,
            }
        }
        (_, Some(Source::Delimited(feed))) => {
            let mut picker = Picker::new("nfm-delimited", ctx, feed.store.clone(), config);
            if !preview.is_empty() {
                picker.set_preview_provider(
                    ctx,
                    preview.provider(|s: &Selection<DelimitedPickerItem>| {
                        let item = &s.item;
                        Some(Target {
                            item: item.preview_item.clone().unwrap_or_else(|| item.value.clone()),
                            line: item.preview_center_line,
                            file: item.preview_item.clone(),
                        })
                    }),
                );
            }
            View::Delimited { picker, feed }
        }
        (_, Some(Source::Csv(feed))) => {
            let mut picker = Picker::new("nfm-csv", ctx, feed.store.clone(), config);
            if !preview.is_empty() {
                picker.set_preview_provider(
                    ctx,
                    preview.provider(|s: &Selection<StructuredPickerItem>| {
                        Some(Target {
                            item: s.item.value.clone(),
                            ..Target::default()
                        })
                    }),
                );
            }
            View::Csv { picker, feed }
        }
        (_, None) => return Err("line input has no source".into()),
    };
    Ok(view)
}
