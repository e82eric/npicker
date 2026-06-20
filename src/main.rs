mod d2d_ui;
mod ipc;
mod search;
mod skia_ui;
mod store;
mod timing;
mod view_model;
mod walker;

use std::sync::Arc;

use anyhow::Result;
use view_model::ViewModel;

fn main() -> Result<()> {
    let view_model = Arc::new(ViewModel::new());
    let server_view_model = Arc::clone(&view_model);

    std::thread::spawn(move || {
        if let Err(error) = ipc::run_pipe_server(server_view_model) {
            eprintln!("pipe server stopped: {error:?}");
        }
    });

    match selected_ui() {
        UiBackend::D2d => d2d_ui::run(view_model),
        UiBackend::Skia => skia_ui::run(view_model),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiBackend {
    D2d,
    Skia,
}

fn selected_ui() -> UiBackend {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--ui" {
            return match args.next().as_deref() {
                Some("skia") => UiBackend::Skia,
                Some("d2d") | _ => UiBackend::D2d,
            };
        }
    }

    UiBackend::D2d
}
