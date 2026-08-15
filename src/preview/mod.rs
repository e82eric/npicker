use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

pub(crate) mod document;
use crate::view_model::ViewModelEvent;
#[cfg(test)]
use crossbeam_channel::bounded;
use crossbeam_channel::Sender;
use document::PreviewDocument;
pub use document::PreviewLine;

mod command;
use command::CommandPreviewBackend;
#[cfg(test)]
use command::{expand_preview_argument, run_process, run_resolver};
pub use command::{
    CommandPreviewTarget, NativePreviewResolver, PreviewCancellation, PreviewFunction, PreviewJob,
    PreviewOutputType, PreviewProfile, PreviewResolver,
};
mod formatted;
use formatted::FormattedPreviewBackend;
pub use formatted::PickerPreviewFormatter;
#[cfg(windows)]
pub(crate) mod native_file;
mod native_window;
pub use native_window::NativeWindowId;
use native_window::NativeWindowPreviewBackend;

#[derive(Clone, Debug)]
pub enum PreviewUpdate {
    Clear {
        generation: u64,
    },
    Ready {
        generation: u64,
        lines: Arc<[PreviewLine]>,
        truncated: bool,
        center_line: Option<usize>,
    },
    ImageReady {
        generation: u64,
        encoded: Arc<[u8]>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub enum PreviewEvent {
    Command(PreviewUpdate),
    NativeWindow(Option<NativeWindowId>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Default)]
pub enum PreviewConfig {
    #[default]
    None,
    Command(PreviewResolver),
    NativeWindow,
    Formatted,
    CommandOrNativeWindow(PreviewResolver),
}

pub struct PreviewFactory {
    config: PreviewConfig,
    shared_generation: Arc<AtomicU64>,
}

impl PreviewFactory {
    pub fn new(config: PreviewConfig) -> Self {
        Self {
            config,
            shared_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn create<I: Send + Sync + 'static>(
        &self,
        events: Sender<ViewModelEvent>,
        routes: PreviewRoutes<I>,
    ) -> Arc<dyn SelectionPreview<I>> {
        match self.config.clone() {
            PreviewConfig::None => Arc::new(NoPreviewBackend),
            PreviewConfig::Command(resolver) => Arc::new(CommandPreviewBackend::new(
                resolver,
                events,
                Arc::clone(&self.shared_generation),
                routes.command_target,
            )),
            PreviewConfig::NativeWindow => Arc::new(NativeWindowPreviewBackend {
                events,
                selected: Mutex::new(None),
                target: routes.native_window,
            }),
            PreviewConfig::Formatted => Arc::new(FormattedPreviewBackend {
                events,
                formatter: routes.formatted,
                selected: Mutex::new(None),
                generation: Arc::clone(&self.shared_generation),
            }),
            PreviewConfig::CommandOrNativeWindow(resolver) => Arc::new(RoutingPreviewBackend {
                command: CommandPreviewBackend::new(
                    resolver,
                    events.clone(),
                    Arc::clone(&self.shared_generation),
                    routes.command_target,
                ),
                native_window: NativeWindowPreviewBackend {
                    events: events.clone(),
                    selected: Mutex::new(None),
                    target: routes.native_window,
                },
                formatted: FormattedPreviewBackend {
                    events,
                    formatter: routes.formatted,
                    selected: Mutex::new(None),
                    generation: Arc::clone(&self.shared_generation),
                },
            }),
        }
    }
}

impl Default for PreviewFactory {
    fn default() -> Self {
        Self::new(PreviewConfig::None)
    }
}

pub trait SelectionPreview<I>: Send + Sync {
    fn selection_changed(&self, item: Option<&I>);
    fn clear(&self);
}

pub type CommandPreviewTargetResolver<I> =
    Arc<dyn Fn(&I) -> Option<CommandPreviewTarget> + Send + Sync>;
pub type NativeWindowTargetResolver<I> = Arc<dyn Fn(&I) -> Option<NativeWindowId> + Send + Sync>;

pub struct PreviewRoutes<I> {
    pub command_target: Option<CommandPreviewTargetResolver<I>>,
    pub native_window: Option<NativeWindowTargetResolver<I>>,
    pub formatted: Option<PickerPreviewFormatter<I>>,
}

impl<I> Clone for PreviewRoutes<I> {
    fn clone(&self) -> Self {
        Self {
            command_target: self.command_target.clone(),
            native_window: self.native_window.clone(),
            formatted: self.formatted.clone(),
        }
    }
}

impl<I> Default for PreviewRoutes<I> {
    fn default() -> Self {
        Self {
            command_target: None,
            native_window: None,
            formatted: None,
        }
    }
}

struct RoutingPreviewBackend<I> {
    command: CommandPreviewBackend<I>,
    native_window: NativeWindowPreviewBackend<I>,
    formatted: FormattedPreviewBackend<I>,
}

impl<I> SelectionPreview<I> for RoutingPreviewBackend<I> {
    fn selection_changed(&self, item: Option<&I>) {
        if self.formatted.is_configured() {
            self.command.clear();
            self.native_window.clear();
            self.formatted.selection_changed(item);
        } else if self.native_window.target(item).is_some() {
            self.command.clear();
            self.formatted.clear();
            self.native_window.selection_changed(item);
        } else {
            self.native_window.clear();
            self.formatted.clear();
            self.command.selection_changed(item);
        }
    }

    fn clear(&self) {
        self.command.clear();
        self.native_window.clear();
        self.formatted.clear();
    }
}

struct NoPreviewBackend;

impl<I> SelectionPreview<I> for NoPreviewBackend {
    fn selection_changed(&self, _item: Option<&I>) {}

    fn clear(&self) {}
}

fn send_preview(events: &Sender<ViewModelEvent>, event: PreviewEvent) {
    let _ = events.send(ViewModelEvent::Preview(event));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn process_publishes_one_completed_document() {
        let (events, receiver) = bounded(4);
        let generation = 7;
        let current_generation = Arc::new(AtomicU64::new(generation));
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Write-Output 'first'; Write-Output 'second'; Write-Output $env:NFM_PREVIEW_LINE"
                    .into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec![
                "-c".into(),
                "printf 'first\\nsecond\\n%s\\n' \"$NFM_PREVIEW_LINE\"".into(),
            ],
        );

        run_process(
            &program,
            &arguments,
            None,
            PreviewOutputType::Text,
            &CommandPreviewTarget {
                item: String::new(),
                center_line: Some(42),
            },
            generation,
            current_generation,
            events,
        );

        let ViewModelEvent::Preview(PreviewEvent::Command(update)) =
            receiver.recv().expect("preview update")
        else {
            panic!("expected command preview event");
        };
        let PreviewUpdate::Ready {
            generation: result_generation,
            lines,
            truncated,
            center_line,
        } = update
        else {
            panic!("expected completed preview");
        };
        assert_eq!(result_generation, generation);
        assert!(!truncated);
        assert_eq!(center_line, Some(42));
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert_eq!(&text[..3], ["first", "second", "42"]);
    }

    #[test]
    fn preview_arguments_expand_item_and_line_placeholders() {
        let target = CommandPreviewTarget {
            item: r"C:\files\a b.png".into(),
            center_line: Some(17),
        };
        assert_eq!(
            expand_preview_argument("--input={item}", &target),
            r"--input=C:\files\a b.png"
        );
        assert_eq!(expand_preview_argument("{line}", &target), "17");
    }

    #[test]
    fn process_resolver_returns_one_profile_name() {
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Write-Output image".into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), "printf 'image\\n'".into()],
        );

        let profile = run_resolver(
            &program,
            &arguments,
            &CommandPreviewTarget {
                item: "sample.png".into(),
                center_line: None,
            },
            3,
            Arc::new(AtomicU64::new(3)),
        )
        .unwrap();

        assert_eq!(profile.as_deref(), Some("image"));
    }

    #[test]
    fn image_process_publishes_binary_stdout() {
        let (events, receiver) = bounded(4);
        let generation = 9;
        let current_generation = Arc::new(AtomicU64::new(generation));
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Console]::OpenStandardOutput().Write([byte[]](1,2,3), 0, 3)".into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), "printf '\\001\\002\\003'".into()],
        );

