use std::sync::Mutex;
use std::time::Instant;

use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::core::PCWSTR;

static REQUEST_START: Mutex<Option<Instant>> = Mutex::new(None);

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
    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }
}

pub fn elapsed_us(start: Instant) -> u128 {
    start.elapsed().as_micros()
}
