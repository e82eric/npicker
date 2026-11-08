//! `nfm`: a fuzzy picker window for scripts. Reads candidates from standard
//! input, a file, a command, the file system, processes or windows, and prints
//! the selection.
mod actions;
mod app;
mod config;
mod keys;
mod options;
mod output;
mod preview;
mod source;
mod theme;
#[cfg(windows)]
mod window;

use nfm_egui::{SearchSortMode, egui};
use nfm_search_core::{search::search_with_sort, store::ItemsSource};
use options::{Layer, Options, Parsed};
use output::{EXIT_ERROR, EXIT_NO_MATCH, Outcome};
use source::{Feed, Source};
use std::sync::{Arc, OnceLock};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut layers = match config::layers(&argv) {
        Ok(layers) => layers,
        Err(error) => return fail(&error),
    };
    layers.push(Layer {
        origin: "command line".into(),
        args: argv,
        positionals: true,
    });
    let options = match options::parse(&layers) {
        Ok(Parsed::Help) => {
            print!("{}", options::HELP);
            return 0;
        }
        Ok(Parsed::Version) => {
            println!("nfm {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Ok(Parsed::Run(options)) => Arc::new(*options),
        Err(error) => {
            eprintln!("nfm: {error}\nRun 'nfm --help' for usage.");
            return EXIT_ERROR;
        }
    };
    if options.debug_wait {
        eprintln!("pid: {}", std::process::id());
        eprintln!("Attach debugger now...");
        std::thread::sleep(std::time::Duration::from_secs(20));
    }

    let context: Arc<OnceLock<egui::Context>> = Arc::default();
    let repaint: source::Repaint = {
        let context = context.clone();
        Arc::new(move || {
            if let Some(ctx) = context.get() {
                ctx.request_repaint();
            }
        })
    };
    let source = if options.command.is_line_input() {
        match source::start(&options, repaint) {
            Ok(source) => Some(source),
            Err(error) => return fail(&error),
        }
    } else {
        None
    };

    let sort = if options.sort_input {
        SearchSortMode::SourceOrder
    } else {
        SearchSortMode::Score
    };
    if let Some(source) = &source {
        if let Some(query) = &options.filter {
            return match matches(source, query, sort) {
                Ok((indices, _)) => {
                    let chosen: Vec<_> = indices
                        .into_iter()
                        .filter_map(|index| app::chosen_at(source, index))
                        .collect();
                    output::print_matches(&chosen, query, &options)
                }
                Err(error) => fail(&error),
            };
        }
        if options.select_1 || options.exit_0 {
            let (indices, matched) = match matches(source, &options.query, sort) {
                Ok(found) => found,
                Err(error) => return fail(&error),
            };
            if matched == 0 && options.exit_0 {
                return EXIT_NO_MATCH;
            }
            if matched == 1 && options.select_1 {
                let chosen = indices.first().and_then(|&i| app::chosen_at(source, i));
                let outcome = Outcome::Selected {
                    chosen,
                    key: None,
                    query: options.query.clone(),
                };
                return output::finish(&outcome, &options);
            }
        }
    }

    let outcome: app::SharedOutcome = Arc::default();
    let result = {
        let (options, outcome) = (options.clone(), outcome.clone());
        eframe::run_native(
            &options.title.clone().unwrap_or_else(|| "nfm".into()),
            native_options(&options),
            Box::new(move |cc| {
                let _ = context.set(cc.egui_ctx.clone());
                app::App::new(cc, options, source, outcome)
                    .map(|app| Box::new(app) as Box<dyn eframe::App>)
                    .map_err(Into::into)
            }),
        )
    };
    if let Err(error) = result {
        return fail(&error.to_string());
    }
    let outcome = outcome
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or(Outcome::Cancelled);
    output::finish(&outcome, &options)
}

fn fail(error: &str) -> i32 {
    eprintln!("nfm: {error}");
    EXIT_ERROR
}

/// Wait for the whole input, then search it: matching indices (at most
/// `RESULT_LIMIT`, best first) and the total number of matches.
fn matches(source: &Source, query: &str, sort: SearchSortMode) -> Result<(Vec<usize>, usize), String> {
    fn search<S: ItemsSource + Send + Sync>(
        feed: &Feed<S>,
        query: &str,
        sort: SearchSortMode,
    ) -> Result<(Vec<usize>, usize), String> {
        feed.wait();
        if let Some(error) = feed.error() {
            return Err(error);
        }
        let Some(snapshot) = feed.store.snapshot() else {
            return Ok((Vec::new(), 0));
        };
        let output = search_with_sort(snapshot, query, sort, || false).unwrap_or_default();
        Ok((
            output.results.iter().map(|r| r.node_index).collect(),
            output.matched,
        ))
    }
    match source {
        Source::Lines(feed, _) => search(feed, query, sort),
        Source::Delimited(feed) => search(feed, query, sort),
        Source::Csv(feed) => search(feed, query, sort),
    }
}

fn native_options(options: &Options) -> eframe::NativeOptions {
    // Picker rows are the font's line height plus 3 points of padding on each
    // side; the panel frame, query and dividers add a fixed overhead.
    let font_size = options.font_size.unwrap_or(14.0);
    let row = (font_size * 1.2 + 6.0).ceil();
    let rows = options.rows.unwrap_or(app::DEFAULT_ROWS) as f32;
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
    let (preview, overhead) = if app::preview_initially_visible(options) {
        let rows = options.preview_rows.unwrap_or(app::DEFAULT_PREVIEW_ROWS) as f32;
        (rows * row, 58.0)
    } else {
        (0.0, 43.0)
    };
    // Tables (processes, windows, csv rows without cards) add a header row.
    let header = match options.command {
        options::Command::Processes | options::Command::Windows => row,
        _ if options.input_format == options::InputFormat::Csv
            && options.row_template.is_none() =>
        {
            row
        }
        _ => 0.0,
    };
    let height = options
        .height
        .unwrap_or(row + header + rows * result + preview + overhead);
    let width = options.width.unwrap_or(app::DEFAULT_WIDTH);
    eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(options.title.clone().unwrap_or_else(|| "nfm".into()))
            .with_inner_size([width, height])
            .with_min_inner_size([200.0, 80.0])
            .with_decorations(false)
            .with_always_on_top()
            // A popup: no resize border or taskbar button (WS_EX_APPWINDOW),
            // so tiling window managers float it.
            .with_resizable(false)
            .with_taskbar(false),
        // Windows builds place the window on the monitor under the mouse.
        centered: !cfg!(windows),
        ..Default::default()
    }
}
