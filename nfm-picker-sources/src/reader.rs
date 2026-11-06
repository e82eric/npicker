//! Any byte stream, such as standard input, as incrementally published picker
//! candidates, with the same record rules as [`crate::file`].
use crate::{
    file::{read_forward, read_reverse, CHUNK_BYTES},
    records::{spawn, Records, StreamControl, StreamSink},
};
use std::{
    io::{Cursor, Read},
    thread::JoinHandle,
};

pub struct ReaderSourceOptions {
    /// Publish records last line first. The whole stream is read before the
    /// first record is published, since the end is not known until then.
    pub reverse: bool,
    pub line_continuation: Option<String>,
    pub unique: bool,
    /// Bound one physical line or joined continuation record, not the stream.
    pub max_record_bytes: usize,
}
impl Default for ReaderSourceOptions {
    fn default() -> Self {
        Self {
            reverse: false,
            line_continuation: None,
            unique: false,
            max_record_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Starts an owned worker with the same completion, failure and cancellation
/// contract as [`crate::command::start`]. `name` labels errors, for example
/// `"stdin"`.
pub fn start<T, R>(
    name: impl Into<String>,
    mut reader: R,
    options: ReaderSourceOptions,
    sink: StreamSink<T>,
    control: StreamControl,
) -> Result<JoinHandle<()>, String>
where
    T: Copy + Default + Send + Sync + 'static,
    R: Read + Send + 'static,
{
    let name = name.into();
    let records = Records::with(
        options.line_continuation.clone(),
        options.unique,
        options.max_record_bytes,
        &sink,
    );
    spawn("reader", records, sink, control, move |records, control| {
        let describe = |error: String| format!("{name}\n{error}");
        if options.reverse {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|e| describe(e.to_string()))?;
            read_reverse(&mut Cursor::new(bytes), CHUNK_BYTES, records, control)
        } else {
            read_forward(&mut reader, CHUNK_BYTES, records, control)
        }
        .map_err(describe)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::{
        snapshot_store::SnapshotStore,
        store::{ItemsSource, StreamingItemSnapshotWithPayload},
    };
    use std::sync::{Arc, Mutex};

    fn run(input: &str, options: ReaderSourceOptions) -> Vec<String> {
        let sink = StreamSink::<u64> {
            store: Arc::new(SnapshotStore::<StreamingItemSnapshotWithPayload<u64>>::new_append_only()),
            payload: 0,
            values: Some(Arc::default()),
            error: Arc::new(Mutex::new(None)),
        };
        let store = sink.store.clone();
        let reader = Cursor::new(input.as_bytes().to_vec());
        start("test", reader, options, sink, StreamControl::default())
            .unwrap()
            .join()
            .unwrap();
        assert!(store.is_done());
        store.snapshot().map_or_else(Vec::new, |s| {
            (0..s.len())
                .map(|i| s.get_string_lossy(i, &mut Vec::new()))
                .collect()
        })
    }

    #[test]
    fn reads_forward_reverse_and_unique() {
        assert_eq!(run("a\nb\na\n", Default::default()), ["a", "b", "a"]);
        let reverse = ReaderSourceOptions {
            reverse: true,
            unique: true,
            ..Default::default()
        };
        assert_eq!(run("a\nb\na\n", reverse), ["a", "b"]);
    }
}
