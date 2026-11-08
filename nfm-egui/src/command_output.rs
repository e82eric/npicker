//! A query command's output in the picker's preview pane: see
//! [`Picker::show_command_output`](crate::Picker::show_command_output).
use crate::styled_document::StyledDocument;
use egui::Context;
use std::sync::{Arc, Mutex};

/// A query command's output, which the host keeps publishing as it grows,
/// from any thread. The picker shows the latest document in its preview pane
/// below the command line, until Esc, another command's output, or leaving
/// command mode dismisses it, or the picker is dropped. The host learns of
/// that with [`CommandOutput::on_close`] or [`CommandOutput::is_closed`], to
/// stop whatever produces the output.
#[derive(Clone, Default)]
pub struct CommandOutput(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    version: u64,
    document: Option<Arc<StyledDocument>>,
    finished: bool,
    closed: bool,
    ctx: Option<Context>,
    on_close: Vec<Box<dyn FnOnce() + Send>>,
}

impl CommandOutput {
    pub fn new() -> Self {
        Self::default()
    }
    /// Show `document` in place of the one before. It keeps its place, or
    /// moves to the new end while its end was in view.
    pub fn publish(&self, document: Arc<StyledDocument>) {
        let ctx = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.version += 1;
            state.document = Some(document);
            state.ctx.clone()
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }
    /// Say that no more output is coming, which stops the loading indicator.
    pub fn finish(&self) {
        let ctx = {
            let mut state = self.lock();
            state.finished = true;
            state.ctx.clone()
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }
    /// The document last published.
    pub fn document(&self) -> Option<Arc<StyledDocument>> {
        self.lock().document.clone()
    }
    /// Whether `finish` was called.
    pub fn is_finished(&self) -> bool {
        self.lock().finished
    }
    /// Whether the picker dismissed the output.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
    /// Run `close` once the picker dismisses the output, or now if it has.
    /// It runs on the thread that dismisses it, usually the UI thread.
    pub fn on_close(&self, close: impl FnOnce() + Send + 'static) {
        let mut state = self.lock();
        if state.closed {
            drop(state);
            close();
        } else {
            state.on_close.push(Box::new(close));
        }
    }

    pub(crate) fn attach(&self, ctx: &Context) {
        self.lock().ctx = Some(ctx.clone());
    }
    /// The latest document and its version, and whether it is finished.
    pub(crate) fn current(&self) -> (u64, Option<Arc<StyledDocument>>, bool) {
        let state = self.lock();
        (state.version, state.document.clone(), state.finished)
    }
    pub(crate) fn close(&self) {
        let callbacks = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.ctx = None;
            std::mem::take(&mut state.on_close)
        };
        for callback in callbacks {
            callback();
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
