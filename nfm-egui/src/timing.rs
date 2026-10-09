//! Opt-in picker UI diagnostics; no queries, item text, or paths are logged.
use std::{
    io::Write,
    sync::{OnceLock, mpsc},
    time::Instant,
};
struct Trace {
    started: Instant,
    sender: mpsc::SyncSender<String>,
}
static TRACE: OnceLock<Option<Trace>> = OnceLock::new();
fn trace() -> Option<&'static Trace> {
    TRACE
        .get_or_init(|| {
            let path = std::env::var_os("NFM_PICKER_TRACE")?;
            let mut file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => file,
                Err(error) => {
                    eprintln!("Cannot open NFM picker trace: {error}");
                    return None;
                }
            };
            let started = Instant::now();
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis();
            let (sender, receiver) = mpsc::sync_channel::<String>(4096);
            std::thread::Builder::new()
                .name("nfm-picker-trace".into())
                .spawn(move || {
                    let _ = writeln!(
                        file,
                        "nfm_picker_trace pid={} version=1 unix_ms={unix_ms}",
                        std::process::id()
                    );
                    for line in receiver {
                        let _ = writeln!(file, "{line}");
                    }
                })
                .ok()?;
            Some(Trace { started, sender })
        })
        .as_ref()
}
pub(crate) struct Span {
    stage: &'static str,
    count: usize,
    started: Option<Instant>,
}
impl Span {
    pub(crate) fn new(stage: &'static str, count: usize) -> Self {
        Self {
            stage,
            count,
            started: trace().map(|_| Instant::now()),
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let (Some(started), Some(trace)) = (self.started, TRACE.get().and_then(Option::as_ref)) {
            let _ = trace.sender.try_send(format!(
                "t_ms={:.3} stage={} elapsed_ms={:.3} count={}",
                trace.started.elapsed().as_secs_f64() * 1000.0,
                self.stage,
                started.elapsed().as_secs_f64() * 1000.0,
                self.count
            ));
        }
    }
}
