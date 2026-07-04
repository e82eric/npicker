use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use rust_nfm::ipc::StdInRequest;
#[cfg(feature = "skia")]
use rust_nfm::skia_ui;
use rust_nfm::view_model::ViewModel;
use rust_nfm::{d2d_ui, ipc};

fn output_debug_string(line: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }
}

fn main() -> Result<()> {
    let options = app_options();
    let view_model = ViewModel::new();

    if options.debug_wait {
        debug_wait();
    }

    nfm_search_core::timing::set_sink(output_debug_string);

    if options.stdin {
        run_stdin_request(Arc::clone(&view_model))?;
    } else {
        let server_view_model = Arc::clone(&view_model);
        std::thread::spawn(move || {
            if let Err(error) = ipc::run_pipe_server(server_view_model) {
                eprintln!("pipe server stopped: {error:?}");
            }
        });
    }

    match options.ui {
        UiBackend::D2d => d2d_ui::run(view_model),
        #[cfg(feature = "skia")]
        UiBackend::Skia => skia_ui::run(view_model),
    }
}

fn debug_wait() {
    eprintln!("pid: {}", std::process::id());
    eprintln!("Attach debugger now...");
    std::thread::sleep(std::time::Duration::from_secs(20));
}

fn run_stdin_request(view_model: Arc<ViewModel>) -> Result<()> {
    std::thread::spawn(move || {
        let request = StdInRequest::new(None);

        let code = match view_model.run_request(&request) {
            Ok(response) if response.status == "selected" => {
                if let Some(item) = response.selected_item {
                    let mut stdout = std::io::stdout().lock();
                    if writeln!(stdout, "{item}").is_err() || stdout.flush().is_err() {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            Ok(response) if response.status == "cancelled" => 0,
            Ok(response) => {
                if let Some(message) = response.error_message {
                    eprintln!("{message}");
                }
                1
            }
            Err(error) => {
                eprintln!("{error:?}");
                1
            }
        };
        std::process::exit(code);
    });

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiBackend {
    D2d,
    #[cfg(feature = "skia")]
    Skia,
}

struct AppOptions {
    ui: UiBackend,
    stdin: bool,
    debug_wait: bool,
}

fn app_options() -> AppOptions {
    let mut options = AppOptions {
        ui: UiBackend::D2d,
        stdin: false,
        debug_wait: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--ui" {
            options.ui = match args.next().as_deref() {
                #[cfg(feature = "skia")]
                Some("skia") => UiBackend::Skia,
                Some("d2d") | _ => UiBackend::D2d,
            };
        } else if arg == "--stdin" {
            options.stdin = true;
        } else if arg == "--debug-wait" {
            options.debug_wait = true;
        }
    }

    options
}
