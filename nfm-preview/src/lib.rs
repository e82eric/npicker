//! Shared command, formatted, file and egui previews without a native picker runtime.
pub mod preview;
pub use preview::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn hosts_receive_preview_events_without_a_view_model_or_bridge_thread() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let capture = received.clone();
        let factory = PreviewFactory::new(PreviewConfig::Formatted);
        let backend = factory.create(
            PreviewEvents::new(move |event| {
                capture.lock().unwrap().push(event);
            }),
            PreviewRoutes {
                formatted: Some(Arc::new(|item: &String| Ok(format!("Detail: {item}")))),
                ..Default::default()
            },
        );
        backend.selection_changed(Some(&"fixture".into()));
        let events = received.lock().unwrap();
        assert!(events.iter().any(|event| matches!(event,
            PreviewEvent::Command(PreviewUpdate::Ready { lines, .. })
                if lines[0].plain_text() == "Detail: fixture")));
    }
}
