use std::sync::{Mutex, OnceLock};
use std::time::Instant;

type TimingSink = fn(&str);

static REQUEST_START: Mutex<Option<Instant>> = Mutex::new(None);
static SINK: OnceLock<TimingSink> = OnceLock::new();

pub fn set_sink(sink: TimingSink) {
    let _ = SINK.set(sink);
}

/// Installs the startup timing sink only when `NFM_TIMING` is exactly `1`.
/// Once installed, the sink remains active for the process lifetime.
pub fn set_sink_from_env(sink: TimingSink) {
    if std::env::var_os("NFM_TIMING").as_deref() == Some(std::ffi::OsStr::new("1")) {
        set_sink(sink);
    }
}

pub fn is_enabled() -> bool {
    SINK.get().is_some()
}

pub fn begin_request() {
    if !is_enabled() {
        return;
    }
    *REQUEST_START.lock().expect("timing mutex poisoned") = Some(Instant::now());
}

pub fn write(message: impl AsRef<str>) {
    write_lazy(|| message);
}

/// Formats and writes a timing message only when a sink is configured.
pub fn write_lazy<M: AsRef<str>>(message: impl FnOnce() -> M) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let message = message();
    let elapsed_ms = REQUEST_START
        .lock()
        .expect("timing mutex poisoned")
        .map(|start| start.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let line = format!(
        "[RustWin32HostTiming] +{elapsed_ms:.3}ms {}\n",
        message.as_ref()
    );
    sink(&line);
}

pub fn elapsed_us(start: Instant) -> u128 {
    start.elapsed().as_micros()
}
