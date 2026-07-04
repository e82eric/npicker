use std::sync::{Mutex, OnceLock};
use std::time::Instant;

type TimingSink = fn(&str);

static REQUEST_START: Mutex<Option<Instant>> = Mutex::new(None);
static SINK: OnceLock<TimingSink> = OnceLock::new();

pub fn set_sink(sink: TimingSink) {
    let _ = SINK.set(sink);
}

pub fn begin_request() {
    *REQUEST_START.lock().expect("timing mutex poisoned") = Some(Instant::now());
}

pub fn write(message: impl AsRef<str>) {
    let elapsed_ms = REQUEST_START
        .lock()
        .expect("timing mutex poisoned")
        .map(|start| start.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let line = format!(
        "[RustWin32HostTiming] +{elapsed_ms:.3}ms {}\n",
        message.as_ref()
    );
    if let Some(sink) = SINK.get() {
        sink(&line);
    }
}

pub fn elapsed_us(start: Instant) -> u128 {
    start.elapsed().as_micros()
}