        run_process(
            &program,
            &arguments,
            None,
            PreviewOutputType::Image,
            &CommandPreviewTarget {
                item: String::new(),
                center_line: None,
            },
            generation,
            current_generation,
            events,
        );

        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::Command(PreviewUpdate::ImageReady {
                generation: 9,
                encoded
            })) if encoded.as_ref() == [1, 2, 3]
        ));
    }

    #[test]
    fn native_window_backend_deduplicates_and_clears_selection() {
        let (events, receiver) = bounded(4);
        let backend = NativeWindowPreviewBackend::<()> {
            events,
            selected: Mutex::new(None),
            target: None,
        };

        backend.set_window(Some(NativeWindowId(42)));
        backend.set_window(Some(NativeWindowId(42)));
        backend.clear();

        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(Some(NativeWindowId(42))))
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(None))
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn formatted_preview_backend_parses_injected_text() {
        let (events, receiver) = bounded(4);
        let backend = FormattedPreviewBackend::<()> {
            events,
            formatter: None,
            selected: Mutex::new(None),
            generation: Arc::new(AtomicU64::new(0)),
        };

        backend.set_preview(Some(Ok("Name: example.exe\nPID: 1234".into())));

        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::Command(PreviewUpdate::Clear {
                generation: 1
            }))
        ));
        let ViewModelEvent::Preview(PreviewEvent::Command(PreviewUpdate::Ready {
            generation,
            lines,
            ..
        })) = receiver.recv().unwrap()
        else {
            panic!("expected formatted preview")
        };
        assert_eq!(generation, 1);
        assert_eq!(lines.len(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn routing_preview_backend_routes_and_clears_by_item_type() {
        fn no_command_preview(
            _target: &CommandPreviewTarget,
        ) -> Result<Option<PreviewJob>, String> {
            Ok(None)
        }

        let (events, receiver) = bounded(8);
        #[derive(Clone)]
        struct TestItem {
            text: String,
            window: Option<NativeWindowId>,
        }
        let backend = RoutingPreviewBackend {
            command: CommandPreviewBackend::new(
                PreviewResolver::Function(no_command_preview),
                events.clone(),
                Arc::new(AtomicU64::new(0)),
                Some(Arc::new(|item: &TestItem| {
                    Some(CommandPreviewTarget {
                        item: item.text.clone(),
                        center_line: None,
                    })
                })),
            ),
            native_window: NativeWindowPreviewBackend {
                events: events.clone(),
                selected: Mutex::new(None),
                target: Some(Arc::new(|item: &TestItem| item.window)),
            },
            formatted: FormattedPreviewBackend {
                events,
                formatter: None,
                selected: Mutex::new(None),
                generation: Arc::new(AtomicU64::new(0)),
            },
        };
        let window = TestItem {
            text: "window".into(),
            window: Some(NativeWindowId(42)),
        };

        backend.selection_changed(Some(&window));
        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(Some(NativeWindowId(42))))
        ));

        let item = TestItem {
            text: "item".into(),
            window: None,
        };
        backend.selection_changed(Some(&item));
        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(None))
        ));
    }
}
