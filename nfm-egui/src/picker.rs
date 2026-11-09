use crate::{
    Appearance, ItemsSource, Placement, PreviewProvider, PreviewRequest, SearchSnapshotProvider,
    SearchSortMode,
};
use crate::{
    preview::{self, Preview, PreviewWorker},
    session::{Session, Update},
};
use egui::{Context, Id, Key, Modifiers};
use nfm_picker_sources::structured::{StructuredStreamingSnapshot, structured_source};
use crate::icons::{IconCache, IconResolver, paint_icon};
use crate::query_command::{self, QueryCommand};
use nfm_search_core::row::{
    RowAlign, RowFields, RowSlot, RowStyle, RowTemplate, SlotKind,
};
use nfm_search_core::search::resolve_match_positions;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const PANEL_RADIUS: u8 = 8;
const PANEL_MARGIN: egui::Margin = egui::Margin::symmetric(18, 10);
const PANEL_BORDER: f32 = 1.0;
const PANEL_INSET_BOTTOM: f32 = PANEL_MARGIN.bottom as f32 + PANEL_BORDER;
const PANEL_FRAME_HEIGHT: f32 =
    (PANEL_MARGIN.top as f32 + PANEL_MARGIN.bottom as f32) + 2.0 * PANEL_BORDER;
const PANEL_FRAME_WIDTH: f32 =
    (PANEL_MARGIN.left as f32 + PANEL_MARGIN.right as f32) + 2.0 * PANEL_BORDER;
const DIVIDER_HEIGHT: f32 = 15.0;
// Existing query sizing allowance, independent of the panel's margins.
const QUERY_HEIGHT_ALLOWANCE: f32 = 4.0;
const RESULTS_OVERHEAD: f32 = PANEL_FRAME_HEIGHT + DIVIDER_HEIGHT + QUERY_HEIGHT_ALLOWANCE;
const PREVIEW_OVERHEAD: f32 = RESULTS_OVERHEAD + DIVIDER_HEIGHT;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    QueryBottom,
    QueryTop,
}
/// An interaction the host can override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerTrigger {
    Key(Key),
    DoubleClick,
}
/// A picker command. Key bindings, the command palette and
/// [`Picker::execute`] all run commands; `Custom` commands are reported to
/// the host as [`PickerEvent::CustomAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerAction {
    /// Open or close the command palette, which also lists key bindings.
    ToggleCommandPalette,
    TogglePreview,
    /// Enter or leave preview copy mode.
    TogglePreviewFocus,
    QuickSelect,
    /// Ask the host to copy the selected result.
    CopySelection,
    /// Copy the whole preview document.
    CopyPreviewDocument,
    ScrollPreviewPageUp,
    ScrollPreviewPageDown,
    ScrollPreviewLineUp,
    ScrollPreviewLineDown,
    /// Show or hide line numbers, like Vim's `number`. With relative
    /// numbers on, this switches between relative and hybrid.
    TogglePreviewLineNumbers,
    /// Show or hide relative line numbers, like Vim's `relativenumber`.
    /// With line numbers on, this switches between absolute and hybrid.
    ToggleRelativePreviewLineNumbers,
    /// Wrap plain-text previews at the pane width, or scroll them sideways.
    TogglePreviewWrap,
    ToggleFullPane,
    /// Switch the query between searching and typing a query command (see
    /// [`Picker::set_query_commands`]). Applies only with commands registered.
    ToggleCommandMode,
    /// Bind a chord to this to remove its default command.
    Unbind,
    Custom(usize),
}
impl PickerAction {
    /// Every built-in command, in command palette order.
    pub const BUILT_IN: [PickerAction; 15] = [
        Self::ToggleCommandPalette,
        Self::TogglePreview,
        Self::TogglePreviewFocus,
        Self::QuickSelect,
        Self::CopySelection,
        Self::CopyPreviewDocument,
        Self::ScrollPreviewPageUp,
        Self::ScrollPreviewPageDown,
        Self::ScrollPreviewLineUp,
        Self::ScrollPreviewLineDown,
        Self::TogglePreviewLineNumbers,
        Self::ToggleRelativePreviewLineNumbers,
        Self::TogglePreviewWrap,
        Self::ToggleFullPane,
        Self::ToggleCommandMode,
    ];
    /// Stable name for host configuration, such as `"TogglePreview"`. `Unbind`
    /// is `"None"`; custom commands have no name.
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::ToggleCommandPalette => "ToggleCommandPalette",
            Self::TogglePreview => "TogglePreview",
            Self::TogglePreviewFocus => "TogglePreviewFocus",
            Self::QuickSelect => "QuickSelect",
            Self::CopySelection => "CopySelection",
            Self::CopyPreviewDocument => "CopyPreviewDocument",
            Self::ScrollPreviewPageUp => "ScrollPreviewPageUp",
            Self::ScrollPreviewPageDown => "ScrollPreviewPageDown",
            Self::ScrollPreviewLineUp => "ScrollPreviewLineUp",
            Self::ScrollPreviewLineDown => "ScrollPreviewLineDown",
            Self::TogglePreviewLineNumbers => "TogglePreviewLineNumbers",
            Self::ToggleRelativePreviewLineNumbers => "ToggleRelativePreviewLineNumbers",
            Self::TogglePreviewWrap => "TogglePreviewWrap",
            Self::ToggleFullPane => "ToggleFullPane",
            Self::ToggleCommandMode => "ToggleCommandMode",
            Self::Unbind => "None",
            Self::Custom(_) => return None,
        })
    }
    /// The built-in command (or `Unbind`) with this [`name`](Self::name).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::BUILT_IN
            .into_iter()
            .chain([Self::Unbind])
            .find(|action| action.name() == Some(name))
    }
    /// Command palette title; custom commands use their binding's description.
    pub fn title(self) -> Option<&'static str> {
        Some(match self {
            Self::ToggleCommandPalette => "Command palette",
            Self::TogglePreview => "Toggle preview",
            Self::TogglePreviewFocus => "Focus preview / query",
            Self::QuickSelect => "Quick Select preview",
            Self::CopySelection => "Copy selection",
            Self::CopyPreviewDocument => "Copy preview document",
            Self::ScrollPreviewPageUp => "Scroll preview up a page",
            Self::ScrollPreviewPageDown => "Scroll preview down a page",
            Self::ScrollPreviewLineUp => "Scroll preview up a line",
            Self::ScrollPreviewLineDown => "Scroll preview down a line",
            Self::TogglePreviewLineNumbers => "Toggle line numbers",
            Self::ToggleRelativePreviewLineNumbers => "Toggle relative line numbers",
            Self::TogglePreviewWrap => "Toggle preview line wrap",
            Self::ToggleFullPane => "Toggle full-pane picker",
            Self::ToggleCommandMode => "Toggle command mode",
            Self::Unbind | Self::Custom(_) => return None,
        })
    }
    /// Whether a key repeat runs the command again. Egui also reports a press
    /// as a repeat when it never saw the key's release, so most commands accept
    /// repeats; these run once per physical press.
    fn repeats(self) -> bool {
        !matches!(
            self,
            Self::ToggleFullPane
                | Self::ToggleCommandMode
                | Self::TogglePreviewLineNumbers
                | Self::ToggleRelativePreviewLineNumbers
                | Self::TogglePreviewWrap
                | Self::Unbind
                | Self::Custom(_)
        )
    }
}
#[derive(Clone, Debug)]
pub struct PickerBinding {
    pub trigger: PickerTrigger,
    pub modifiers: Modifiers,
    pub action: PickerAction,
    /// Command palette title of a custom command. Omit it to leave the
    /// command out of the palette. Built-in commands use their own title.
    pub description: Option<String>,
}
impl PickerBinding {
    fn key(key: Key, modifiers: Modifiers, action: PickerAction) -> Self {
        Self {
            trigger: PickerTrigger::Key(key),
            modifiers,
            action,
            description: None,
        }
    }
    fn shortcut_label(&self) -> String {
        let mut label = String::new();
        for (enabled, prefix) in [
            (self.modifiers.ctrl, "Ctrl+"),
            (self.modifiers.shift, "Shift+"),
            (self.modifiers.alt, "Alt+"),
            (self.modifiers.mac_cmd, "Cmd+"),
        ] {
            if enabled {
                label.push_str(prefix);
            }
        }
        label.push_str(match self.trigger {
            PickerTrigger::Key(Key::Slash) => "/",
            PickerTrigger::Key(Key::Questionmark) => "?",
            PickerTrigger::Key(Key::Semicolon) => ";",
            PickerTrigger::Key(Key::ArrowUp) => "Up",
            PickerTrigger::Key(Key::ArrowDown) => "Down",
            PickerTrigger::Key(Key::ArrowLeft) => "Left",
            PickerTrigger::Key(Key::ArrowRight) => "Right",
            PickerTrigger::Key(Key::PageUp) => "Page Up",
            PickerTrigger::Key(Key::PageDown) => "Page Down",
            PickerTrigger::Key(key) => key.name(),
            PickerTrigger::DoubleClick => "Double click",
        });
        label
    }
}
/// The picker's default key bindings. Host bindings replace these by chord;
/// bind a chord to [`PickerAction::Unbind`] to remove one.
pub fn default_bindings() -> Vec<PickerBinding> {
    use PickerAction::*;
    let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
    [
        (Key::Slash, ctrl_shift, ToggleCommandPalette),
        // Shifted slash arrives as a question mark on some layouts.
        (Key::Questionmark, ctrl_shift, ToggleCommandPalette),
        (Key::P, Modifiers::CTRL, TogglePreview),
        (Key::Space, Modifiers::CTRL, TogglePreview),
        (Key::W, Modifiers::CTRL, TogglePreviewFocus),
        (Key::Space, Modifiers::ALT, QuickSelect),
        (Key::C, Modifiers::CTRL, CopySelection),
        (Key::C, ctrl_shift, CopyPreviewDocument),
        (Key::PageUp, Modifiers::CTRL, ScrollPreviewPageUp),
        (Key::PageDown, Modifiers::CTRL, ScrollPreviewPageDown),
        (Key::ArrowUp, Modifiers::CTRL, ScrollPreviewLineUp),
        (Key::ArrowDown, Modifiers::CTRL, ScrollPreviewLineDown),
        (Key::L, ctrl_shift, TogglePreviewLineNumbers),
        // As in VS Code's word wrap toggle.
        (Key::Z, Modifiers::ALT, TogglePreviewWrap),
        (Key::F, ctrl_shift, ToggleFullPane),
        (Key::Semicolon, Modifiers::CTRL, ToggleCommandMode),
    ]
    .into_iter()
    .map(|(key, modifiers, action)| PickerBinding::key(key, modifiers, action))
    .collect()
}
fn resolved_bindings(bindings: Vec<PickerBinding>) -> Vec<PickerBinding> {
    let mut resolved = default_bindings();
    for binding in bindings {
        if let Some(existing) = resolved.iter_mut().find(|existing| {
            existing.trigger == binding.trigger
                && same_binding_modifiers(existing.modifiers, binding.modifiers)
        }) {
            *existing = binding;
        } else {
            resolved.push(binding);
        }
    }
    resolved.retain(|binding| binding.action != PickerAction::Unbind);
    resolved
}
/// A command palette row: a command, its title and its current keys.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaletteRow {
    keys: String,
    title: String,
    action: PickerAction,
}
/// The open command palette: a picker over a Command/Keys table, so it
/// searches, highlights, filters columns and completes like any structured
/// source. `actions[i]` is the command on row `i`.
struct CommandPalette {
    picker: Picker<StructuredStreamingSnapshot>,
    actions: Vec<PickerAction>,
}
/// Space between the palette and the edge of the picker it covers.
const PALETTE_MARGIN: f32 = 12.0;
/// Right-align numbers in `rect`, leaving its last cell as a separator;
/// left-aligned numbers start at its left edge.
fn paint_line_numbers(
    ui: &egui::Ui,
    rect: egui::Rect,
    numbers: &[Option<crate::preview_copy::GutterNumber>],
    appearance: &Appearance,
) {
    let typography = &appearance.typography;
    let row_height = typography.preview_row_height(ui.ctx());
    let right = rect.right() - typography.cell_width.max(1.0);
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    for (y, number) in numbers.iter().enumerate() {
        let Some(number) = number else {
            continue;
        };
        let (font, color) = if number.current {
            (typography.bold.clone(), appearance.palette.text)
        } else {
            (typography.normal.clone(), appearance.palette.muted)
        };
        let y = rect.top() + (y as f32 + 0.5) * row_height;
        let (x, align) = if number.left_aligned {
            (rect.left(), egui::Align2::LEFT_CENTER)
        } else {
            (right, egui::Align2::RIGHT_CENTER)
        };
        painter.text(egui::pos2(x, y), align, &number.text, font, color);
    }
}
fn same_binding_modifiers(a: Modifiers, b: Modifiers) -> bool {
    (a.ctrl, a.shift, a.alt, a.mac_cmd) == (b.ctrl, b.shift, b.alt, b.mac_cmd)
}
/// Preview gutter numbering. Numbers count logical lines: soft-wrapped
/// continuation rows are left blank, matching how `j`/`k` counts move.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewLineNumbers {
    #[default]
    Off,
    /// One-based line numbers.
    Absolute,
    /// Distance from the copy cursor's line while the preview is focused;
    /// absolute numbers otherwise. The cursor line shows 0.
    Relative,
    /// Relative numbers with the cursor line's absolute number (Vim's
    /// `number` + `relativenumber`); absolute numbers while unfocused.
    Hybrid,
}
/// Fewest digits the line-number gutter reserves, so that short documents
/// share one content width.
const MIN_LINE_NUMBER_DIGITS: usize = 3;
#[derive(Clone, Debug)]
pub struct PickerConfig {
    pub layout: Layout,
    /// Host metadata; the Ghostty-style panel has no separate title row.
    pub title: String,
    pub initial_query: String,
    pub result_rows: usize,
    /// Hosts can enable this for theme/action lists that accept a single click.
    pub accept_on_click: bool,
    /// Override shared defaults with built-in actions or host custom actions.
    pub bindings: Vec<PickerBinding>,
    pub preview_rows: usize,
    pub preview_visible: bool,
    pub preview_line_numbers: PreviewLineNumbers,
    /// Wrap plain-text previews at the pane width. Off, lines up to 4096
    /// cells scroll sideways. Providers that lay out their own pages, such as
    /// terminal output, are unaffected.
    pub preview_wrap: bool,
    pub sort: SearchSortMode,
}
impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            layout: Layout::QueryBottom,
            title: "Picker".into(),
            initial_query: String::new(),
            result_rows: 7,
            accept_on_click: false,
            bindings: Vec::new(),
            preview_rows: 12,
            preview_visible: true,
            preview_line_numbers: PreviewLineNumbers::Off,
            preview_wrap: true,
            sort: SearchSortMode::Score,
        }
    }
}
#[derive(Clone)]
pub struct Selection<T> {
    pub item: T,
    pub text: String,
    pub index: usize,
    pub source_version: u64,
    pub snapshot: Arc<dyn std::any::Any + Send + Sync>,
}
impl<T> Selection<T> {
    pub fn source_snapshot<S: Send + Sync + 'static>(&self) -> Option<Arc<S>> {
        self.snapshot.clone().downcast().ok()
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Selection<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection")
            .field("item", &self.item)
            .field("text", &self.text)
            .field("index", &self.index)
            .field("source_version", &self.source_version)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug)]
pub enum PickerEvent<T> {
    Accepted(Selection<T>),
    CustomAction {
        action: usize,
        selection: Selection<T>,
    },
    Cancelled,
    CopyRequested(Selection<T>),
    /// Enter on a query command line (see [`Picker::set_query_commands`]).
    /// The picker stays open with the command line as its query; the host
    /// decides what happens next, such as replacing the source or closing.
    Command {
        name: String,
        argument: String,
        /// Run with Ctrl+Enter rather than Enter, for a command's second
        /// action, such as copying a calculator's result instead of using it.
        alternate: bool,
    },
}
pub struct PickerOutput<T> {
    /// Content paint commands were built this frame; this is not GPU presentation.
    pub preview_rendered: bool,
    /// Document identity of the worker preview drawn this frame.
    pub preview_selection: Option<(u64, usize)>,
    /// Host-rendered preview bounds in egui points; hidden while help is open.
    pub preview_rect: Option<egui::Rect>,
    pub event: Option<PickerEvent<T>>,
    frame_selection: Option<Selection<T>>,
    pub wants_keyboard: bool,
    pub busy: bool,
    pub matched: usize,
    pub total: usize,
    pub rect: egui::Rect,
}

impl<T> PickerOutput<T> {
    /// The event's owned selection, or the current selection captured this frame.
    /// Event selections retain their exact source snapshot after the picker closes.
    pub fn selection(&self) -> Option<&Selection<T>> {
        match &self.event {
            Some(PickerEvent::Accepted(selection))
            | Some(PickerEvent::CustomAction { selection, .. })
            | Some(PickerEvent::CopyRequested(selection)) => Some(selection),
            _ => self.frame_selection.as_ref(),
        }
    }
}

/// The icon reference for a one-line result, from its item (see
/// [`Picker::set_row_icon`]).
pub type RowIcon<T> = Arc<dyn Fn(&T) -> Option<String> + Send + Sync>;

/// Which query command a result stands for, if any (see
/// [`Picker::set_command_rows`]).
pub type CommandRows<T> = Arc<dyn Fn(&Selection<T>) -> Option<String> + Send + Sync>;

/// What shows after a command line as it is typed, from the command's name
/// and argument (see [`Picker::set_command_preview`]).
pub type CommandPreview = Arc<dyn Fn(&str, &str) -> Option<CommandHint> + Send + Sync>;

/// A host's preview of a command line, such as a calculator's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandHint {
    /// Dim text after the cursor.
    pub text: String,
    /// What Right arrow at the end of the line makes the argument, such as
    /// the result in place of the expression.
    pub completion: Option<String>,
    /// Rows under the command, such as a result in each base. Up and Down
    /// select one; Enter makes its value the argument and Ctrl+Enter copies
    /// it, without reporting the command.
    pub choices: Vec<CommandChoice>,
}

/// One of a [`CommandHint`]'s choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandChoice {
    /// The row, such as `0b1101_1101`.
    pub text: String,
    /// What Enter and Ctrl+Enter use, such as `0b11011101`.
    pub value: String,
}

/// A command's output as the picker shows it. Dropping it dismisses the
/// output, which tells the host.
struct ShownOutput {
    output: crate::CommandOutput,
    /// The version, columns and rows the viewport was laid out for, and
    /// whether that layout was empty.
    laid_out: Option<(u64, u16, usize, bool)>,
    /// The search preview's scroll position, for after the output.
    search_scroll_offset: usize,
    /// Showing the output opened the hidden preview; dismissing it hides
    /// the preview again unless the user toggled it meanwhile.
    opened_preview: bool,
}
impl Drop for ShownOutput {
    fn drop(&mut self) {
        self.output.close();
    }
}

pub struct Picker<S: ItemsSource + Send + Sync + 'static> {
    id: Id,
    session: Session<S>,
    config: PickerConfig,
    query: String,
    revision: u64,
    transition_pending: bool,
    displayed: Option<Update<S>>,
    completion_snapshot: Option<Arc<S>>,
    selected: usize,
    visible_start: usize,
    focus_query: bool,
    closed: bool,
    preview: Option<PreviewWorker<S::Item>>,
    preview_key: Option<(u64, usize, u16, u16, usize, u32, u32)>,
    scroll_offset: usize,
    error: Option<String>,
    visible_result_rows: usize,
    /// Host-resolved icons in card slots.
    icons: IconCache,
    /// Icons beside one-line results.
    row_icon: Option<RowIcon<S::Item>>,
    /// Results that stand for query commands.
    command_rows: Option<CommandRows<S::Item>>,
    visible_preview_rows: usize,
    /// The preview pane's height this frame when a command's output fills the
    /// command list's space too, down to the query; else rows of the preview.
    output_pane_height: Option<f32>,
    external_preview: bool,
    preview_rect: Option<egui::Rect>,
    preview_rendered: bool,
    copied_until: Option<Instant>,
    suggestions: Vec<nfm_search_core::store::SearchCompletion>,
    suggestion_selected: usize,
    query_cursor: usize,
    query_rect: egui::Rect,
    query_text_pos: egui::Pos2,
    dismissed_completion: Option<(String, usize)>,
    palette: Option<Box<CommandPalette>>,
    /// Commands from `execute`, run at the start of the next frame.
    pending_actions: Vec<PickerAction>,
    copy_notice: String,
    preview_viewport: Option<crate::preview_copy::PreviewCopyMode>,
    preview_focus: bool,
    placement_override: Option<crate::Position>,
    /// The full-pane toggle showed a hidden preview; leaving full pane hides
    /// it again unless the user or host changed its visibility meanwhile.
    full_pane_preview: bool,
    result_actions_enabled: bool,
    cursor_scroll_padding: u32,
    /// Logical lines in the last numbered preview; sizes the gutter.
    preview_line_count: u64,
    /// Commands typed in command mode; none leaves `> ` and `: ` as search text.
    query_commands: Vec<QueryCommand>,
    /// The query last sent to the search. Command mode leaves it alone and
    /// restores it as the query on the way out.
    searched_query: String,
    /// Whether the query box holds a command line rather than a search.
    command_mode: bool,
    /// The command line kept while searching, restored in command mode.
    command_line: String,
    /// Why the last Enter on a command line did not run it. Cleared when
    /// the query changes.
    command_error: Option<String>,
    /// The selected row of the commands command mode lists in place of
    /// results.
    command_selected: usize,
    /// A listed command clicked this frame.
    command_clicked: Option<usize>,
    command_preview: Option<CommandPreview>,
    /// The preview of the command line it was made for.
    previewed_command: Option<(String, Option<CommandHint>)>,
    /// See [`Picker::set_preview_follows_end`].
    preview_follows_end: bool,
    /// Whether a growing preview is showing its end, to keep it there.
    preview_following: bool,
    /// A query command's output, shown in the preview pane in command mode.
    command_output: Option<ShownOutput>,
}
impl<S> Picker<S>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Send + 'static,
{
    pub fn new<P: SearchSnapshotProvider<S> + ?Sized>(
        id: impl egui::AsId,
        ctx: &Context,
        provider: Arc<P>,
        mut config: PickerConfig,
    ) -> Self {
        let query = config.initial_query.clone();
        let query_cursor = query.len();
        let searched_query = query.clone();
        config.bindings = resolved_bindings(config.bindings);
        Self {
            id: Id::new(id),
            session: Session::new(provider, query.clone(), config.sort, ctx.clone()),
            config,
            query,
            revision: 0,
            transition_pending: false,
            displayed: None,
            completion_snapshot: None,
            selected: 0,
            visible_start: 0,
            focus_query: true,
            closed: false,
            preview: None,
            preview_key: None,
            scroll_offset: 0,
            error: None,
            visible_result_rows: 7,
            icons: IconCache::default(),
            row_icon: None,
            command_rows: None,
            visible_preview_rows: 12,
            output_pane_height: None,
            external_preview: false,
            preview_rect: None,
            preview_rendered: false,
            copied_until: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            query_cursor,
            query_rect: egui::Rect::NOTHING,
            query_text_pos: egui::Pos2::ZERO,
            dismissed_completion: None,
            palette: None,
            pending_actions: Vec::new(),
            copy_notice: "Copied".into(),
            preview_viewport: None,
            preview_focus: false,
            placement_override: None,
            full_pane_preview: false,
            result_actions_enabled: true,
            cursor_scroll_padding: crate::copy_mode::SCROLL_PADDING_ROWS,
            preview_line_count: 0,
            query_commands: Vec::new(),
            searched_query,
            command_mode: false,
            command_line: String::new(),
            command_error: None,
            command_selected: 0,
            command_clicked: None,
            command_preview: None,
            previewed_command: None,
            preview_follows_end: false,
            preview_following: true,
            command_output: None,
        }
    }
    /// Register the commands typed in command mode, such as
    /// `GrepClaudeSessions fn main`. [`PickerAction::ToggleCommandMode`]
    /// (Ctrl+;) switches the query box between searching and a command line,
    /// as does typing `> ` or `: ` at the start of a search; the prompt shows
    /// `:` in command mode. In command mode the results area lists the commands
    /// whose names match what is typed; Up and Down select one and Tab or
    /// Enter types its name. The rest of the line is the argument, verbatim.
    /// Enter on a valid command line reports [`PickerEvent::Command`]
    /// instead of accepting a result, and Ctrl+Enter reports it as the
    /// command's alternate, for a second action such as copying. The search
    /// before command mode, with its results and selection, comes back on
    /// leaving it. With no commands, there is no command mode and `> ` and
    /// `: ` are ordinary search text.
    pub fn set_query_commands(&mut self, commands: Vec<QueryCommand>) {
        self.query_commands = commands;
        self.command_error = None;
        if self.query_commands.is_empty() {
            self.set_command_mode(false);
        }
    }
    /// Mark results that stand for query commands, such as a commands list
    /// that also lists its query commands: `rows` names the command a result
    /// stands for. Accepting such a result does what completing the command's
    /// name does: one with an argument enters command mode with `Name `
    /// typed, ready for the argument, and one without reports
    /// [`PickerEvent::Command`]. Other results are accepted as usual.
    pub fn set_command_rows(&mut self, rows: Option<CommandRows<S::Item>>) {
        self.command_rows = rows;
    }
    /// Show text after a command line as it is typed, such as a
    /// calculator's result: `preview` gets the name of a registered command
    /// and its argument, once the argument is not empty. It runs on the UI
    /// thread when the line changes, so it must be quick; `None` shows
    /// nothing. The hint's text is dim, after the cursor, as the argument
    /// placeholder is, and Right arrow at the end of the line makes its
    /// `completion` the argument.
    pub fn set_command_preview(&mut self, preview: Option<CommandPreview>) {
        self.command_preview = preview;
        self.previewed_command = None;
    }
    /// The line Right arrow at its end completes to: the selected command's
    /// name while a name is typed that starts it, or the host's completion
    /// of the argument.
    fn ghost_completion(&self) -> Option<String> {
        if !self.command_mode || self.command_error.is_some() {
            return None;
        }
        let line = query_command::parse(&self.query);
        match line.argument {
            None => {
                let command = *self.listed_commands().get(self.command_selected)?;
                name_rest(&command.name, line.name)?;
                Some(if command.argument.is_some() {
                    format!("{} ", command.name)
                } else {
                    command.name.clone()
                })
            }
            Some(argument) if !argument.is_empty() => {
                let (previewed, hint) = self.previewed_command.as_ref()?;
                let completion = hint.as_ref()?.completion.as_ref()?;
                (*previewed == self.query && completion != argument)
                    .then(|| format!("{} {completion}", line.name))
            }
            Some(_) => None,
        }
    }
    /// Keep a growing preview's end in view, as a terminal does, for a
    /// preview of output that is still arriving, such as a streamed answer:
    /// when the source publishes a new version of the selected result, the
    /// preview keeps its place instead of starting again at the top, and
    /// moves to the new end while it was showing the end. Scrolling up stops
    /// following until the preview is scrolled back to the end.
    pub fn set_preview_follows_end(&mut self, follow: bool) {
        self.preview_follows_end = follow;
    }
    /// Show a command's output in the preview pane, below the command line,
    /// such as an answer that streams in after Enter on
    /// [`PickerEvent::Command`]. The pane opens if it was hidden, and the
    /// output follows its end as it grows, as a preview with
    /// [`Picker::set_preview_follows_end`] does. Ctrl+Up and Ctrl+Down scroll it,
    /// Ctrl+W enters copy mode on it, Ctrl+Shift+C copies all of it, and
    /// so does Ctrl+C while no command-line text is selected. The command
    /// line stays editable: Enter runs it again, and its output replaces
    /// this one. Esc dismisses the output and keeps the line, as does
    /// leaving command mode. Outside command mode the output is dismissed
    /// at once.
    pub fn show_command_output(&mut self, ctx: &Context, output: crate::CommandOutput) {
        if !self.command_mode {
            output.close();
            return;
        }
        // The output before this one is dismissed, and its state carried over.
        let previous = self.command_output.take();
        self.leave_preview_focus();
        let opened_preview = match &previous {
            Some(previous) => previous.opened_preview,
            None => !std::mem::replace(&mut self.config.preview_visible, true),
        };
        let search_scroll_offset = previous
            .as_ref()
            .map_or(self.scroll_offset, |previous| previous.search_scroll_offset);
        drop(previous);
        output.attach(ctx);
        self.command_output = Some(ShownOutput {
            output,
            laid_out: None,
            search_scroll_offset,
            opened_preview,
        });
        self.preview_following = true;
        self.scroll_offset = 0;
        ctx.request_repaint();
    }
    /// Whether a command's output is shown; see [`Picker::show_command_output`].
    pub fn command_output_shown(&self) -> bool {
        self.command_output.is_some()
    }
    /// Dismiss the command's output, if any, as Esc does.
    pub fn dismiss_command_output(&mut self) {
        let Some(shown) = self.command_output.take() else {
            return;
        };
        self.leave_preview_focus();
        self.scroll_offset = shown.search_scroll_offset;
        self.preview_following = true;
        if shown.opened_preview {
            self.config.preview_visible = false;
        }
    }
    /// Lay out the command's output for the preview pane when it changed, as
    /// a viewport that stays at the end while following it.
    fn refresh_command_output(&mut self, width: f32, appearance: &Appearance) {
        let cell_width = appearance.typography.cell_width.max(1.0);
        let width = width - self.preview_gutter_columns() as f32 * cell_width;
        let columns = (width / cell_width).floor().clamp(1.0, 4096.0) as u16;
        let rows = self.visible_preview_rows.max(1);
        let Some(shown) = &mut self.command_output else {
            return;
        };
        // Copy mode keeps the document it opened on until it is left.
        if self.preview_focus {
            return;
        }
        let (version, document, _) = shown.output.current();
        let current = shown
            .laid_out
            .is_some_and(|(shown_version, shown_columns, shown_rows, empty)| {
                (shown_version, shown_columns, shown_rows) == (version, columns, rows)
                    && (empty || self.preview_viewport.is_some())
            });
        if current {
            return;
        }
        let layout = document.and_then(|document| {
            document
                .layout(columns as usize, &crate::Cancellation::never())
                .ok()
        });
        let total = layout.as_ref().map_or(0, |layout| layout.total_rows());
        let last_page = total.saturating_sub(rows);
        let top = if self.preview_following {
            last_page
        } else {
            self.scroll_offset.min(last_page)
        };
        shown.laid_out = Some((version, columns, rows, total == 0));
        self.scroll_offset = top;
        self.preview_viewport = layout.and_then(|layout| {
            let mut copy = crate::preview_copy::PreviewCopyMode::host(layout, top, rows)?;
            copy.set_scroll_padding(self.cursor_scroll_padding);
            Some(copy)
        });
    }
    /// Copy all of the command's output, as Ctrl+Shift+C does.
    fn copy_command_output(&mut self, ctx: &Context) -> bool {
        let Some(viewport) = self
            .preview_viewport
            .as_ref()
            .filter(|_| self.command_output.is_some())
        else {
            return false;
        };
        viewport.copy_all(ctx);
        self.notify_copied_value(ctx, "");
        self.copy_notice = "Copied the output".into();
        true
    }
    /// Say why the host could not run the command line, as ghost text in the
    /// error color, as for an unknown command, until the line changes.
    pub fn set_command_error(&mut self, ctx: &Context, error: impl Into<String>) {
        if self.command_mode {
            self.command_error = Some(error.into());
            ctx.request_repaint();
        }
    }
    /// The preview of the command line, made once per line.
    fn refresh_command_preview(&mut self) {
        if self
            .previewed_command
            .as_ref()
            .is_some_and(|(line, _)| *line == self.query)
        {
            return;
        }
        let hint = self.command_preview.as_ref().and_then(|preview| {
            let (command, argument) = self.typed_command()?;
            (!argument.is_empty())
                .then(|| preview(&command.name, argument))
                .flatten()
        });
        self.previewed_command = Some((self.query.clone(), hint));
    }
    /// Enter command mode with `line` typed and the cursor after it, as if
    /// the user had typed it.
    pub fn start_command(&mut self, ctx: &Context, line: impl Into<String>) {
        self.set_command_mode(true);
        if self.command_mode {
            self.set_query(line);
            self.store_query_cursor(ctx);
            ctx.request_repaint();
        }
    }
    /// Report accepting `selection`, or start the query command it stands for.
    fn accept(&mut self, ctx: &Context, selection: Selection<S::Item>) -> Option<PickerEvent<S::Item>> {
        let command = self
            .command_rows
            .as_ref()
            .and_then(|rows| rows(&selection))
            .and_then(|name| self.query_commands.iter().find(|command| command.name == name))
            .cloned();
        match command {
            None => Some(PickerEvent::Accepted(selection)),
            Some(command) if command.argument.is_some() => {
                self.start_command(ctx, format!("{} ", command.name));
                None
            }
            Some(command) => Some(PickerEvent::Command {
                name: command.name,
                argument: String::new(),
                alternate: false,
            }),
        }
    }
    /// Whether the query box holds a command line rather than a search.
    pub fn command_mode(&self) -> bool {
        self.command_mode
    }
    /// Switch between searching and a command line, keeping each one's text
    /// for the next switch. Command mode needs registered commands.
    pub fn set_command_mode(&mut self, on: bool) {
        if on == self.command_mode || (on && self.query_commands.is_empty()) {
            return;
        }
        self.dismiss_command_output();
        self.leave_preview_focus();
        let restored = if on {
            std::mem::take(&mut self.command_line)
        } else {
            self.command_line = std::mem::take(&mut self.query);
            self.searched_query.clone()
        };
        self.command_mode = on;
        self.command_error = None;
        self.command_selected = 0;
        self.dismissed_completion = None;
        self.suggestions.clear();
        self.suggestion_selected = 0;
        self.focus_query = true;
        // The search query is restored as it was searched, so this does not
        // search again.
        self.set_query(restored);
        self.query_cursor = self.query.len();
    }
    fn search_query(&mut self) {
        self.searched_query = self.query.clone();
        self.revision = self.session.set_query(self.query.clone());
        self.selected = 0;
        self.visible_start = 0;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            preview.document_selection = None;
        }
        // Clearing the published document identity also invalidates its request key.
        // The same selected item must be republished for copy/quick-select access.
        self.preview_key = None;
        // Keep old rows and preview painted while the new query runs.
        // selection() still rejects acceptance until the revision matches.
    }
    /// Change sources without clearing the presented rows or preview. Stale
    /// selections remain disabled until the replacement has results or finishes.
    pub fn replace_source<P: SearchSnapshotProvider<S> + ?Sized>(
        &mut self,
        ctx: &Context,
        provider: Arc<P>,
        query: impl Into<String>,
    ) {
        self.leave_preview_focus();
        self.session.stop();
        // A new source starts with a search.
        self.command_mode = false;
        self.query = query.into();
        self.query_cursor = self.query.len();
        self.command_error = None;
        self.searched_query = self.query.clone();
        self.session = Session::new(
            provider,
            self.searched_query.clone(),
            self.config.sort,
            ctx.clone(),
        );
        self.revision = 0;
        self.transition_pending = true;
        if let Some(displayed) = &mut self.displayed {
            displayed.revision = u64::MAX;
        }
        if let Some(preview) = &self.preview {
            self.preview = Some(preview.restart_retaining_document(ctx.clone()));
        }
        self.preview_key = None;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            preview.document_selection = None;
        }
        self.completion_snapshot = None;
        self.suggestions.clear();
        self.suggestion_selected = 0;
        self.dismissed_completion = None;
        self.close_palette();
        self.error = None;
        self.closed = false;
        self.focus_query = true;
        self.copied_until = None;
        self.store_query_cursor(ctx);
        ctx.request_repaint();
    }

    /// Release a retained copy layout so the provider can wrap at the new size.
    pub fn reset_preview_layout(&mut self, ctx: &Context) {
        self.leave_preview_focus();
        self.preview_viewport = None;
        self.preview_key = None;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            // Row offsets belong to the old layout. Request an initial page
            // around the same candidate rather than reusing that offset.
            preview.clear();
        }
        ctx.request_repaint();
    }

    /// Discard old results, selection and preview copy state before a source
    /// reload. The query is retained; call replace_source to restart searching.
    pub fn clear_for_source_reload(&mut self, ctx: &Context) {
        self.leave_preview_focus();
        self.session.stop();
        if let Some(preview) = self.preview.take() {
            preview.stop();
        }
        self.displayed = None;
        self.revision = self.revision.wrapping_add(1);
        self.selected = 0;
        self.visible_start = 0;
        self.preview_key = None;
        self.scroll_offset = 0;
        self.preview_rect = None;
        self.completion_snapshot = None;
        self.suggestions.clear();
        self.suggestion_selected = 0;
        self.dismissed_completion = None;
        self.copied_until = None;
        self.transition_pending = true;
        self.result_actions_enabled = false;
        self.store_query_cursor(ctx);
        ctx.request_repaint();
    }
    /// Reserve preview space for a native host renderer without a worker.
    pub fn set_external_preview(&mut self, enabled: bool) {
        self.external_preview = enabled;
    }
    pub fn set_preview_provider(
        &mut self,
        ctx: &Context,
        provider: Arc<dyn PreviewProvider<S::Item>>,
    ) {
        self.leave_preview_focus();
        let old = self.preview.take();
        let mut next = PreviewWorker::new(provider, ctx.clone());
        if let Some(old) = old {
            old.stop();
            next.document = old.document.clone();
            next.document_scroll_offset = old.document_scroll_offset;
        }
        self.preview = Some(next);
        self.preview_key = None;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            preview.document_selection = None;
        }
    }
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }
    /// The query box's text: the search, or the command line in command mode.
    pub fn query(&self) -> &str {
        &self.query
    }
    /// Set the query box's text. In command mode this is the command line.
    /// With commands registered, a search becoming `> ` or `: ` (typed or pasted
    /// into an empty query) switches to command mode, with what follows as
    /// the command line.
    pub fn set_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query == self.query {
            return;
        }
        let shortcut = !self.command_mode
            && !self.query_commands.is_empty()
            && (self.query.is_empty() || query_command::shortcut_started(&self.query));
        if shortcut && let Some(line) = query_command::strip_shortcut(&query) {
            self.command_line = line.to_owned();
            self.query = self.searched_query.clone();
            self.set_command_mode(true);
            return;
        }
        self.leave_preview_focus();
        self.query = query;
        self.query_cursor = self.query.len();
        self.suggestion_selected = 0;
        self.dismissed_completion = None;
        self.command_error = None;
        self.command_selected = 0;
        // A `>` or `:` typed into an empty search may start a shortcut; it
        // is searched once something other than a space follows.
        let shortcut_started = shortcut && query_command::shortcut_started(&self.query);
        if !self.command_mode && !shortcut_started && self.query != self.searched_query {
            self.search_query();
        }
    }
    pub fn completions(&self, cursor: usize) -> Vec<nfm_search_core::store::SearchCompletion> {
        if !self.query.is_char_boundary(cursor) {
            return Vec::new();
        }
        // Command mode lists its commands in the results area instead.
        if self.command_mode() {
            return Vec::new();
        }
        self.completion_snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| {
                snapshot.completions(&self.query, cursor)
            })
    }
    /// The commands command mode lists: those whose names match the name
    /// typed, best first, or once a space ends the name, the one it names.
    fn listed_commands(&self) -> Vec<&QueryCommand> {
        let line = query_command::parse(&self.query);
        if line.argument.is_some() {
            self.query_commands
                .iter()
                .filter(|command| command.name == line.name)
                .collect()
        } else {
            query_command::matching(&self.query_commands, line.name)
        }
    }
    /// The host's choices for the command line, listed under its command.
    fn command_choices(&self) -> &[CommandChoice] {
        self.previewed_command
            .as_ref()
            .filter(|(line, _)| *line == self.query)
            .and_then(|(_, hint)| hint.as_ref())
            .map_or(&[], |hint| hint.choices.as_slice())
    }
    /// Rows in command mode: the listed commands, then the choices.
    fn command_rows_len(&self) -> usize {
        self.listed_commands().len() + self.command_choices().len()
    }
    fn move_command_selection(&mut self, delta: isize) {
        let len = self.command_rows_len();
        self.command_selected = self
            .command_selected
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
    }
    /// Tab or Enter (`run`) on listed command `row`: type its name, then a
    /// space when it takes an argument. Enter runs a command without one,
    /// and runs the command line once the name is typed. Returns the event
    /// and whether the command line should run.
    fn pick_command(
        &mut self,
        ctx: &Context,
        row: usize,
        run: bool,
        alternate: bool,
    ) -> (Option<PickerEvent<S::Item>>, bool) {
        let line = query_command::parse(&self.query);
        let listed = self.listed_commands();
        // A choice: Ctrl+Enter copies its value; Enter or Tab makes it the
        // argument.
        if let Some(choice) = row
            .checked_sub(listed.len())
            .and_then(|choice| self.command_choices().get(choice))
            .cloned()
        {
            if alternate {
                ctx.copy_text(choice.value.clone());
                self.notify_copied_value(ctx, &choice.value);
            } else {
                self.set_query(format!("{} {}", line.name, choice.value));
                self.store_query_cursor(ctx);
                ctx.request_repaint();
            }
            return (None, false);
        }
        let command = line
            .argument
            .is_none()
            .then(|| listed.get(row.min(listed.len().saturating_sub(1))).cloned())
            .flatten()
            .cloned();
        match command {
            None => (None, run),
            Some(command) if command.argument.is_some() => {
                self.set_query(format!("{} ", command.name));
                self.store_query_cursor(ctx);
                ctx.request_repaint();
                (None, false)
            }
            Some(command) => {
                self.set_query(command.name.clone());
                self.store_query_cursor(ctx);
                ctx.request_repaint();
                let event = run.then(|| PickerEvent::Command {
                    name: command.name,
                    argument: String::new(),
                    alternate,
                });
                (event, false)
            }
        }
    }
    pub fn apply_completion(
        &mut self,
        completion: &nfm_search_core::store::SearchCompletion,
    ) -> bool {
        let range = completion.replace.clone();
        if range.start > range.end
            || range.end > self.query.len()
            || !self.query.is_char_boundary(range.start)
            || !self.query.is_char_boundary(range.end)
        {
            return false;
        }
        let mut query = self.query.clone();
        query.replace_range(range.clone(), &completion.replacement);
        self.set_query(query);
        self.query_cursor = range.start + completion.replacement.len();
        self.focus_query = true;
        true
    }
    fn refresh_completions(&mut self) {
        if self
            .dismissed_completion
            .as_ref()
            .is_some_and(|(query, cursor)| query == &self.query && *cursor == self.query_cursor)
        {
            self.suggestions.clear();
            return;
        }
        let suggestions = self.completions(self.query_cursor);
        if suggestions != self.suggestions {
            self.suggestion_selected = self
                .suggestion_selected
                .min(suggestions.len().saturating_sub(1));
            self.suggestions = suggestions;
        }
    }
    fn store_query_cursor(&self, ctx: &Context) {
        let id = self.id.with("query");
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) {
            let cursor = self.query[..self.query_cursor.min(self.query.len())]
                .chars()
                .count();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            state.store(ctx, id);
        }
    }
    fn completion_ui(&mut self, ctx: &Context, bounds: egui::Rect, appearance: &Appearance) {
        if self.suggestions.is_empty() || !self.query_rect.is_positive() {
            return;
        }
        let row_height = appearance.typography.preview_row_height(ctx);
        let visible = self
            .suggestions
            .len()
            .min(6)
            .min(((bounds.height() - 16.0) / row_height).floor().max(1.0) as usize);
        let width = self
            .suggestions
            .iter()
            .map(|s| {
                ctx.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(
                            completion_label(s),
                            appearance.typography.normal.clone(),
                            appearance.palette.text,
                        )
                        .size()
                        .x
                })
            })
            .fold(0.0_f32, f32::max);
        let width = (width + 24.0)
            .max(280.0)
            .min(self.query_rect.width())
            .min(bounds.width())
            .max(1.0);
        let height = visible as f32 * row_height + 16.0;
        let token_start = self.suggestions[self.suggestion_selected]
            .replace
            .start
            .min(self.query.len());
        let prefix = &self.query[..token_start];
        let prefix_width = ctx.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(
                    prefix.into(),
                    appearance.typography.normal.clone(),
                    appearance.palette.text,
                )
                .size()
                .x
        });
        let x = (self.query_text_pos.x + prefix_width)
            .clamp(bounds.left(), (bounds.right() - width).max(bounds.left()));
        let y = match self.config.layout {
            Layout::QueryBottom => (self.query_rect.top() - height - 4.0).max(bounds.top()),
            Layout::QueryTop => {
                (self.query_rect.bottom() + 4.0).min((bounds.bottom() - height).max(bounds.top()))
            }
        };
        let first = self
            .suggestion_selected
            .saturating_sub(visible - 1)
            .min(self.suggestions.len() - visible);
        let mut chosen = None;
        let popup_id = self.id.with("autocomplete");
        let previous_size = ctx.memory(|memory| memory.area_rect(popup_id).map(|rect| rect.size()));
        let area = egui::Area::new(popup_id)
            .order(egui::Order::Tooltip)
            .fade_in(false)
            .fixed_pos(egui::pos2(x, y))
            .show(ctx, |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(bounds));
                egui::Frame::new()
                    .fill(appearance.palette.background)
                    .stroke(egui::Stroke::new(PANEL_BORDER, appearance.palette.divider))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width((width - 24.0).max(1.0));
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for index in first..first + visible {
                            let suggestion = &self.suggestions[index];
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2((width - 24.0).max(1.0), row_height),
                                egui::Sense::click(),
                            );
                            let selected = index == self.suggestion_selected;
                            if selected {
                                ui.painter().rect_filled(
                                    rect,
                                    6,
                                    appearance.palette.selection_background,
                                );
                            }
                            let mut job = highlighted_layout(
                                &suggestion.text,
                                &suggestion.positions,
                                selected,
                                appearance,
                            );
                            if !suggestion.detail.is_empty() {
                                job.append(
                                    &format!("{DETAIL_GAP}{}", suggestion.detail),
                                    0.0,
                                    egui::TextFormat::simple(
                                        appearance.typography.normal.clone(),
                                        appearance.palette.muted,
                                    ),
                                );
                            }
                            let galley = ui.painter().layout_job(job);
                            ui.painter().with_clip_rect(rect.intersect(bounds)).galley(
                                egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
                                galley,
                                appearance.palette.text,
                            );
                            if response.clicked() {
                                chosen = Some(suggestion.clone());
                            }
                        }
                    });
            });
        if previous_size.is_none_or(|size| {
            (size.x - area.response.rect.width()).abs() > 0.5
                || (size.y - area.response.rect.height()).abs() > 0.5
        }) {
            ctx.request_discard("resolve autocomplete size before presentation");
        }
        if let Some(completion) = chosen {
            if self.apply_completion(&completion) {
                self.store_query_cursor(ctx);
                self.refresh_completions();
                ctx.request_repaint();
            }
        }
    }
    /// The command a finished command-line name names.
    fn typed_command(&self) -> Option<(&QueryCommand, &str)> {
        if !self.command_mode() {
            return None;
        }
        let line = query_command::parse(&self.query);
        let argument = line.argument?;
        let command = self
            .query_commands
            .iter()
            .find(|command| command.name == line.name)?;
        Some((command, argument))
    }
    /// Dim text after the command line: why Enter did not run it, in the
    /// error color, the rest of the selected command's name while one is
    /// typed, `<argument>` while a command's argument is empty, or the
    /// host's preview of the line.
    fn ghost_text(&self, appearance: &Appearance) -> Option<(String, egui::Color32)> {
        if !self.command_mode {
            return None;
        }
        if let Some(error) = &self.command_error {
            // Apart from the text, unless it ends with the space after a name.
            let gap = if self.query.is_empty() || self.query.ends_with(' ') {
                ""
            } else {
                "  "
            };
            return Some((format!("{gap}{error}"), appearance.palette.error));
        }
        let line = query_command::parse(&self.query);
        if line.argument.is_none() {
            // The rest of the selected command's name, which Right completes.
            let command = *self.listed_commands().get(self.command_selected)?;
            let rest = name_rest(&command.name, line.name)?;
            return Some((rest.to_owned(), appearance.palette.muted));
        }
        let (command, argument) = self.typed_command()?;
        if !argument.is_empty() {
            let (line, hint) = self.previewed_command.as_ref()?;
            return (*line == self.query)
                .then(|| hint.as_ref())
                .flatten()
                .map(|hint| (format!("  {}", hint.text), appearance.palette.muted));
        }
        let spec = command.argument.as_ref()?;
        Some((format!("<{}>", spec.name), appearance.palette.muted))
    }
    /// Add host actions to the shared keyboard help overlay.
    /// True while keyboard input navigates and selects preview text.
    /// Set the preview cursor scroll margin. Zero disables the margin.
    pub fn set_cursor_scroll_padding(&mut self, rows: u32) {
        self.cursor_scroll_padding = rows;
        if let Some(viewport) = &mut self.preview_viewport {
            viewport.set_scroll_padding(rows);
        }
    }

    /// Add a resolver for icon references other than `glyph:` in card icon
    /// slots (see [`crate::icons`]). Resolvers are asked in the order they
    /// were added. Without any, only glyphs are drawn.
    pub fn add_icon_resolver(&mut self, resolver: Arc<dyn IconResolver>) {
        self.icons.add_resolver(resolver);
    }

    /// Draw an icon at the left of each one-line result. `icon` maps the
    /// result's item to an icon reference (see [`crate::icons`]); items that
    /// return `None` keep the column empty so text stays aligned. Card rows
    /// use their template's icon slots instead. Build references in host
    /// code: a reference taken from item text lets data pick what a host
    /// resolver reads. The function stays until replaced, also across
    /// [`Self::replace_source`].
    pub fn set_row_icon(&mut self, icon: Option<RowIcon<S::Item>>) {
        self.row_icon = icon;
    }

    pub fn has_row_icon(&self) -> bool {
        self.row_icon.is_some()
    }

    /// Show line numbers in a gutter left of preview text. Wrapping providers
    /// receive the narrower width, so changing this re-requests the preview.
    pub fn set_preview_line_numbers(&mut self, numbers: PreviewLineNumbers) {
        self.config.preview_line_numbers = numbers;
    }

    /// The current style, which the line-number commands change; hosts can
    /// read it to keep the user's choice.
    pub fn preview_line_numbers(&self) -> PreviewLineNumbers {
        self.config.preview_line_numbers
    }

    /// Wrap plain-text previews at the pane width, or let them scroll sideways.
    /// Changing it lays the preview out again from its first row.
    pub fn set_preview_wrap(&mut self, ctx: &Context, wrap: bool) {
        if self.config.preview_wrap == wrap {
            return;
        }
        self.config.preview_wrap = wrap;
        self.reset_preview_layout(ctx);
    }

    /// Whether plain-text previews wrap; the wrap command changes it.
    pub fn preview_wrap(&self) -> bool {
        self.config.preview_wrap
    }

    /// Gutter width in cells: the widest line number plus a separator column.
    fn preview_gutter_columns(&self) -> usize {
        if self.config.preview_line_numbers == PreviewLineNumbers::Off {
            return 0;
        }
        // Relative distances never exceed the line count, so it sizes every mode.
        let digits = self.preview_line_count.max(1).ilog10() as usize + 1;
        digits.max(MIN_LINE_NUMBER_DIGITS) + 1
    }

    /// Suspend result acceptance while a host is rebuilding positional payloads.
    /// Preview navigation and copying remain available.
    pub fn set_result_actions_enabled(&mut self, enabled: bool) {
        self.result_actions_enabled = enabled;
    }

    pub fn preview_focused(&self) -> bool {
        self.preview_focus
    }

    /// Enter copy mode on the preview, as Ctrl+W does, unless it is already
    /// focused. Its cursor starts at the document's anchor when one is visible.
    /// Returns whether the preview is focused: not while it has nothing to show.
    pub fn focus_preview(&mut self, ctx: &Context) -> bool {
        if !self.preview_focus {
            self.toggle_preview_focus(ctx);
        }
        self.preview_focus
    }

    // Document changes discard the viewport; focus changes keep it in place.
    fn leave_preview_focus(&mut self) {
        self.deactivate_preview_focus();
        self.preview_viewport = None;
    }

    fn deactivate_preview_focus(&mut self) {
        if let Some(viewport) = &mut self.preview_viewport {
            self.scroll_offset = viewport.top as usize;
            viewport.hide_decorations();
        }
        self.preview_focus = false;
        self.focus_query = true;
    }

    fn toggle_preview_focus(&mut self, ctx: &Context) {
        if self.preview_focus {
            self.deactivate_preview_focus();
            return;
        }
        self.ensure_preview_viewport();
        if self.preview_viewport.is_some() {
            self.preview_focus = true;
            self.focus_query = false;
            self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
            self.suggestions.clear();
            ctx.memory_mut(|memory| memory.request_focus(self.id.with("preview-copy")));
        }
    }

    fn ensure_preview_viewport(&mut self) {
        // A command's output is laid out with the pane's width when shown.
        if self.preview_viewport.is_some() || self.command_mode {
            return;
        }
        if !self.config.preview_visible
            || self.visible_preview_rows == 0
            || self.palette.is_some()
            || !self.current_results()
        {
            return;
        }
        let Some(worker) = &self.preview else {
            return;
        };
        let Ok(Some(document)) = worker.copy_document() else {
            return;
        };
        let mut copy = crate::preview_copy::PreviewCopyMode::host(
            document,
            worker.document_scroll_offset,
            self.visible_preview_rows,
        );
        if let Some(viewport) = &mut copy {
            viewport.set_scroll_padding(self.cursor_scroll_padding);
        }
        self.preview_viewport = copy;
    }

    pub fn keybinding_help_visible(&self) -> bool {
        self.palette.is_some()
    }
    pub fn preview_visible(&self) -> bool {
        self.config.preview_visible
    }
    /// Update presentation when the host switches between picker views.
    pub fn set_presentation(
        &mut self,
        result_rows: usize,
        preview_rows: usize,
        preview_visible: bool,
    ) {
        self.config.result_rows = result_rows.clamp(1, 256);
        self.config.preview_rows = preview_rows.clamp(1, 256);
        if !preview_visible {
            self.leave_preview_focus();
        }
        self.config.preview_visible = preview_visible;
        self.full_pane_preview = false;
    }
    /// Has no effect: the command palette lists only commands. Give a custom
    /// command a `PickerBinding::description` to list it.
    #[deprecated(
        note = "the command palette lists only commands; describe custom commands with PickerBinding::description"
    )]
    pub fn set_keybinding_help(&mut self, _bindings: Vec<(String, String)>) {}

    /// Run a command at the start of the next frame, as if its key had been
    /// pressed. A custom command is reported as [`PickerEvent::CustomAction`].
    pub fn execute(&mut self, ctx: &Context, action: PickerAction) {
        self.pending_actions.push(action);
        ctx.request_repaint();
    }

    /// Every palette row: built-in commands, then the host's described custom
    /// commands.
    fn palette_rows(&self) -> Vec<PaletteRow> {
        let keys = |action: PickerAction| {
            self.config
                .bindings
                .iter()
                .filter(|binding| binding.action == action)
                .map(PickerBinding::shortcut_label)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut rows: Vec<PaletteRow> = PickerAction::BUILT_IN
            .into_iter()
            // Command mode needs commands to type.
            .filter(|action| {
                *action != PickerAction::ToggleCommandMode || !self.query_commands.is_empty()
            })
            .map(|action| PaletteRow {
                keys: keys(action),
                title: action.title().unwrap_or_default().into(),
                action,
            })
            .collect();
        for binding in &self.config.bindings {
            let (PickerAction::Custom(_), Some(title)) = (binding.action, &binding.description)
            else {
                continue;
            };
            if !rows.iter().any(|row| row.action == binding.action) {
                rows.push(PaletteRow {
                    keys: keys(binding.action),
                    title: title.clone(),
                    action: binding.action,
                });
            }
        }
        rows
    }

    fn open_palette(&mut self, ctx: &Context) {
        let rows = self.palette_rows();
        let source = structured_source(
            vec!["Command".into(), "Keys".into()],
            rows.iter()
                .map(|row| vec![row.title.clone(), row.keys.clone()])
                .collect(),
        )
        .expect("palette columns are valid");
        // The palette runs no picker commands of its own. This picker's
        // palette keys close it (see `palette_ui`).
        let bindings = default_bindings()
            .into_iter()
            .map(|binding| PickerBinding {
                action: PickerAction::Unbind,
                ..binding
            })
            .collect();
        let config = PickerConfig {
            layout: Layout::QueryTop,
            title: "Command palette".into(),
            // Every command has a row, so filtering never resizes the palette.
            result_rows: rows.len().max(1),
            bindings,
            preview_visible: false,
            ..PickerConfig::default()
        };
        let picker = Picker::new(self.id.with("command-palette"), ctx, source, config);
        self.palette = Some(Box::new(CommandPalette {
            picker,
            actions: rows.into_iter().map(|row| row.action).collect(),
        }));
        self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
        self.suggestions.clear();
    }

    fn close_palette(&mut self) {
        if let Some(palette) = self.palette.take() {
            let mut picker = palette.picker;
            picker.close();
            self.focus_query = true;
        }
    }

    /// Show the open palette at the top of `bounds`. The chosen command runs
    /// on the next frame, as if from [`Self::execute`].
    fn palette_ui(&mut self, ctx: &Context, bounds: egui::Rect, appearance: &Appearance) {
        if self.palette.is_none() {
            return;
        }
        // This picker's palette keys close the palette, whatever it shows.
        let bindings = &self.config.bindings;
        let toggled = ctx.input_mut(|input| {
            let mut toggled = false;
            let mut events = std::mem::take(&mut input.events).into_iter().peekable();
            while let Some(event) = events.next() {
                if let egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } = &event
                    && bindings.iter().any(|binding| {
                        binding.action == PickerAction::ToggleCommandPalette
                            && binding.trigger == PickerTrigger::Key(*key)
                            && same_binding_modifiers(binding.modifiers, *modifiers)
                    })
                {
                    if *pressed {
                        toggled = true;
                        if matches!(events.peek(), Some(egui::Event::Text(_))) {
                            events.next();
                        }
                    }
                    continue;
                }
                input.events.push(event);
            }
            toggled
        });
        if toggled {
            self.close_palette();
            return;
        }
        let Some(palette) = &mut self.palette else {
            return;
        };
        let mut placement = Placement::new(bounds.intersect(ctx.content_rect()));
        placement.position = crate::Position::Top;
        placement.margin = PALETTE_MARGIN;
        let output = palette.picker.show(ctx, placement, appearance, false);
        ctx.move_to_top(egui::LayerId::new(
            egui::Order::Foreground,
            palette.picker.id,
        ));
        let chosen = match output.event {
            Some(PickerEvent::Accepted(selection)) => palette.actions.get(selection.index).copied(),
            Some(PickerEvent::Cancelled) => None,
            _ => return,
        };
        self.close_palette();
        if let Some(action) = chosen {
            self.execute(ctx, action);
        }
    }

    pub fn notify_copied(&mut self, ctx: &Context) {
        let value = self
            .selection()
            .map(|selection| selection.text)
            .unwrap_or_default();
        self.notify_copied_value(ctx, &value);
    }
    /// Confirm the actual clipboard value when it differs from the displayed candidate.
    pub fn notify_copied_value(&mut self, ctx: &Context, value: &str) {
        self.copy_notice = if value.is_empty() {
            "Copied".into()
        } else {
            format!(
                "Copied: {}",
                value
                    .replace('\r', "\\r")
                    .replace('\n', "\\n")
                    .replace('\t', "\\t")
            )
        };
        self.copied_until = Some(Instant::now() + Duration::from_secs(2));
        ctx.request_repaint();
        ctx.request_repaint_after(Duration::from_secs(2));
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn close(&mut self) {
        self.leave_preview_focus();
        self.closed = true;
        self.session.stop();
        if let Some(preview) = &self.preview {
            preview.stop();
        }
    }
    /// Whether the shown results are for the search in the query box. In
    /// command mode the box holds a command line, and the results are those
    /// of the search before it.
    fn current_results(&self) -> bool {
        let searching = if self.command_mode {
            &self.searched_query
        } else {
            &self.query
        };
        self.displayed
            .as_ref()
            .is_some_and(|d| d.revision == self.revision && &d.query == searching)
    }
    fn displayed_selection(&self) -> Option<Selection<S::Item>> {
        let displayed = self.displayed.as_ref()?;
        let result = displayed.output.results.get(self.selected)?;
        Some(Selection {
            item: displayed.snapshot.item(result.node_index)?,
            text: result.path.clone(),
            index: result.node_index,
            source_version: displayed.snapshot.version(),
            snapshot: displayed.snapshot.clone(),
        })
    }
    /// Monotonic timestamps for the initial search and first nonempty result.
    pub fn search_timings(&self) -> crate::SearchTimings {
        self.session.timings()
    }
    /// Optional host wake path independent of egui's coalesced repaint callback.
    pub fn set_search_wake_callback(
        &self,
        callback: Arc<dyn Fn(crate::SearchTimings) + Send + Sync>,
    ) {
        self.session.set_wake_callback(callback);
    }
    pub fn set_bindings(&mut self, bindings: Vec<PickerBinding>) {
        self.config.bindings = resolved_bindings(bindings);
    }
    /// Effective placement after built-in actions, for hosts that resize a native window.
    pub fn effective_position(&self, configured: crate::Position) -> crate::Position {
        self.placement_override.unwrap_or(configured)
    }
    fn double_click_action(&self) -> Option<PickerAction> {
        self.config
            .bindings
            .iter()
            .find(|binding| binding.trigger == PickerTrigger::DoubleClick)
            .map(|binding| binding.action)
    }
    /// Whether a bound key runs `action` now. Commands that would take keys
    /// copy mode uses for itself leave them to copy mode while it is focused.
    fn binding_applies(&self, action: PickerAction) -> bool {
        use PickerAction::*;
        let focused = self.preview_focused();
        // A command's output takes the preview's keys, except copying a
        // result, which there is none of.
        if self.command_output.is_some() && matches!(action, CopySelection | Custom(_)) {
            return false;
        }
        // Command mode shows commands, not the results and preview these
        // act on; their keys go to the command line instead.
        if self.command_mode
            && self.command_output.is_none()
            && matches!(
                action,
                TogglePreviewFocus
                    | QuickSelect
                    | CopySelection
                    | CopyPreviewDocument
                    | ScrollPreviewPageUp
                    | ScrollPreviewPageDown
                    | ScrollPreviewLineUp
                    | ScrollPreviewLineDown
                    | Custom(_)
            )
        {
            return false;
        }
        match action {
            CopySelection
            | ScrollPreviewPageUp
            | ScrollPreviewPageDown
            | ScrollPreviewLineUp
            | ScrollPreviewLineDown => !focused,
            CopyPreviewDocument => focused,
            ToggleCommandMode => !focused && !self.query_commands.is_empty(),
            // Ctrl+P moves to the previous match while copy-mode search is open.
            TogglePreview => {
                !(focused
                    && self
                        .preview_viewport
                        .as_ref()
                        .is_some_and(|viewport| viewport.search_input_active()))
            }
            _ => true,
        }
    }

    /// Run a command. Returns a custom command's id for the host event, and
    /// sets `copy` for `CopySelection`.
    fn run_action(
        &mut self,
        action: PickerAction,
        ctx: &Context,
        configured: crate::Position,
        copy: &mut bool,
    ) -> Option<usize> {
        use PickerAction::*;
        match action {
            Custom(id) => return Some(id),
            Unbind => {}
            ToggleCommandPalette => {
                if self.palette.is_some() {
                    self.close_palette();
                } else {
                    self.open_palette(ctx);
                }
            }
            TogglePreview => {
                self.config.preview_visible = !self.config.preview_visible;
                self.full_pane_preview = false;
                if let Some(shown) = &mut self.command_output {
                    shown.opened_preview = false;
                }
                if !self.config.preview_visible {
                    self.leave_preview_focus();
                    if let Some(preview) = &mut self.preview {
                        preview.clear();
                    }
                    self.preview_key = None;
                }
            }
            TogglePreviewFocus => self.toggle_preview_focus(ctx),
            QuickSelect => self.quick_select(ctx),
            CopySelection => *copy = true,
            CopyPreviewDocument if self.copy_command_output(ctx) => {
                ctx.input_mut(|input| {
                    input
                        .events
                        .retain(|event| !matches!(event, egui::Event::Copy))
                });
            }
            CopyPreviewDocument => {
                self.ensure_preview_viewport();
                if let Some(viewport) = &self.preview_viewport {
                    viewport.copy_all(ctx);
                    // Egui can send Copy with the chord; it must not also
                    // copy the selected result.
                    ctx.input_mut(|input| {
                        input
                            .events
                            .retain(|event| !matches!(event, egui::Event::Copy))
                    });
                }
            }
            ScrollPreviewPageUp => self.scroll_preview(-1),
            ScrollPreviewPageDown => self.scroll_preview(1),
            ScrollPreviewLineUp => self.scroll_preview_rows(-1),
            ScrollPreviewLineDown => self.scroll_preview_rows(1),
            // As in Vim, absolute and relative numbers are independent
            // switches; with both on the gutter is hybrid.
            TogglePreviewLineNumbers | ToggleRelativePreviewLineNumbers => {
                use PreviewLineNumbers as Style;
                let (number, relative) = match self.config.preview_line_numbers {
                    Style::Off => (false, false),
                    Style::Absolute => (true, false),
                    Style::Relative => (false, true),
                    Style::Hybrid => (true, true),
                };
                let (number, relative) = if action == TogglePreviewLineNumbers {
                    (!number, relative)
                } else {
                    (number, !relative)
                };
                self.set_preview_line_numbers(match (number, relative) {
                    (false, false) => Style::Off,
                    (true, false) => Style::Absolute,
                    (false, true) => Style::Relative,
                    (true, true) => Style::Hybrid,
                });
            }
            TogglePreviewWrap => self.set_preview_wrap(ctx, !self.config.preview_wrap),
            ToggleCommandMode => {
                self.set_command_mode(!self.command_mode);
                self.store_query_cursor(ctx);
                ctx.request_repaint();
            }
            ToggleFullPane => {
                self.placement_override = if self.placement_override.is_some() {
                    None
                } else {
                    Some(if configured == crate::Position::FullPane {
                        crate::Position::Center
                    } else {
                        crate::Position::FullPane
                    })
                };
                // Full pane gives its extra height to the preview, so show an
                // available preview that is hidden, and hide it again on exit.
                let has_preview = self.preview_available();
                if self.effective_position(configured) == crate::Position::FullPane {
                    if has_preview && !self.config.preview_visible {
                        self.config.preview_visible = true;
                        self.full_pane_preview = true;
                    }
                } else if std::mem::take(&mut self.full_pane_preview) {
                    self.config.preview_visible = false;
                }
                self.reset_preview_layout(ctx);
            }
        }
        None
    }

    /// Open Quick Select on the preview, or cancel an open one.
    fn quick_select(&mut self, ctx: &Context) {
        // Some hosts emit a text space alongside Alt+Space. It belongs to the
        // shortcut and must never edit the query on cancellation.
        ctx.input_mut(|input| {
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Text(text) if text == " "))
        });
        self.ensure_preview_viewport();
        let from_copy = self.preview_focus;
        let Some(viewport) = &mut self.preview_viewport else {
            return;
        };
        if viewport.quick_select_active() {
            if viewport.cancel_quick_select() {
                self.deactivate_preview_focus();
            }
        } else if viewport.open_quick_select(from_copy, self.visible_preview_rows) {
            self.preview_focus = true;
            self.focus_query = false;
            self.suggestions.clear();
            ctx.memory_mut(|memory| memory.request_focus(self.id.with("preview-copy")));
        }
    }

    pub fn selection(&self) -> Option<Selection<S::Item>> {
        self.current_results()
            .then(|| self.displayed_selection())
            .flatten()
    }
    fn poll(&mut self) {
        if let Some(update) = self.session.take()
            && update.revision == self.revision
            && update.query == self.query
            && (!self.transition_pending || update.done || !update.output.results.is_empty())
        {
            if self.transition_pending && update.output.results.is_empty() {
                if let Some(preview) = &mut self.preview {
                    preview.clear();
                }
            }
            self.transition_pending = false;
            let same_query = self
                .displayed
                .as_ref()
                .is_some_and(|d| d.revision == update.revision);
            let stable_indices = self.displayed.as_ref().is_some_and(|d| {
                d.snapshot.version() == update.snapshot.version() || update.append_only
            });
            let old_index = self
                .displayed
                .as_ref()
                .and_then(|d| d.output.results.get(self.selected))
                .map(|r| r.node_index);
            self.selected = if same_query && stable_indices {
                old_index
                    .and_then(|index| {
                        update
                            .output
                            .results
                            .iter()
                            .position(|r| r.node_index == index)
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            let new_index = update
                .output
                .results
                .get(self.selected)
                .map(|r| r.node_index);
            // A growing preview's new version stays in copy mode, which
            // keeps the document it opened on until it is left.
            let grows = self.preview_follows_end && same_query && old_index == new_index;
            if old_index != new_index || !same_query || (!stable_indices && !grows) {
                self.leave_preview_focus();
            }
            self.completion_snapshot = Some(update.snapshot.clone());
            self.displayed = Some(update);
            self.keep_selected_visible();
        }
        if let Some(preview) = &mut self.preview {
            let loading = preview.loading;
            preview.poll();
            if loading && !preview.loading && !self.preview_focus {
                self.preview_viewport = None;
            }
            if loading && !preview.loading && preview.document.as_ref().is_some_and(|d| d.is_ok()) {
                self.scroll_offset = preview.document_scroll_offset;
                if let Some(key) = &mut self.preview_key {
                    key.4 = self.scroll_offset;
                }
            }
        }
    }
    fn keep_selected_visible(&mut self) {
        if self.selected < self.visible_start {
            self.visible_start = self.selected;
        } else if self.selected >= self.visible_start + self.visible_result_rows {
            self.visible_start = self.selected + 1 - self.visible_result_rows;
        }
    }
    fn move_selection(&mut self, delta: isize) {
        let len = self
            .displayed
            .as_ref()
            .map_or(0, |d| d.output.results.len());
        if len > 0 {
            let next = self.selected.saturating_add_signed(delta).min(len - 1);
            if next != self.selected {
                self.leave_preview_focus();
                self.selected = next;
                self.scroll_offset = 0;
                if let Some(preview) = &mut self.preview {
                    preview.document_selection = None;
                }
                self.keep_selected_visible();
            }
        }
    }
    /// Scroll the preview by one page in `direction`.
    fn scroll_preview(&mut self, direction: isize) {
        let rows = self.visible_preview_rows.max(1);
        let step = self.preview.as_ref().map_or(rows, |p| p.page_step(rows));
        self.scroll_preview_rows(direction.saturating_mul(step as isize));
    }
    /// Scroll the preview by `delta` rows, stopping at either end.
    fn scroll_preview_rows(&mut self, delta: isize) {
        let rows = self.visible_preview_rows.max(1);
        let total = if self.command_mode {
            self.preview_viewport
                .as_ref()
                .map(|viewport| viewport.total_rows() as usize)
        } else {
            match self.preview.as_ref().and_then(|p| p.document.as_ref()) {
                Some(Ok(Preview::Grid { total_rows, .. })) => Some(*total_rows),
                _ => None,
            }
        };
        if let Some(viewport) = &mut self.preview_viewport {
            viewport.scroll(delta, rows);
            self.scroll_offset = viewport.top as usize;
        } else if let Some(total) = total {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add_signed(delta)
                .min(total.saturating_sub(rows));
        } else {
            return;
        }
        // Scrolling up leaves a growing preview's end; scrolling down to it
        // follows again. Copy-mode scrolling stops a row short of the end.
        if delta < 0 {
            self.preview_following = false;
        } else if total.is_some_and(|total| self.scroll_offset + rows + 1 >= total) {
            self.preview_following = true;
        }
    }
    /// Whether a growing preview has a newer version than the one shown, as
    /// after copy mode, which keeps the document it opened on.
    fn preview_grew(&self) -> bool {
        self.preview_follows_end
            && self
                .preview_key
                .zip(self.selection())
                .is_some_and(|(key, selection)| {
                    key.0 != 0 && key.1 == selection.index && key.0 != selection.source_version
                })
    }
    fn refresh_preview(&mut self, width: f32, appearance: &Appearance) {
        let cell_width = appearance.typography.cell_width.max(1.0);
        let width = width - self.preview_gutter_columns() as f32 * cell_width;
        let columns = (width / cell_width).floor().clamp(1.0, 4096.0) as u16;
        let rows = self.visible_preview_rows.clamp(1, u16::MAX as usize) as u16;
        if self.preview_viewport.is_some() {
            // Passive previews also cache a copy viewport for scrolling. That
            // cache must not suppress width-aware provider requests on resize.
            if self.preview_focus
                || self.preview.is_none()
                || (self
                    .preview_key
                    .is_some_and(|key| key.2 == columns && key.3 == rows)
                    && !self.preview_grew())
            {
                return;
            }
            self.preview_viewport = None;
        }
        if !self.current_results() {
            return;
        }
        let Some(selection) = self.selection() else {
            if self.preview_key.take().is_some()
                && let Some(preview) = &mut self.preview
            {
                preview.clear();
            }
            return;
        };
        let Some(preview) = &mut self.preview else {
            return;
        };
        let foreground = appearance.palette.text;
        let background = appearance.palette.background;
        // Append-only publications do not change an existing selected item's
        // preview. Avoid restarting it for every incoming candidate batch.
        let version = if self.displayed.as_ref().is_some_and(|d| d.append_only) {
            0
        } else {
            selection.source_version
        };
        // A new version of the shown result continues its preview: from the
        // same place, or from the new end while the end was in view.
        let continues = self.preview_follows_end
            && preview
                .document_selection
                .is_some_and(|(shown, index)| index == selection.index && shown != version);
        if continues {
            if self.preview_following {
                // The provider stops at the last page.
                self.scroll_offset = usize::MAX / 2;
            }
        } else if preview.document_selection != Some((version, selection.index)) {
            // Another result's preview starts out following.
            self.preview_following = true;
        }
        let key = (
            version,
            selection.index,
            columns,
            rows,
            self.scroll_offset,
            u32::from_le_bytes(foreground.to_array()),
            u32::from_le_bytes(background.to_array()),
        );
        if self.preview_key != Some(key) {
            self.preview_key = Some(key);
            preview.request(PreviewRequest {
                initial_page: !continues
                    && preview.document_selection != Some((version, selection.index)),
                document_version: version,
                selection,
                columns,
                rows,
                scroll_offset: self.scroll_offset,
                foreground,
                background,
                wrap_text: self.config.preview_wrap,
            });
        }
    }
    /// Whether the picker has a preview pane to show: in command mode, the
    /// pane keeps its space even with no command's output in it.
    fn preview_available(&self) -> bool {
        self.preview.is_some()
            || self.external_preview
            || (self.command_mode && self.command_output.is_some())
    }
    fn busy(&self) -> bool {
        if self.command_mode {
            // Listed commands never load; a command's output does until it
            // is finished.
            return !self.closed
                && self
                    .command_output
                    .as_ref()
                    .is_some_and(|shown| !shown.output.current().2);
        }
        !self.closed
            && (!self.current_results()
                || self.displayed.as_ref().is_none_or(|d| !d.done)
                || (self.visible_preview_rows > 0
                    && self.preview.as_ref().is_some_and(|p| p.loading)))
    }
    fn preview_ui(&mut self, ui: &mut egui::Ui, appearance: &Appearance) {
        // Allocate every row, including loading/empty/short documents, so the
        // query and selected row never move when asynchronous content arrives.
        let height = self.output_pane_height.unwrap_or(
            self.visible_preview_rows as f32 * appearance.typography.preview_row_height(ui.ctx()),
        );
        if self.command_mode && self.command_output.is_none() {
            // Nothing previews a command; the pane keeps its space.
            ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
            return;
        }
        self.preview_rect = Some(
            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), height))
                .intersect(ui.clip_rect()),
        );
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                ui.set_min_height(height);
                ui.set_max_height(height);
                self.ensure_preview_viewport();
                let gutter_columns = self.preview_gutter_columns();
                if gutter_columns == 0 || !self.preview_numbered() {
                    self.preview_content_ui(ui, appearance, PreviewLineNumbers::Off);
                    return;
                }
                // The gutter sits outside the content so copy-mode cell
                // coordinates, overlays and yanked text are unaffected.
                let gutter_width =
                    gutter_columns as f32 * appearance.typography.cell_width.max(1.0);
                let origin = ui.cursor().min;
                let content = egui::Rect::from_min_size(
                    origin + egui::vec2(gutter_width, 0.0),
                    egui::vec2((ui.available_width() - gutter_width).max(0.0), height),
                );
                let style = self.config.preview_line_numbers;
                let numbers = ui
                    .scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(content)
                            .layout(egui::Layout::top_down(egui::Align::LEFT)),
                        |ui| self.preview_content_ui(ui, appearance, style),
                    )
                    .inner;
                paint_line_numbers(
                    ui,
                    egui::Rect::from_min_size(origin, egui::vec2(gutter_width, height)),
                    &numbers,
                    appearance,
                );
                if self.preview_gutter_columns() != gutter_columns {
                    // The next frame requests the preview at the new width.
                    ui.ctx().request_repaint();
                }
            },
        );
    }
    /// Whether the displayed preview takes a line-number gutter: a copy
    /// viewport, or a page that reports its rows' lines.
    fn preview_numbered(&self) -> bool {
        self.preview_viewport.is_some() || self.page_lines().is_some()
    }
    /// Lines of the worker's page, which is painted while there is no viewport.
    fn page_lines(&self) -> Option<&crate::preview::GridLines> {
        if self.command_mode {
            return None;
        }
        match self.preview.as_ref()?.document.as_ref()? {
            Ok(Preview::Grid { lines, .. }) => lines.as_ref(),
            _ => None,
        }
    }
    /// Paint the preview document and return its gutter numbers, one per
    /// viewport row, when `numbers` is not `Off`.
    fn preview_content_ui(
        &mut self,
        ui: &mut egui::Ui,
        appearance: &Appearance,
        numbers: PreviewLineNumbers,
    ) -> Vec<Option<crate::preview_copy::GutterNumber>> {
        let rows = self.visible_preview_rows;
        if let Some(copy) = &mut self.preview_viewport {
            self.preview_rendered = true;
            copy.paint_layers(ui, appearance, rows, self.preview_focus);
            self.scroll_offset = copy.top as usize;
            if numbers == PreviewLineNumbers::Off {
                return Vec::new();
            }
            self.preview_line_count = copy.line_count();
            return copy.line_numbers(rows, numbers, self.preview_focus);
        }
        if self.command_mode {
            // A command's output with nothing in it yet.
            return Vec::new();
        }
        match self
            .preview
            .as_ref()
            .and_then(|worker| worker.document.as_ref())
        {
            Some(Ok(document)) => {
                self.preview_rendered = true;
                preview::paint(ui, document, appearance, rows);
            }
            Some(Err(error)) => {
                ui.colored_label(appearance.palette.error, error);
            }
            None => {}
        }
        // Without a viewport the preview is unfocused, so its numbers are the
        // page's absolute lines.
        let Some(lines) = self
            .page_lines()
            .filter(|_| numbers != PreviewLineNumbers::Off)
        else {
            return Vec::new();
        };
        let total = lines.total;
        let gutter = lines
            .starts
            .iter()
            .map(|line| {
                line.map(|line| crate::preview_copy::GutterNumber {
                    text: (line + 1).to_string(),
                    current: false,
                    left_aligned: false,
                })
            })
            .collect();
        self.preview_line_count = total;
        gutter
    }
    /// Height of one result: a line, or a card when the source has a row template.
    fn result_row_height(&self, ctx: &egui::Context, appearance: &Appearance) -> f32 {
        match self
            .displayed
            .as_ref()
            .and_then(|displayed| displayed.snapshot.row_template())
        {
            Some(template) => card_metrics(ctx, appearance, template).height,
            None => appearance.typography.preview_row_height(ctx),
        }
    }
    fn results_ui(
        &mut self,
        ui: &mut egui::Ui,
        appearance: &Appearance,
        accept: &mut bool,
        double_click: &mut bool,
    ) {
        let row_height = appearance.typography.preview_row_height(ui.ctx());
        if let Some(error) = &self.error {
            let height =
                self.visible_result_rows as f32 * self.result_row_height(ui.ctx(), appearance);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(self.id.with("error_message"))
                    .max_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(error)
                                    .font(appearance.typography.normal.clone())
                                    .color(appearance.palette.error),
                            )
                            .wrap(),
                        );
                    });
            });
            return;
        }
        if self.command_mode {
            self.command_list_ui(ui, appearance);
            return;
        }
        // Rows are rebuilt every frame. Input lines are capped (16 KiB by
        // default), so text and match positions for the visible rows are
        // cheap enough not to cache. The query is the same for every row.
        let effective_query = self
            .displayed
            .as_ref()
            .map(|displayed| displayed.snapshot.effective_query(&displayed.query));
        let template = self
            .displayed
            .as_ref()
            .and_then(|displayed| displayed.snapshot.row_template().cloned());
        let card = template
            .as_ref()
            .map(|template| card_metrics(ui.ctx(), appearance, template));
        let result_height = card.as_ref().map_or(row_height, |card| card.height);
        self.icons.poll();
        let mut first_rect = None;
        for slot in 0..self.visible_result_rows {
            let index = if self.config.layout == Layout::QueryBottom {
                self.visible_start + self.visible_result_rows - 1 - slot
            } else {
                self.visible_start + slot
            };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), result_height),
                egui::Sense::hover(),
            );
            first_rect.get_or_insert(rect);
            let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
            if let Some(displayed) = &self.displayed
                && let Some(result) = displayed.output.results.get(index)
            {
                let response =
                    ui.interact(rect, self.id.with(("row", index)), egui::Sense::click());
                let hovered = response.hovered() && index != self.selected;
                if hovered {
                    painter.rect_filled(rect, 4, appearance.palette.hover_background());
                }
                if index == self.selected {
                    painter.rect_filled(rect, 4, appearance.palette.selection_background);
                    painter.rect_filled(
                        egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 1.5, rect.center().y),
                            egui::vec2(
                                3.0,
                                if card.is_some() {
                                    (rect.height() - 20.0).max(16.0)
                                } else {
                                    16.0
                                },
                            ),
                        ),
                        1.5,
                        appearance.palette.accent,
                    );
                }
                let display = if let (Some(template), Some(card)) = (&template, &card) {
                    let below = if self.config.layout == Layout::QueryBottom {
                        index.checked_sub(1)
                    } else {
                        Some(index + 1)
                    };
                    // The row below this one on screen is the next slot, in
                    // either layout. Its response isn't built yet, so test the
                    // pointer against its rect.
                    let below_rect = rect.translate(egui::vec2(0.0, result_height));
                    let below_hovered = ui.is_enabled() && ui.rect_contains_pointer(below_rect);
                    if slot + 1 < self.visible_result_rows
                        && !hovered
                        && !below_hovered
                        && below.is_some_and(|below| {
                            below < displayed.output.results.len()
                                && below != self.selected
                                && index != self.selected
                        })
                    {
                        let y = rect.bottom() - 0.5;
                        painter.line_segment(
                            [
                                egui::pos2(rect.left() + 12.0, y),
                                egui::pos2(rect.right() - 12.0, y),
                            ],
                            egui::Stroke::new(
                                1.0,
                                crate::appearance::blend(
                                    appearance.palette.background,
                                    appearance.palette.divider,
                                    0.3,
                                ),
                            ),
                        );
                    }
                    let fields = displayed
                        .snapshot
                        .row_fields(result.node_index)
                        .unwrap_or_default();
                    paint_card(
                        ui.ctx(),
                        &painter,
                        rect,
                        template,
                        card,
                        &fields,
                        effective_query.as_deref().unwrap_or(""),
                        index == self.selected,
                        &mut self.icons,
                        appearance,
                    )
                } else {
                    let display = displayed
                        .snapshot
                        .display_text(result.node_index, &displayed.query)
                        .unwrap_or_else(|| result.path.clone());
                    let mut inset = 0.0;
                    if let Some(row_icon) = &self.row_icon {
                        let size = (rect.height() - 2.0).max(1.0);
                        let square = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 12.0, rect.center().y - size / 2.0),
                            egui::vec2(size, size),
                        );
                        if let Some(reference) = displayed
                            .snapshot
                            .item(result.node_index)
                            .and_then(|item| row_icon(&item))
                        {
                            paint_icon(
                                ui.ctx(),
                                &painter,
                                square,
                                &reference,
                                &mut self.icons,
                                appearance,
                            );
                        }
                        inset = size + CARD_ICON_GAP;
                    }
                    paint_result_line(
                        ui,
                        &painter,
                        rect,
                        inset,
                        &display,
                        effective_query.as_deref().unwrap_or(""),
                        index == self.selected,
                        appearance,
                    );
                    display
                };
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        index == self.selected,
                        &display,
                    )
                });
                if response.clicked() {
                    self.leave_preview_focus();
                    self.selected = index;
                    self.focus_query = true;
                    self.scroll_offset = 0;
                    if let Some(preview) = &mut self.preview {
                        preview.document_selection = None;
                    }
                    if self.config.accept_on_click && self.double_click_action().is_none() {
                        *accept = true;
                    }
                }
                if response.double_clicked() {
                    self.selected = index;
                    if self.double_click_action().is_some() {
                        *double_click = true;
                    } else {
                        *accept = true;
                    }
                }
            } else if slot == self.visible_result_rows - 1
                && self
                    .displayed
                    .as_ref()
                    .is_none_or(|d| d.output.results.is_empty())
            {
                let text =
                    if self.current_results() && self.displayed.as_ref().is_some_and(|d| d.done) {
                        "No matches"
                    } else {
                        " "
                    };
                painter.text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    text,
                    appearance.typography.normal.clone(),
                    appearance.palette.muted,
                );
            }
        }
        if let Some(top) = first_rect {
            let results_rect = egui::Rect::from_min_size(
                top.min,
                egui::vec2(top.width(), self.visible_result_rows as f32 * result_height),
            );
            self.paint_copy_notice(ui, results_rect, appearance);
        }
        let hovered = first_rect.is_some_and(|first| {
            ui.rect_contains_pointer(egui::Rect::from_min_size(
                first.min,
                egui::vec2(
                    first.width(),
                    self.visible_result_rows as f32 * result_height,
                ),
            ))
        });
        let wheel = ui.input(|input| {
            if hovered {
                input.smooth_scroll_delta.y
            } else {
                0.0
            }
        });
        if ui.is_enabled() && wheel.abs() > 0.0 {
            self.move_selection(if wheel > 0.0 { 1 } else { -1 });
        }
    }
    /// The "Copied" notice, centered over `results_rect`, while it lasts.
    fn paint_copy_notice(&self, ui: &egui::Ui, results_rect: egui::Rect, appearance: &Appearance) {
        if self.copied_until.is_none_or(|until| until <= Instant::now()) {
            return;
        }
        let galley = ui.painter().layout(
            self.copy_notice.clone(),
            appearance.typography.normal.clone(),
            appearance.palette.success,
            (results_rect.width() - 32.0).max(1.0),
        );
        let toast = egui::Rect::from_center_size(
            results_rect.center(),
            galley.size() + egui::vec2(16.0, 6.0),
        );
        let painter = ui
            .painter()
            .with_clip_rect(results_rect.intersect(ui.clip_rect()));
        painter.rect_filled(toast, 3, appearance.palette.selection_background);
        painter.galley(
            toast.min + egui::vec2(8.0, 3.0),
            galley,
            appearance.palette.success,
        );
    }
    /// Command mode's results: the listed commands, one per result slot so
    /// the layout stays as it was. Each row shows the command's signature,
    /// with the typed letters highlighted, and its description; once the
    /// name is typed, also its argument's.
    fn command_list_ui(&mut self, ui: &mut egui::Ui, appearance: &Appearance) {
        let line = query_command::parse(&self.query);
        let named = line.argument.is_some();
        let typed = line.name.to_lowercase();
        let commands: Vec<QueryCommand> = self.listed_commands().into_iter().cloned().collect();
        let choices = self.command_choices().to_vec();
        let rows = commands.len() + choices.len();
        let slot_height = self.result_row_height(ui.ctx(), appearance);
        let row_height = appearance.typography.preview_row_height(ui.ctx());
        let visible = self.visible_result_rows.max(1);
        self.command_selected = self.command_selected.min(rows.saturating_sub(1));
        let first = self
            .command_selected
            .saturating_sub(visible - 1)
            .min(rows.saturating_sub(visible));
        let results_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(ui.available_width(), visible as f32 * slot_height),
        );
        for slot in 0..visible {
            let index = if self.config.layout == Layout::QueryBottom {
                first + visible - 1 - slot
            } else {
                first + slot
            };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), slot_height),
                egui::Sense::hover(),
            );
            let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
            let selected = index == self.command_selected;
            let paint_selection = |painter: &egui::Painter| {
                painter.rect_filled(rect, 4, appearance.palette.selection_background);
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 1.5, rect.center().y),
                        egui::vec2(3.0, 16.0),
                    ),
                    1.5,
                    appearance.palette.accent,
                );
            };
            let icon_size = (row_height - 2.0).max(1.0);
            if let Some(choice) = index
                .checked_sub(commands.len())
                .and_then(|choice| choices.get(choice))
            {
                // A choice, lined up with the command's text above it.
                if selected {
                    paint_selection(&painter);
                }
                painter.text(
                    egui::pos2(rect.left() + 12.0 + icon_size + CARD_ICON_GAP, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &choice.text,
                    appearance.typography.normal.clone(),
                    if selected {
                        appearance.palette.selection_text
                    } else {
                        appearance.palette.text
                    },
                );
                let response =
                    ui.interact(rect, self.id.with(("command_choice", index)), egui::Sense::click());
                if response.clicked() {
                    self.command_selected = index;
                    self.command_clicked = Some(index);
                    self.focus_query = true;
                }
                continue;
            }
            let Some(command) = commands.get(index) else {
                // Where the best match would be.
                if commands.is_empty() && index == 0 {
                    painter.text(
                        rect.left_center() + egui::vec2(12.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        format!("No command matches {}", line.name),
                        appearance.typography.normal.clone(),
                        appearance.palette.muted,
                    );
                }
                continue;
            };
            if selected {
                paint_selection(&painter);
            }
            // The command prompt marks a command, as hosts mark command rows.
            let size = icon_size;
            painter.text(
                egui::pos2(rect.left() + 12.0 + size / 2.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                COMMAND_PROMPT,
                appearance.typography.normal.clone(),
                appearance.palette.success,
            );
            let positions = if typed.is_empty() || named {
                Vec::new()
            } else {
                resolve_match_positions(&typed, &command.name)
            };
            let mut job = highlighted_layout(&command.signature(), &positions, selected, appearance);
            let mut detail = command.description.clone();
            if named
                && let Some(argument) = &command.argument
                && !argument.description.is_empty()
            {
                if !detail.is_empty() {
                    detail.push_str("  ·  ");
                }
                detail.push_str(&format!("<{}> {}", argument.name, argument.description));
            }
            if !detail.is_empty() {
                job.append(
                    &format!("{DETAIL_GAP}{detail}"),
                    0.0,
                    egui::TextFormat::simple(
                        appearance.typography.normal.clone(),
                        appearance.palette.muted,
                    ),
                );
            }
            let galley = painter.layout_job(job);
            painter.galley(
                egui::pos2(
                    rect.left() + 12.0 + size + CARD_ICON_GAP,
                    rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                appearance.palette.text,
            );
            let response = ui.interact(rect, self.id.with(("command", index)), egui::Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    selected,
                    command.signature(),
                )
            });
            if response.clicked() {
                self.command_selected = index;
                self.command_clicked = Some(index);
                self.focus_query = true;
            }
        }
        // Hosts confirm what a command copied, such as a calculator result.
        self.paint_copy_notice(ui, results_rect, appearance);
    }
    fn query_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &Context,
        input_id: Id,
        appearance: &Appearance,
        complete: bool,
        interactive: bool,
    ) {
        let before = self.query.clone();
        let row_height = appearance.typography.preview_row_height(ctx);
        let command_mode = self.command_mode;
        ui.horizontal(|ui| {
            // The prompt shows the mode: `>` to search, `:` for a command line.
            let (prompt, color) = if command_mode {
                (COMMAND_PROMPT, appearance.palette.success)
            } else {
                (SEARCH_PROMPT, appearance.palette.accent)
            };
            let prompt_width = ui.fonts_mut(|fonts| {
                fonts
                    .glyph_width(&appearance.typography.normal, SEARCH_PROMPT)
                    .max(fonts.glyph_width(&appearance.typography.normal, COMMAND_PROMPT))
            });
            let (prompt_rect, _) =
                ui.allocate_exact_size(egui::vec2(prompt_width, row_height), egui::Sense::hover());
            ui.painter().text(
                prompt_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                prompt,
                appearance.typography.normal.clone(),
                color,
            );
            let total = self.displayed.as_ref().map_or(0, |d| d.output.total);
            let matched = if command_mode {
                // Listed commands; the status keeps the search count's width.
                self.listed_commands().len()
            } else if self.current_results() {
                self.displayed.as_ref().map_or(0, |d| d.output.matched)
            } else {
                0
            };
            // Left-out source records, such as overlong lines, are mentioned
            // so missing results are not a silent surprise.
            let skipped = match self.session.skipped() {
                0 => String::new(),
                skipped => format!(" · {skipped} skipped"),
            };
            let count = if command_mode {
                format!("{matched}/{}", self.query_commands.len())
            } else {
                format!("{matched}/{total}{skipped}")
            };
            let count_size = ui
                .painter()
                .layout_no_wrap(
                    format!("{total}/{total}{skipped}"),
                    appearance.typography.normal.clone(),
                    appearance.palette.text,
                )
                .size();
            let status_width = count_size.x + 24.0;
            let query_width =
                (ui.available_width() - status_width - ui.spacing().item_spacing.x).max(1.0);
            let query_interactive = interactive && !self.preview_focused();
            let edit = egui::TextEdit::singleline(&mut self.query)
                .id(input_id)
                .interactive(query_interactive)
                .font(appearance.typography.normal.clone())
                .vertical_align(egui::Align::Center)
                .margin(0)
                .min_size(egui::vec2(query_width, row_height))
                .desired_width(query_width)
                .frame(egui::Frame::NONE)
                .event_filter(egui::EventFilter {
                    tab: true,
                    escape: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                })
                .show(ui);
            if interactive && self.focus_query && !self.preview_focused() {
                edit.response.request_focus();
                self.focus_query = false;
            }
            if before != self.query {
                let query = std::mem::replace(&mut self.query, before.clone());
                self.set_query(query);
            }
            // Typing `> ` or `: ` switched modes and replaced the text under the
            // cursor; draw the frame again with the new prompt and text.
            if self.command_mode != command_mode {
                self.store_query_cursor(ctx);
                ctx.request_discard("the query switched to or from command mode");
            } else {
                self.query_cursor = edit.cursor_range.map_or(self.query.len(), |range| {
                    self.query
                        .char_indices()
                        .nth(range.primary.index.into())
                        .map_or(self.query.len(), |(offset, _)| offset)
                });
            }
            self.query_rect = edit.response.rect;
            self.query_text_pos = edit.galley_pos;
            if self.command_mode {
                self.refresh_command_preview();
            }
            if let Some((ghost, color)) = self.ghost_text(appearance) {
                let rect = edit.response.rect;
                ui.painter().with_clip_rect(rect.intersect(ui.clip_rect())).text(
                    egui::pos2(edit.galley_pos.x + edit.galley.size().x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    ghost,
                    appearance.typography.normal.clone(),
                    color,
                );
            }
            if !self.preview_focused() {
                self.refresh_completions();
            }
            if complete
                && let Some(completion) = self.suggestions.get(self.suggestion_selected).cloned()
                && self.apply_completion(&completion)
            {
                let cursor = self.query[..completion.replace.start + completion.replacement.len()]
                    .chars()
                    .count();
                let mut state = edit.state.clone();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(cursor),
                    )));
                state.store(ctx, input_id);
                self.refresh_completions();
            }
            let (status_rect, _) =
                ui.allocate_exact_size(egui::vec2(status_width, row_height), egui::Sense::hover());
            if self.error.is_none() && self.busy() {
                let size = ui
                    .painter()
                    .layout_no_wrap(
                        count.clone(),
                        appearance.typography.normal.clone(),
                        appearance.palette.muted,
                    )
                    .size();
                let center =
                    egui::pos2(status_rect.right() - size.x - 14.0, status_rect.center().y);
                paint_loading_indicator(
                    ui,
                    egui::Rect::from_center_size(center, egui::vec2(12.0, 12.0)),
                    appearance.palette.muted,
                );
            }
            ui.painter().text(
                status_rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                count,
                appearance.typography.normal.clone(),
                appearance.palette.muted,
            );
        });
    }
    /// Draw within an existing host egui pass; no window, GL or platform ownership.
    /// Paint the picker. Disabled controls retain background rendering and
    /// worker updates without consuming input or requesting keyboard focus.
    pub fn show(
        &mut self,
        ctx: &Context,
        placement: Placement,
        appearance: &Appearance,
        disabled: bool,
    ) -> PickerOutput<S::Item> {
        self.show_mode(ctx, placement, appearance, !disabled)
    }
    fn show_mode(
        &mut self,
        ctx: &Context,
        mut placement: Placement,
        appearance: &Appearance,
        interactive: bool,
    ) -> PickerOutput<S::Item> {
        self.poll();
        if self
            .preview_viewport
            .as_ref()
            .is_some_and(|copy| !copy.valid())
        {
            self.leave_preview_focus();
        }
        self.refresh_completions();
        let input_id = self.id.with("query");
        let focus_id = if self.preview_focused() {
            self.id.with("preview-copy")
        } else {
            input_id
        };
        // The host decides which pane is interactive. Restore query focus before
        // processing input, including the first frame after a pane is uncovered.
        // An open palette has focus and takes the input instead.
        let palette_was_open = self.palette.is_some();
        ctx.memory_mut(|memory| {
            if interactive && !self.closed && !palette_was_open {
                memory.request_focus(focus_id);
            } else {
                if memory.has_focus(input_id) {
                    memory.surrender_focus(input_id);
                }
                if memory.has_focus(self.id.with("preview-copy")) {
                    memory.surrender_focus(self.id.with("preview-copy"));
                }
            }
        });
        let focused = ctx.memory(|m| {
            m.has_focus(input_id) || (m.focused().is_none() && m.had_focus_last_frame(input_id))
        }) || self.focus_query
            || self.preview_focused();
        let mut event = None;
        let mut accept = false;
        let mut double_click = false;
        let mut copy = false;
        let mut complete = false;
        let mut leave_command_line = false;
        let mut dismiss_output = false;
        let mut copy_output = false;
        let query_selected = egui::text_edit::TextEditState::load(ctx, input_id)
            .and_then(|state| state.cursor.char_range())
            .is_some_and(|range| !range.is_empty());
        let mut run_command_line = false;
        // A listed command row Tab (false) or Enter (true) was pressed on.
        let mut pick_command = None;
        // Whether Ctrl+Enter rather than Enter runs the command line.
        let mut alternate_command = false;
        let direction = if self.config.layout == Layout::QueryBottom {
            -1
        } else {
            1
        };
        // Commands run in order: those from `execute` and the palette, then
        // bound keys. A bound key is consumed only when its command applies,
        // so copy mode keeps the keys it uses itself.
        let mut actions = std::mem::take(&mut self.pending_actions);
        if interactive && !self.closed && focused {
            if !palette_was_open {
                ctx.input_mut(|input| {
                    let mut events = std::mem::take(&mut input.events).into_iter().peekable();
                    while let Some(event) = events.next() {
                        let binding = if let egui::Event::Key { key, modifiers, .. } = &event {
                            self.config.bindings.iter().find(|binding| {
                                binding.trigger == PickerTrigger::Key(*key)
                                    && same_binding_modifiers(binding.modifiers, *modifiers)
                                    && self.binding_applies(binding.action)
                            })
                        } else {
                            None
                        };
                        if let Some(binding) = binding {
                            let egui::Event::Key {
                                pressed, repeat, ..
                            } = event
                            else {
                                unreachable!()
                            };
                            if pressed && (!repeat || binding.action.repeats()) {
                                actions.push(binding.action);
                            }
                            if pressed && matches!(events.peek(), Some(egui::Event::Text(_))) {
                                events.next();
                            }
                        } else {
                            input.events.push(event);
                        }
                    }
                });
            }
        }
        // Only the first custom command reaches the host this frame.
        let mut custom_action = None;
        if !self.closed {
            for action in actions {
                if let Some(id) = self.run_action(action, ctx, placement.position, &mut copy) {
                    custom_action.get_or_insert(id);
                }
            }
        }
        // The palette takes every key while it is open; keys that arrived
        // with the one that opened it go nowhere.
        let palette_open = self.palette.is_some();
        if palette_open && !palette_was_open {
            ctx.input_mut(|input| input.events.clear());
        }
        let mut preview_handled = palette_open;
        if interactive && !self.closed && focused && !palette_open && self.preview_focused() {
            let events = ctx.input(|input| input.events.clone());
            if let Some(viewport) = &mut self.preview_viewport
                && viewport.input(ctx, &events, self.visible_preview_rows)
            {
                self.deactivate_preview_focus();
            }
            ctx.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key { .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Copy
                            | egui::Event::Cut
                    )
                })
            });
            preview_handled = true;
        }
        // Right arrow at the end of a command line completes its ghost text,
        // as a shell's suggestion does; elsewhere it moves the cursor.
        let ghost_completion = (self.query_cursor == self.query.len())
            .then(|| self.ghost_completion())
            .flatten();
        let mut complete_ghost = None;
        if interactive && !self.closed && focused && !preview_handled {
            ctx.input_mut(|input| {
                if let Some(line) = ghost_completion
                    && input.consume_key(Modifiers::NONE, Key::ArrowRight)
                {
                    complete_ghost = Some(line);
                }
                // Unbound Ctrl+C chords, such as Ctrl+Shift+C outside copy
                // mode, copy the selected result as they always have.
                // In command mode they copy the command line's text instead.
                if !self.command_mode && input.consume_key(Modifiers::CTRL, Key::C) {
                    copy = true;
                }
                // With nothing selected on the command line, Ctrl+C copies
                // the command's output.
                if self.command_output.is_some() && !query_selected {
                    let mut chord = input.consume_key(Modifiers::CTRL, Key::C);
                    input.events.retain(|event| {
                        let copies = matches!(event, egui::Event::Copy);
                        chord |= copies;
                        !copies
                    });
                    copy_output |= chord;
                }
                let suggestions_open = !self.suggestions.is_empty();
                if input.consume_key(Modifiers::NONE, Key::Escape) {
                    if suggestions_open {
                        self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
                        self.suggestions.clear();
                        self.suggestion_selected = 0;
                        self.focus_query = true;
                    } else if self.command_output.is_some() {
                        // Back to the command line, as it was typed.
                        dismiss_output = true;
                    } else if self.command_mode() {
                        leave_command_line = true;
                    } else {
                        event = Some(PickerEvent::Cancelled);
                    }
                }
                if input.consume_key(Modifiers::NONE, Key::Enter) {
                    if suggestions_open {
                        complete = true;
                    } else if self.command_mode() {
                        pick_command = Some((self.command_selected, true));
                    } else {
                        accept = true;
                    }
                }
                // A command's second action, such as copying a result.
                if self.command_mode() && input.consume_key(Modifiers::CTRL, Key::Enter) {
                    pick_command = Some((self.command_selected, true));
                    alternate_command = true;
                }
                if input.consume_key(Modifiers::NONE, Key::Tab) {
                    if self.command_mode() {
                        pick_command = Some((self.command_selected, false));
                    } else {
                        complete = true;
                    }
                }
                if suggestions_open {
                    if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        self.suggestion_selected = self
                            .suggestion_selected
                            .checked_sub(1)
                            .unwrap_or(self.suggestions.len().saturating_sub(1));
                    }
                    if input.consume_key(Modifiers::NONE, Key::ArrowDown)
                        && !self.suggestions.is_empty()
                    {
                        self.suggestion_selected =
                            (self.suggestion_selected + 1) % self.suggestions.len();
                    }
                }
                // A command's output covers the command list, so these keys
                // scroll it, Up toward its start, rather than move the hidden
                // selection.
                if self.command_mode && self.command_output.is_some() {
                    let page = self.visible_preview_rows.max(1) as isize;
                    for (key, modifiers, rows) in [
                        (Key::N, Modifiers::CTRL, 1),
                        (Key::ArrowDown, Modifiers::NONE, 1),
                        (Key::ArrowUp, Modifiers::NONE, -1),
                        (Key::PageDown, Modifiers::NONE, page),
                        (Key::PageUp, Modifiers::NONE, -page),
                    ] {
                        if input.consume_key(modifiers, key) {
                            self.scroll_preview_rows(rows);
                        }
                    }
                }
                for (key, modifiers, delta) in [
                    (Key::N, Modifiers::CTRL, 1),
                    (Key::ArrowDown, Modifiers::NONE, direction),
                    (Key::ArrowUp, Modifiers::NONE, -direction),
                    (
                        Key::PageDown,
                        Modifiers::NONE,
                        direction * self.visible_result_rows as isize,
                    ),
                    (
                        Key::PageUp,
                        Modifiers::NONE,
                        -direction * self.visible_result_rows as isize,
                    ),
                ] {
                    if input.consume_key(modifiers, key) {
                        if self.command_mode {
                            self.move_command_selection(delta);
                        } else {
                            self.move_selection(delta);
                        }
                    }
                }

                input.events.retain(|e| {
                    if !self.command_mode && matches!(e, egui::Event::Copy) {
                        copy = true;
                        false
                    } else {
                        true
                    }
                });
            });
        }
        if leave_command_line {
            self.set_command_mode(false);
            self.store_query_cursor(ctx);
        }
        if dismiss_output {
            self.dismiss_command_output();
            ctx.request_repaint();
        }
        if copy_output {
            self.copy_command_output(ctx);
        }
        if let Some(line) = complete_ghost {
            self.set_query(line);
            self.store_query_cursor(ctx);
            ctx.request_repaint();
        }
        placement.position = self.effective_position(placement.position);
        let row_height = appearance.typography.preview_row_height(ctx);
        let full_pane = placement.position == crate::Position::FullPane;
        let available = (placement.bounds.height()
            - if full_pane {
                0.0
            } else {
                2.0 * placement.margin.max(0.0)
            })
        .max(0.0);
        let header = self
            .displayed
            .as_ref()
            .and_then(|displayed| displayed.snapshot.header(&self.query));
        let header_height = if header.is_some() { row_height } else { 0.0 };
        let result_height = self.result_row_height(ctx, appearance);
        let cards = self
            .displayed
            .as_ref()
            .is_some_and(|displayed| displayed.snapshot.row_template().is_some());
        // Room for the preview's divider only when a preview can show.
        let overhead =
            if self.preview_available() && self.config.preview_visible {
                PREVIEW_OVERHEAD
            } else {
                RESULTS_OVERHEAD
            };
        self.visible_result_rows = self.config.result_rows.clamp(1, 256).min(
            rows_below_query(
                available - overhead - header_height,
                row_height,
                result_height,
            )
            .floor()
            .max(1.0) as usize,
        );
        let preview_row_height = appearance.typography.preview_row_height(ctx);
        let remaining = (available
            - row_height
            - self.visible_result_rows as f32 * result_height
            - PREVIEW_OVERHEAD
            - header_height)
            .max(0.0);
        self.preview_rect = None;
        self.preview_rendered = false;
        self.output_pane_height = None;
        self.visible_preview_rows =
            if self.preview_available() && self.config.preview_visible {
                let capacity = (remaining / preview_row_height).floor() as usize;
                if full_pane {
                    capacity.min(u16::MAX as usize)
                } else {
                    self.config.preview_rows.clamp(1, 256).min(capacity)
                }
            } else {
                0
            };
        if full_pane && self.visible_preview_rows == 0 {
            // The pane belongs to results when there is no visible preview.
            // Reserve the query, divider, frame margins, and optional header.
            self.visible_result_rows = rows_below_query(
                available - PANEL_FRAME_HEIGHT - DIVIDER_HEIGHT - header_height,
                row_height,
                result_height,
            )
            .floor()
            .clamp(1.0, u16::MAX as f32) as usize;
        }
        self.keep_selected_visible();
        let has_preview = self.visible_preview_rows > 0;
        if !has_preview {
            self.leave_preview_focus();
        }
        let height = row_height
            + self.visible_result_rows as f32 * result_height
            + self.visible_preview_rows as f32 * preview_row_height
            + if has_preview {
                PREVIEW_OVERHEAD
            } else {
                RESULTS_OVERHEAD
            }
            + header_height;
        let rect = placement.rect(height);
        // A command's output takes the command list's space too, down to the
        // query (or the panel's end with the query on top). The panel keeps
        // its size; the output shows the whole rows that fit.
        let output_covers_results =
            has_preview && self.command_mode && self.command_output.is_some();
        if output_covers_results {
            let pane = if full_pane {
                rect.height() - PANEL_FRAME_HEIGHT - row_height - DIVIDER_HEIGHT
            } else {
                self.visible_preview_rows as f32 * preview_row_height
                    + DIVIDER_HEIGHT
                    + header_height
                    + self.visible_result_rows as f32 * result_height
            }
            .max(preview_row_height);
            self.output_pane_height = Some(pane);
            self.visible_preview_rows = (pane / preview_row_height).floor().max(1.0) as usize;
        }
        if has_preview && self.command_mode {
            self.refresh_command_output((rect.width() - PANEL_FRAME_WIDTH).max(1.0), appearance);
        } else if has_preview {
            self.refresh_preview((rect.width() - PANEL_FRAME_WIDTH).max(1.0), appearance);
        }
        if !self.closed && rect.is_positive() {
            let previous_size =
                ctx.memory(|memory| memory.area_rect(self.id).map(|rect| rect.size()));
            let area = egui::Area::new(self.id)
                .fade_in(false)
                .order(if interactive {
                    egui::Order::Foreground
                } else {
                    egui::Order::Middle
                })
                .interactable(interactive && !palette_open)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    ui.set_clip_rect(rect.intersect(placement.bounds).intersect(ui.clip_rect()));
                    egui::Frame::new()
                        .fill(appearance.palette.background)
                        .stroke(egui::Stroke::new(PANEL_BORDER, appearance.palette.divider))
                        .corner_radius(PANEL_RADIUS)
                        .inner_margin(PANEL_MARGIN)
                        .show(ui, |ui| {
                            ui.set_width((rect.width() - PANEL_FRAME_WIDTH).max(1.0));
                            if full_pane {
                                ui.set_min_height((rect.height() - PANEL_FRAME_HEIGHT).max(0.0));
                            }
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.visuals_mut().override_text_color = Some(appearance.palette.text);
                            ui.visuals_mut().text_cursor.stroke =
                                egui::Stroke::new(2.0, appearance.palette.text);
                            ui.visuals_mut().selection.bg_fill =
                                appearance.palette.selection_background;
                            ui.visuals_mut().selection.stroke.color =
                                appearance.palette.selection_text;
                            if self.config.layout == Layout::QueryTop {
                                self.query_ui(
                                    ui,
                                    ctx,
                                    input_id,
                                    appearance,
                                    complete,
                                    interactive && !palette_open,
                                );
                                divider(ui, appearance);
                            }
                            if has_preview {
                                self.preview_ui(ui, appearance);
                                if !output_covers_results {
                                    divider(ui, appearance);
                                }
                            }
                            if !output_covers_results
                                && full_pane
                                && self.config.layout == Layout::QueryBottom
                            {
                                // Keep results next to the bottom query. Any
                                // unused pane height belongs above this block.
                                let block_height = header_height
                                    + row_height
                                    + self.visible_result_rows as f32 * result_height
                                    + DIVIDER_HEIGHT;
                                let gap = rect.bottom()
                                    - PANEL_INSET_BOTTOM
                                    - ui.cursor().top()
                                    - block_height;
                                ui.add_space(gap.max(0.0));
                            }
                            if output_covers_results {
                                // The output stands in for the command list.
                            } else if header.is_some() && self.command_mode {
                                // The listed commands have no columns; the
                                // space stays so the layout does not move.
                                ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_height),
                                    egui::Sense::hover(),
                                );
                            } else if let Some(header) = &header {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_height),
                                    egui::Sense::hover(),
                                );
                                // Over its columns, which row icons move right.
                                let icon_inset = if self.row_icon.is_some() && !cards {
                                    (result_height - 2.0).max(1.0) + CARD_ICON_GAP
                                } else {
                                    0.0
                                };
                                ui.painter()
                                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                                    .text(
                                        rect.left_center() + egui::vec2(12.0 + icon_inset, 0.0),
                                        egui::Align2::LEFT_CENTER,
                                        header,
                                        appearance.typography.bold.clone(),
                                        appearance.palette.accent,
                                    );
                                ui.painter()
                                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                                    .line_segment(
                                        [
                                            egui::pos2(rect.left() + 12.0, rect.bottom() - 1.0),
                                            egui::pos2(rect.right() - 12.0, rect.bottom() - 1.0),
                                        ],
                                        egui::Stroke::new(
                                            1.5,
                                            egui::Color32::from_rgb(0xd7, 0x99, 0x21),
                                        ),
                                    );
                            }
                            if output_covers_results {
                            } else if interactive {
                                self.results_ui(ui, appearance, &mut accept, &mut double_click);
                            } else {
                                ui.scope_builder(egui::UiBuilder::new().disabled(), |ui| {
                                    self.results_ui(ui, appearance, &mut accept, &mut double_click)
                                });
                            }
                            if self.config.layout == Layout::QueryBottom {
                                divider(ui, appearance);
                                self.query_ui(
                                    ui,
                                    ctx,
                                    input_id,
                                    appearance,
                                    complete,
                                    interactive && !palette_open,
                                );
                            }
                        });
                });
            if event.is_none()
                && !accept
                && previous_size.is_none_or(|size| {
                    (size.x - area.response.rect.width()).abs() > 0.5
                        || (size.y - area.response.rect.height()).abs() > 0.5
                })
            {
                ctx.request_discard("resolve shared picker size before presentation");
            }
        }
        // After the query box applied this frame's typing, so text typed
        // just before Enter is part of the argument. A click picks like Enter.
        if let Some((row, run)) = pick_command.or(self.command_clicked.take().map(|row| (row, true)))
            && self.command_mode
        {
            let (picked, run) = self.pick_command(ctx, row, run, alternate_command);
            event = event.or(picked);
            run_command_line |= run;
        }
        if run_command_line && self.command_mode {
            match query_command::resolve(&self.query_commands, &self.query) {
                Ok((command, argument)) => {
                    event = Some(PickerEvent::Command {
                        name: command.name.clone(),
                        argument: argument.to_owned(),
                        alternate: alternate_command,
                    });
                }
                Err(error) => {
                    // Shown after the query box, which this frame already drew.
                    self.command_error = Some(error);
                    ctx.request_repaint();
                }
            }
        }
        if interactive && !self.closed {
            self.palette_ui(ctx, rect, appearance);
        }
        if interactive
            && !self.closed
            && self.palette.is_none()
            && !self.preview_focused()
            && event.is_none()
        {
            self.completion_ui(ctx, placement.bounds, appearance);
        }
        if event.is_none()
            && self.result_actions_enabled
            && let Some(selection) = self.selection()
        {
            if double_click {
                if let Some(action) = self.double_click_action() {
                    let mut copy = false;
                    if let Some(action) =
                        self.run_action(action, ctx, placement.position, &mut copy)
                    {
                        event = Some(PickerEvent::CustomAction { action, selection });
                    } else if copy {
                        event = Some(PickerEvent::CopyRequested(selection));
                    }
                }
            } else if accept {
                event = self.accept(ctx, selection);
            } else if copy {
                event = Some(PickerEvent::CopyRequested(selection));
            } else if let Some(action) = custom_action {
                event = Some(PickerEvent::CustomAction { action, selection });
            }
        }
        if matches!(
            event,
            Some(PickerEvent::Accepted(_)) | Some(PickerEvent::Cancelled)
        ) {
            self.close();
        }
        let busy = self.busy();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let frame_selection = match &event {
            Some(PickerEvent::Accepted(_))
            | Some(PickerEvent::CustomAction { .. })
            | Some(PickerEvent::CopyRequested(_)) => None,
            _ => self.selection(),
        };
        PickerOutput {
            frame_selection,
            preview_rendered: self.preview_rendered,
            preview_selection: self
                .preview
                .as_ref()
                .filter(|worker| !worker.loading)
                .and_then(|worker| worker.document_selection),
            preview_rect: if self.closed || self.palette.is_some() {
                None
            } else {
                self.preview_rect
            },
            event,
            wants_keyboard: interactive
                && !self.closed
                && (self.palette.is_some()
                    || self.preview_focused()
                    || ctx.memory(|m| m.has_focus(input_id))),
            busy,
            matched: if self.current_results() {
                self.displayed.as_ref().map_or(0, |d| d.output.matched)
            } else {
                0
            },
            total: self.displayed.as_ref().map_or(0, |d| d.output.total),
            rect,
        }
    }
}
fn divider(ui: &mut egui::Ui, appearance: &Appearance) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), DIVIDER_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(1.5, appearance.palette.divider.gamma_multiply(0.8)),
    );
}
fn paint_loading_indicator(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let center = rect.center();
    let dot_size = rect.height() / 4.0;
    let dot_step = (rect.height() - dot_size) / 2.0;
    let phase = (ui.input(|input| input.time) * 12.0) as usize % 8;
    for (dot, (x, y)) in [
        (-1.0, -1.0),
        (0.0, -1.0),
        (1.0, -1.0),
        (1.0, 0.0),
        (1.0, 1.0),
        (0.0, 1.0),
        (-1.0, 1.0),
        (-1.0, 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        let alpha = 1.0 - ((dot + 8 - phase) % 8) as f32 * 0.11;
        ui.painter().rect_filled(
            egui::Rect::from_center_size(
                center + egui::vec2(x * dot_step, y * dot_step),
                egui::vec2(dot_size, dot_size),
            ),
            0.0,
            color.gamma_multiply(alpha),
        );
    }
    ui.ctx().request_repaint_after(Duration::from_millis(80));
}
/// Marks text hidden at either end of a result row.
const RESULT_ELLIPSIS: char = '…';
/// Most characters after the last match kept visible when a row is shifted.
const RESULT_MATCH_CONTEXT: usize = 8;
/// Gap between result text and the row's right edge.
const RESULT_RIGHT_PADDING: f32 = 4.0;

/// The part of a result row's text that is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResultWindow {
    /// Byte range of the visible text.
    start: usize,
    end: usize,
    /// Text is hidden before `start`.
    leading: bool,
    /// Text is hidden after `end`.
    trailing: bool,
}

/// Character painted for `c`: a row is one line, so control characters
/// cannot be painted as they are.
fn result_display_char(c: char) -> char {
    if c == '\t' {
        ' '
    } else if c.is_control() {
        char::REPLACEMENT_CHARACTER
    } else {
        c
    }
}

/// Choose the part of `text` that fits in `available` points, given each
/// character's width (`matched` characters use the bold font). Text that fits
/// is shown whole. Otherwise the start is kept if the last match is visible
/// there; if not, the text shifts left so the match ends near the right edge
/// with a little following context, as fzf does. Ellipses mark hidden text and
/// their width is reserved. Only characters near the window are measured.
fn result_window(
    text: &str,
    positions: &[usize],
    available: f32,
    mut width: impl FnMut(char, bool) -> f32,
) -> ResultWindow {
    let matched = |offset: usize| positions.binary_search(&offset).is_ok();
    let ellipsis = width(RESULT_ELLIPSIS, false);
    let mut used = 0.0;
    // Longest prefix that leaves room for a trailing ellipsis.
    let mut prefix_end = 0;
    let mut fits = true;
    for (offset, c) in text.char_indices() {
        used += width(result_display_char(c), matched(offset));
        if used > available {
            fits = false;
            break;
        }
        if used <= available - ellipsis {
            prefix_end = offset + c.len_utf8();
        }
    }
    if fits {
        return ResultWindow {
            start: 0,
            end: text.len(),
            leading: false,
            trailing: false,
        };
    }
    let start_kept = ResultWindow {
        start: 0,
        end: prefix_end,
        leading: false,
        trailing: true,
    };
    let Some(&last) = positions
        .last()
        .filter(|&&last| last < text.len() && text.is_char_boundary(last))
    else {
        return start_kept;
    };
    let match_end = last + text[last..].chars().next().map_or(0, char::len_utf8);
    if match_end <= prefix_end {
        return start_kept;
    }
    // Context after the match takes at most a quarter of the row.
    let mut end = match_end;
    let mut context = 0.0;
    for c in text[match_end..].chars().take(RESULT_MATCH_CONTEXT) {
        context += width(result_display_char(c), false);
        if context > available / 4.0 {
            break;
        }
        end += c.len_utf8();
    }
    let trailing = end < text.len();
    let budget = available - ellipsis - if trailing { ellipsis } else { 0.0 };
    let mut start = end;
    let mut used = 0.0;
    for (offset, c) in text[..end].char_indices().rev() {
        used += width(result_display_char(c), matched(offset));
        if used > budget {
            break;
        }
        start = offset;
    }
    ResultWindow {
        start,
        end,
        leading: start > 0,
        trailing,
    }
}

fn result_format(matched: bool, selected: bool, appearance: &Appearance) -> egui::TextFormat {
    egui::TextFormat {
        font_id: if matched {
            appearance.typography.bold.clone()
        } else {
            appearance.typography.normal.clone()
        },
        color: if matched {
            appearance.palette.match_highlight
        } else if selected {
            appearance.palette.selection_text
        } else {
            appearance.palette.text
        },
        ..Default::default()
    }
}

/// Vertical padding above the first and below the last line of a card.
const CARD_PADDING_Y: f32 = 6.0;
/// Space between card lines.
const CARD_LINE_GAP: f32 = 1.0;
/// Space between an icon and what follows it.
const CARD_ICON_GAP: f32 = 6.0;
/// Space between slots on a card line.
const CARD_SLOT_GAP: f32 = 16.0;
/// The share of a line that right-aligned slots may take.
const CARD_RIGHT_SHARE: f32 = 0.6;

/// Line heights, the leading icon's column and the total height of every
/// card in a template.
#[derive(Clone, Debug, PartialEq)]
struct CardMetrics {
    line_heights: Vec<f32>,
    /// Width of the leading icon's column, its gap included; 0 without one.
    leading: f32,
    height: f32,
}

fn card_font(style: RowStyle, matched: bool, appearance: &Appearance) -> egui::FontId {
    let base = if matched {
        &appearance.typography.bold
    } else {
        &appearance.typography.normal
    };
    if style == RowStyle::Title {
        egui::FontId::new((base.size * 1.15).round(), base.family.clone())
    } else {
        base.clone()
    }
}

fn card_color(style: RowStyle, selected: bool, appearance: &Appearance) -> egui::Color32 {
    let palette = &appearance.palette;
    match style {
        RowStyle::Muted => palette.muted,
        RowStyle::Accent => palette.accent,
        RowStyle::Normal | RowStyle::Title if selected => palette.selection_text,
        RowStyle::Normal | RowStyle::Title => palette.text,
    }
}

/// The height, in points, of a picker showing `rows` results without a preview
/// or column header: as cards when `template` is set, else as lines. Hosts that
/// size a window to its results can call it before any results arrive, and add
/// the placement's margins.
pub fn results_height(
    ctx: &egui::Context,
    appearance: &Appearance,
    template: Option<&RowTemplate>,
    rows: usize,
) -> f32 {
    let row_height = appearance.typography.preview_row_height(ctx);
    let result_height = template.map_or(row_height, |template| {
        card_metrics(ctx, appearance, template).height
    });
    row_height + rows.clamp(1, 256) as f32 * result_height + RESULTS_OVERHEAD
}

/// Each line is as tall as its tallest font; the card height is rounded to a
/// whole physical pixel so stacked cards stay aligned. The leading icon is a
/// square as tall as the title line.
fn card_metrics(
    ctx: &egui::Context,
    appearance: &Appearance,
    template: &RowTemplate,
) -> CardMetrics {
    let line_heights: Vec<f32> = ctx.fonts_mut(|fonts| {
        template
            .lines
            .iter()
            .map(|line| {
                let styles = line.slots.iter().map(|slot| slot.style);
                styles
                    .chain(std::iter::once(RowStyle::Normal))
                    .map(|style| {
                        fonts
                            .row_height(&card_font(style, false, appearance))
                            .max(fonts.row_height(&card_font(style, true, appearance)))
                    })
                    .fold(0.0, f32::max)
            })
            .collect()
    });
    let content: f32 = line_heights.iter().sum::<f32>()
        + CARD_LINE_GAP * line_heights.len().saturating_sub(1) as f32;
    let scale = ctx.pixels_per_point().max(0.01);
    let height = ((content + 2.0 * CARD_PADDING_Y) * scale).round() / scale;
    let leading = match template.leading {
        Some(_) => {
            line_heights.get(template.title_line()).copied().unwrap_or(0.0) + CARD_ICON_GAP
        }
        None => 0.0,
    };
    CardMetrics {
        line_heights,
        leading,
        height,
    }
}

/// Match positions per field. The highlighted fields are joined in template
/// order and matched once, so a query whose terms fall in different fields
/// still highlights each of them; positions are mapped back to each field.
fn card_match_positions(
    template: &RowTemplate,
    fields: &RowFields,
    query: &str,
) -> std::collections::HashMap<usize, Vec<usize>> {
    let mut positions = std::collections::HashMap::new();
    if query.is_empty() {
        return positions;
    }
    let mut joined = String::new();
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    for slot in template.lines.iter().flat_map(|line| &line.slots) {
        if !slot.highlight || ranges.iter().any(|&(field, ..)| field == slot.field) {
            continue;
        }
        if !joined.is_empty() {
            joined.push_str("  ");
        }
        let start = joined.len();
        joined.push_str(fields.get(slot.field));
        ranges.push((slot.field, start, joined.len()));
    }
    let mut matched = resolve_match_positions(query, &joined);
    matched.sort_unstable();
    matched.dedup();
    for (field, start, end) in ranges {
        let local: Vec<usize> = matched
            .iter()
            .filter(|&&p| p >= start && p < end)
            .map(|&p| p - start)
            .collect();
        if !local.is_empty() {
            positions.insert(field, local);
        }
    }
    positions
}

/// Paint one card in `rect` and return its accessible label: the text slots'
/// values (icon references are not words, so icons are left out).
#[allow(clippy::too_many_arguments)]
fn paint_card(
    ctx: &egui::Context,
    painter: &egui::Painter,
    rect: egui::Rect,
    template: &RowTemplate,
    metrics: &CardMetrics,
    fields: &RowFields,
    query: &str,
    selected: bool,
    icons: &mut IconCache,
    appearance: &Appearance,
) -> String {
    let positions = card_match_positions(template, fields, query);
    let no_positions = Vec::new();
    let text_left = rect.left() + 12.0 + metrics.leading;
    let text_right = rect.right() - RESULT_RIGHT_PADDING - 8.0;
    let mut label = Vec::new();
    let mut y = rect.top() + CARD_PADDING_Y;
    let title_line = template.title_line();
    for (line_index, (line, &height)) in
        template.lines.iter().zip(&metrics.line_heights).enumerate()
    {
        let center_y = y + height / 2.0;
        if line_index == title_line
            && let Some(leading) = &template.leading
        {
            let size = metrics.leading - CARD_ICON_GAP;
            let square = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 12.0, center_y - size / 2.0),
                egui::vec2(size, size),
            );
            paint_icon(ctx, painter, square, fields.get(leading.field), icons, appearance);
        }
        let mut left = text_left;
        let mut right = text_right;
        let mut paint_slot = |slot: &RowSlot, available: f32, anchor: f32, align: RowAlign| -> f32 {
            let text = fields.get(slot.field);
            if text.is_empty() || available <= 0.0 {
                return 0.0;
            }
            if slot.kind == SlotKind::Icon {
                if available < height {
                    return 0.0;
                }
                let x = match align {
                    RowAlign::Left => anchor,
                    RowAlign::Right => anchor - height,
                };
                let square = egui::Rect::from_min_size(
                    egui::pos2(x, center_y - height / 2.0),
                    egui::vec2(height, height),
                );
                let painted = paint_icon(ctx, painter, square, text, icons, appearance);
                return if painted { height } else { 0.0 };
            }
            let slot_positions = if slot.highlight {
                positions.get(&slot.field).unwrap_or(&no_positions)
            } else {
                &no_positions
            };
            let window = ctx.fonts_mut(|fonts| {
                result_window(text, slot_positions, available, |c, matched| {
                    fonts.glyph_width(&card_font(slot.style, matched, appearance), c)
                })
            });
            let ellipsis = egui::TextFormat {
                font_id: card_font(slot.style, false, appearance),
                color: appearance.palette.muted,
                ..Default::default()
            };
            let job = window_layout(text, window, slot_positions, ellipsis, |matched| {
                egui::TextFormat {
                    font_id: card_font(slot.style, matched, appearance),
                    color: if matched {
                        appearance.palette.match_highlight
                    } else {
                        card_color(slot.style, selected, appearance)
                    },
                    ..Default::default()
                }
            });
            let galley = painter.layout_job(job);
            let width = galley.size().x;
            let x = match align {
                RowAlign::Left => anchor,
                RowAlign::Right => anchor - width,
            };
            painter.galley(
                egui::pos2(x, center_y - galley.size().y / 2.0),
                galley,
                appearance.palette.text,
            );
            width
        };
        let gap = |slot: &RowSlot| match slot.kind {
            SlotKind::Icon => CARD_ICON_GAP,
            SlotKind::Text => CARD_SLOT_GAP,
        };
        // Right slots first, from the right edge inward, so they keep their
        // width; left slots then share what is left, in order.
        let line_width = (right - left).max(0.0);
        for slot in line
            .slots
            .iter()
            .rev()
            .filter(|s| s.align == RowAlign::Right)
        {
            let budget = (line_width * CARD_RIGHT_SHARE - (text_right - right)).min(right - left);
            let width = paint_slot(slot, budget, right, RowAlign::Right);
            if width > 0.0 {
                right -= width + gap(slot);
            }
        }
        for slot in line.slots.iter().filter(|s| s.align == RowAlign::Left) {
            let width = paint_slot(slot, right - left, left, RowAlign::Left);
            if width > 0.0 {
                left += width + gap(slot);
            }
        }
        for slot in line.slots.iter().filter(|s| s.kind == SlotKind::Text) {
            let text = fields.get(slot.field);
            if !text.is_empty() {
                label.push(text);
            }
        }
        y += height + CARD_LINE_GAP;
    }
    label.join(" — ")
}

/// Paint a one-line result: `display` with query matches highlighted, shifted
/// so the last match stays visible.
fn paint_result_line(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    inset: f32,
    display: &str,
    query: &str,
    selected: bool,
    appearance: &Appearance,
) {
    let mut positions = resolve_match_positions(query, display);
    positions.sort_unstable();
    positions.dedup();
    let text_left = rect.left() + 12.0 + inset;
    let available = (rect.right() - text_left - RESULT_RIGHT_PADDING).max(0.0);
    let typography = &appearance.typography;
    let window = ui.fonts_mut(|fonts| {
        result_window(display, &positions, available, |c, matched| {
            let font = if matched {
                &typography.bold
            } else {
                &typography.normal
            };
            fonts.glyph_width(font, c)
        })
    });
    let job = result_window_layout(display, window, &positions, selected, appearance);
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
        galley,
        appearance.palette.text,
    );
}

/// Rows that fit in `space` below a query row of `row_height`, each
/// `result_height` tall. One-line results keep the original arithmetic.
fn rows_below_query(space: f32, row_height: f32, result_height: f32) -> f32 {
    if result_height == row_height {
        space / row_height - 1.0
    } else {
        (space - row_height) / result_height
    }
}

/// Lay out `window` of `text`, one section per run of matched or unmatched
/// characters. `positions` must be sorted.
fn result_window_layout(
    text: &str,
    window: ResultWindow,
    positions: &[usize],
    selected: bool,
    appearance: &Appearance,
) -> egui::text::LayoutJob {
    let ellipsis = egui::TextFormat {
        font_id: appearance.typography.normal.clone(),
        color: appearance.palette.muted,
        ..Default::default()
    };
    window_layout(text, window, positions, ellipsis, |matched| {
        result_format(matched, selected, appearance)
    })
}

/// Lay out `window` of `text` with `format(matched)` for each run and
/// `ellipsis` for the markers of hidden text. `positions` must be sorted.
fn window_layout(
    text: &str,
    window: ResultWindow,
    positions: &[usize],
    ellipsis: egui::TextFormat,
    format: impl Fn(bool) -> egui::TextFormat,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if window.leading {
        job.append(&RESULT_ELLIPSIS.to_string(), 0.0, ellipsis.clone());
    }
    let mut next = positions.partition_point(|&p| p < window.start);
    let mut run = String::new();
    let mut run_matched = false;
    for (offset, c) in text[window.start..window.end].char_indices() {
        let offset = window.start + offset;
        while positions.get(next).is_some_and(|&p| p < offset) {
            next += 1;
        }
        let matched = positions.get(next) == Some(&offset);
        if matched != run_matched && !run.is_empty() {
            job.append(&run, 0.0, format(run_matched));
            run.clear();
        }
        run_matched = matched;
        run.push(result_display_char(c));
    }
    if !run.is_empty() {
        job.append(&run, 0.0, format(run_matched));
    }
    if window.trailing {
        job.append(&RESULT_ELLIPSIS.to_string(), 0.0, ellipsis);
    }
    job
}

/// The rest of `name` after `typed`, when `typed` starts it, ignoring case.
fn name_rest<'a>(name: &'a str, typed: &str) -> Option<&'a str> {
    let start = name.get(..typed.len())?;
    (!typed.is_empty() && name.len() > typed.len() && start.eq_ignore_ascii_case(typed))
        .then(|| &name[typed.len()..])
}
/// The query prompt while searching.
const SEARCH_PROMPT: char = '>';
/// The query prompt in command mode.
const COMMAND_PROMPT: char = ':';
/// Space between a completion and its detail.
const DETAIL_GAP: &str = "    ";
/// A completion as the popup shows it, for measuring.
fn completion_label(completion: &nfm_search_core::store::SearchCompletion) -> String {
    if completion.detail.is_empty() {
        completion.text.clone()
    } else {
        format!("{}{DETAIL_GAP}{}", completion.text, completion.detail)
    }
}
fn highlighted_layout(
    text: &str,
    positions: &[usize],
    selected: bool,
    appearance: &Appearance,
) -> egui::text::LayoutJob {
    let mut positions = positions.to_vec();
    positions.sort_unstable();
    positions.dedup();
    let whole = ResultWindow {
        start: 0,
        end: text.len(),
        leading: false,
        trailing: false,
    };
    result_window_layout(text, whole, &positions, selected, appearance)
}
#[cfg(test)]
mod query_parity_tests {
    use super::*;
    use nfm_picker_sources::structured::{
        StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore,
    };
    use nfm_search_core::snapshot_store::SnapshotStore;
    fn make() -> (Context, Picker<StructuredStreamingSnapshot>) {
        let ctx = Context::default();
        let mut rows = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into(), "CPU".into()]).unwrap(),
        );
        for row in [
            ["alpha one", "Running", "10"],
            ["beta", "Stopped", "20"],
            ["猫 server", "Running", "30"],
        ] {
            rows.add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let store = Arc::new(SnapshotStore::new());
        store.publish(rows.snapshot());
        store.complete();
        (
            ctx.clone(),
            Picker::new(
                "query-parity",
                &ctx,
                store,
                PickerConfig {
                    preview_visible: false,
                    ..Default::default()
                },
            ),
        )
    }
    fn wait(picker: &mut Picker<StructuredStreamingSnapshot>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn hosts_focus_the_preview_without_toggling_it_back_off() {
        let (ctx, mut picker) = make();
        picker.set_query("a");
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "preview");
        assert!(!picker.preview_focused());
        assert!(picker.focus_preview(&ctx));
        assert!(picker.focus_preview(&ctx));
        assert!(picker.preview_focused());
    }

    #[test]
    fn full_pane_shortcut_retains_selection_restores_placement_and_resets_copy() {
        let (ctx, mut picker) = make();
        picker.set_query("a");
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "preview");
        picker.toggle_preview_focus(&ctx);
        picker.preview_viewport.as_mut().unwrap().mode.cursor.x = 3;
        let selected = picker.selection().unwrap().index;
        let shortcut = || modified_key(Key::F, Modifiers::CTRL | Modifiers::SHIFT);
        draw(
            &ctx,
            &mut picker,
            vec![shortcut(), egui::Event::Text("F".into())],
        );
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
        assert_eq!(picker.query(), "a");
        assert_eq!(
            picker.effective_position(crate::Position::Center),
            crate::Position::FullPane
        );
        assert_eq!(picker.selection().unwrap().index, selected);
        assert!(!picker.preview_focused());
        // The preview reloads at the new size; any viewport is a fresh one.
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .is_none_or(|copy| copy.mode.cursor.x == 0)
        );
        let mut repeat = shortcut();
        if let egui::Event::Key { repeat, .. } = &mut repeat {
            *repeat = true;
        }
        draw(&ctx, &mut picker, vec![repeat]);
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
        let mut release = shortcut();
        if let egui::Event::Key { pressed, .. } = &mut release {
            *pressed = false;
        }
        draw(&ctx, &mut picker, vec![release]);
        draw(&ctx, &mut picker, vec![shortcut()]);
        assert_eq!(picker.placement_override, None);
        assert_eq!(picker.selection().unwrap().index, selected);
        assert_eq!(
            picker.effective_position(crate::Position::Center),
            crate::Position::Center
        );
    }

    #[test]
    fn full_pane_shortcut_restores_host_geometry_including_full_pane_default() {
        for configured in [crate::Position::Cursor, crate::Position::FullPane] {
            let (ctx, mut picker) = make();
            wait(&mut picker);
            picker.set_external_preview(true);
            let bounds =
                egui::Rect::from_min_size(egui::pos2(20.0, 30.0), egui::vec2(800.0, 700.0));
            let mut placement = Placement::new(bounds);
            placement.position = configured;
            placement.width = 400.0;
            placement.anchor = Some(egui::pos2(100.0, 150.0));
            let mut original = None;
            for step in 0..5 {
                let events = if step == 1 || step == 3 {
                    vec![modified_key(Key::F, Modifiers::CTRL | Modifiers::SHIFT)]
                } else if step == 2 {
                    let mut release = modified_key(Key::F, Modifiers::CTRL | Modifiers::SHIFT);
                    if let egui::Event::Key { pressed, .. } = &mut release {
                        *pressed = false;
                    }
                    vec![release]
                } else {
                    vec![]
                };
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 900.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let output =
                            picker.show(ui.ctx(), placement, &Appearance::default(), false);
                        if step == 0 {
                            original = Some(output.rect);
                        }
                        if step == 1 {
                            if configured == crate::Position::FullPane {
                                assert_ne!(output.rect, bounds);
                            } else {
                                assert_eq!(output.rect, bounds);
                            }
                        }
                        if step >= 3 {
                            assert_eq!(output.rect, original.unwrap());
                        }
                    },
                )
                .drop_without_applying_deltas();
            }
        }
    }

    #[test]
    fn palette_lists_commands_with_their_current_keys() {
        let (_, mut picker) = make();
        picker.set_bindings(vec![
            PickerBinding {
                trigger: PickerTrigger::Key(Key::F),
                modifiers: Modifiers::CTRL | Modifiers::SHIFT,
                action: PickerAction::Custom(5),
                description: Some("Custom command".into()),
            },
            PickerBinding {
                trigger: PickerTrigger::DoubleClick,
                modifiers: Modifiers::ALT,
                action: PickerAction::Custom(6),
                description: Some("Open clicked item".into()),
            },
            PickerBinding {
                trigger: PickerTrigger::Key(Key::G),
                modifiers: Modifiers::CTRL,
                action: PickerAction::ToggleFullPane,
                description: None,
            },
            // Undescribed custom commands stay out of the palette.
            PickerBinding {
                trigger: PickerTrigger::Key(Key::H),
                modifiers: Modifiers::CTRL,
                action: PickerAction::Custom(7),
                description: None,
            },
            PickerBinding {
                trigger: PickerTrigger::Key(Key::L),
                modifiers: Modifiers::CTRL | Modifiers::SHIFT,
                action: PickerAction::Unbind,
                description: None,
            },
        ]);
        let rows = picker.palette_rows();
        let row = |title: &str| rows.iter().find(|row| row.title == title).unwrap();
        // Built-in commands come first, in order, with their current keys,
        // followed only by described custom commands.
        let actions: Vec<_> = rows.iter().map(|row| row.action).collect();
        // Without query commands there is no command mode to toggle.
        let built_in: Vec<_> = PickerAction::BUILT_IN
            .into_iter()
            .filter(|action| *action != PickerAction::ToggleCommandMode)
            .collect();
        assert_eq!(actions[..built_in.len()], built_in);
        assert_eq!(
            actions[built_in.len()..],
            [PickerAction::Custom(5), PickerAction::Custom(6)]
        );
        picker.set_query_commands(vec![QueryCommand {
            name: "Reload".into(),
            description: String::new(),
            argument: None,
        }]);
        let rows = picker.palette_rows();
        let toggle = rows
            .iter()
            .find(|row| row.action == PickerAction::ToggleCommandMode)
            .unwrap();
        assert_eq!(
            (toggle.title.as_str(), toggle.keys.as_str()),
            ("Toggle command mode", "Ctrl+;")
        );
        assert_eq!(row("Toggle full-pane picker").keys, "Ctrl+G");
        assert_eq!(row("Toggle preview").keys, "Ctrl+P, Ctrl+Space");
        assert_eq!(row("Command palette").keys, "Ctrl+Shift+/, Ctrl+Shift+?");
        assert_eq!(row("Scroll preview up a line").keys, "Ctrl+Up");
        // Unbound commands stay runnable from the palette.
        assert_eq!(row("Toggle line numbers").keys, "");
        assert_eq!(row("Toggle relative line numbers").keys, "");
        assert_eq!(row("Custom command").keys, "Ctrl+Shift+F");
        assert_eq!(row("Open clicked item").keys, "Alt+Double click");
    }

    #[test]
    fn command_names_round_trip() {
        for action in PickerAction::BUILT_IN
            .into_iter()
            .chain([PickerAction::Unbind])
        {
            assert_eq!(
                PickerAction::from_name(action.name().unwrap()),
                Some(action)
            );
        }
        assert_eq!(PickerAction::from_name("None"), Some(PickerAction::Unbind));
        assert_eq!(PickerAction::Custom(3).name(), None);
        assert_eq!(PickerAction::from_name("Missing"), None);
    }

    /// Press and release a chord, so egui does not report the next press of
    /// the same key as a repeat.
    fn tap(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        key: Key,
        modifiers: Modifiers,
    ) -> Option<PickerEvent<nfm_picker_sources::structured::StructuredPickerItem>> {
        let (_, event) = draw(ctx, picker, vec![modified_key(key, modifiers)]);
        let mut release = modified_key(key, modifiers);
        if let egui::Event::Key { pressed, .. } = &mut release {
            *pressed = false;
        }
        draw(ctx, picker, vec![release]);
        event
    }

    #[test]
    fn line_number_toggles_combine_like_vim_and_work_in_copy_mode() {
        use PickerAction::{
            TogglePreviewLineNumbers as Number, ToggleRelativePreviewLineNumbers as Relative,
        };
        use PreviewLineNumbers as Style;
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "one\ntwo");
        let wide = picker.preview_key.unwrap().2;

        tap(&ctx, &mut picker, Key::L, ctrl_shift);
        assert_eq!(picker.preview_line_numbers(), Style::Absolute);
        // The gutter takes columns, so the preview is requested narrower.
        assert!(picker.preview_key.unwrap().2 < wide);

        load_preview(&mut picker);
        tap(&ctx, &mut picker, Key::W, Modifiers::CTRL);
        assert!(picker.preview_focused());
        tap(&ctx, &mut picker, Key::L, ctrl_shift);
        assert_eq!(picker.preview_line_numbers(), Style::Off);
        assert!(picker.preview_focused());

        // Each command flips one switch: number, relativenumber.
        for (action, expected) in [
            (Relative, Style::Relative),
            (Number, Style::Hybrid),
            (Relative, Style::Absolute),
            (Relative, Style::Hybrid),
            (Number, Style::Relative),
            (Relative, Style::Off),
        ] {
            picker.execute(&ctx, action);
            draw(&ctx, &mut picker, vec![]);
            assert_eq!(picker.preview_line_numbers(), expected, "after {action:?}");
        }
    }

    #[test]
    fn unbound_default_chords_do_nothing() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_bindings(vec![PickerBinding {
            trigger: PickerTrigger::Key(Key::L),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            action: PickerAction::Unbind,
            description: None,
        }]);
        tap(
            &ctx,
            &mut picker,
            Key::L,
            Modifiers::CTRL | Modifiers::SHIFT,
        );
        assert_eq!(picker.preview_line_numbers(), PreviewLineNumbers::Off);
        assert!(!picker.config.bindings.iter().any(|binding| {
            binding.action == PickerAction::TogglePreviewLineNumbers
                || binding.action == PickerAction::Unbind
        }));
    }

    /// The open palette's matching commands, once its search has caught up
    /// with its query.
    fn palette_matches(picker: &mut Picker<StructuredStreamingSnapshot>) -> Vec<PickerAction> {
        let palette = picker.palette.as_mut().expect("palette is open");
        wait(&mut palette.picker);
        palette
            .picker
            .displayed
            .as_ref()
            .unwrap()
            .output
            .results
            .iter()
            .map(|result| palette.actions[result.node_index])
            .collect()
    }

    #[test]
    fn palette_searches_highlights_runs_built_in_and_custom_commands_and_closes() {
        let (ctx, mut picker) = make();
        picker.set_query("a");
        wait(&mut picker);
        picker.set_bindings(vec![PickerBinding {
            trigger: PickerTrigger::Key(Key::K),
            modifiers: Modifiers::CTRL,
            action: PickerAction::Custom(9),
            description: Some("Open in editor".into()),
        }]);
        draw(&ctx, &mut picker, vec![]);
        let open = |picker: &mut Picker<_>| {
            tap(&ctx, picker, Key::Slash, Modifiers::CTRL | Modifiers::SHIFT);
            assert!(picker.keybinding_help_visible());
            assert_eq!(picker.palette.as_ref().unwrap().picker.query(), "");
        };

        // Fuzzy search ranks the closest command first.
        open(&mut picker);
        draw(&ctx, &mut picker, vec![egui::Event::Text("tgrel".into())]);
        assert_eq!(
            palette_matches(&mut picker)[0],
            PickerAction::ToggleRelativePreviewLineNumbers
        );
        // Matched characters are highlighted.
        let (output, _) = draw(&ctx, &mut picker, vec![]);
        let highlight = Appearance::default().palette.match_highlight;
        assert!(output.shapes.iter().any(|shape| matches!(
            &shape.shape,
            egui::Shape::Text(text) if text.galley.job.text.contains("Toggle relative line numbers")
                && text.galley.job.sections.iter().any(|section| section.format.color == highlight)
        )));
        draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(!picker.keybinding_help_visible());
        // The chosen command runs on the next frame.
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.preview_line_numbers(), PreviewLineNumbers::Relative);
        // Typing went to the palette, not the query.
        assert_eq!(picker.query(), "a");

        // Arrows choose among the matches.
        open(&mut picker);
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("line num".into())],
        );
        let matches = palette_matches(&mut picker);
        assert_eq!(matches.len(), 2, "{matches:?}");
        assert!(matches.contains(&PickerAction::TogglePreviewLineNumbers));
        let relative = matches
            .iter()
            .position(|action| *action == PickerAction::ToggleRelativePreviewLineNumbers)
            .unwrap();
        let mut keys = vec![key(Key::ArrowDown); relative];
        keys.push(key(Key::Enter));
        draw(&ctx, &mut picker, keys);
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.preview_line_numbers(), PreviewLineNumbers::Off);

        // Structured filters work on the Keys column.
        open(&mut picker);
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("/:Keys==Ctrl+K".into())],
        );
        assert_eq!(palette_matches(&mut picker), [PickerAction::Custom(9)]);
        // Close the completion popup so Enter runs the command.
        draw(&ctx, &mut picker, vec![]);
        if !picker
            .palette
            .as_ref()
            .unwrap()
            .picker
            .suggestions
            .is_empty()
        {
            draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        }
        assert!(picker.keybinding_help_visible());
        draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        let (_, event) = draw(&ctx, &mut picker, vec![]);
        assert!(matches!(
            event,
            Some(PickerEvent::CustomAction { action: 9, .. })
        ));

        // A query with no match runs nothing; the palette key still closes it.
        open(&mut picker);
        draw(&ctx, &mut picker, vec![egui::Event::Text("zzz".into())]);
        assert!(palette_matches(&mut picker).is_empty());
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(event.is_none());
        assert!(picker.keybinding_help_visible());
        tap(
            &ctx,
            &mut picker,
            Key::Slash,
            Modifiers::CTRL | Modifiers::SHIFT,
        );
        assert!(!picker.keybinding_help_visible());

        // Escape closes the palette only.
        open(&mut picker);
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(event.is_none());
        assert!(!picker.keybinding_help_visible());
        assert!(!picker.is_closed());
        assert_eq!(picker.query(), "a");
    }

    #[test]
    fn executed_commands_run_on_the_next_frame() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        picker.execute(&ctx, PickerAction::TogglePreviewLineNumbers);
        assert_eq!(picker.preview_line_numbers(), PreviewLineNumbers::Off);
        let (_, event) = draw(&ctx, &mut picker, vec![]);
        assert!(event.is_none());
        assert_eq!(picker.preview_line_numbers(), PreviewLineNumbers::Absolute);
        picker.execute(&ctx, PickerAction::Custom(4));
        let (_, event) = draw(&ctx, &mut picker, vec![]);
        assert!(matches!(
            event,
            Some(PickerEvent::CustomAction { action: 4, .. })
        ));
    }

    #[test]
    fn alt_z_toggles_preview_wrap_and_requests_the_preview_again() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        assert!(picker.preview_wrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                move |request: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                      _: crate::Cancellation| {
                    record.lock().unwrap().push(request.wrap_text);
                    Ok(Preview::Text("one\ntwo".into()))
                },
            ),
        );
        draw(&ctx, &mut picker, vec![]);
        load_preview(&mut picker);
        assert_eq!(seen.lock().unwrap().last(), Some(&true));
        tap(&ctx, &mut picker, Key::Z, Modifiers::ALT);
        assert!(!picker.preview_wrap());
        // The toggle drops the old layout; the preview is asked for again unwrapped.
        draw(&ctx, &mut picker, vec![]);
        load_preview(&mut picker);
        assert_eq!(seen.lock().unwrap().last(), Some(&false));
        tap(&ctx, &mut picker, Key::Z, Modifiers::ALT);
        assert!(picker.preview_wrap());
    }

    #[test]
    fn ctrl_p_moves_between_matches_while_copy_search_is_open() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "foo\nbar\nfoo\nbar");
        tap(&ctx, &mut picker, Key::W, Modifiers::CTRL);
        assert!(picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![key(Key::Slash), egui::Event::Text("/".into())],
        );
        draw(&ctx, &mut picker, vec![egui::Event::Text("foo".into())]);
        let viewport = picker.preview_viewport.as_ref().unwrap();
        assert!(viewport.search_input_active());
        let before = viewport.mode.cursor;
        tap(&ctx, &mut picker, Key::P, Modifiers::CTRL);
        assert!(picker.preview_visible());
        assert!(picker.preview_focused());
        assert_ne!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            before
        );
    }

    #[test]
    fn binding_updates_merge_defaults_replace_chords_and_allow_builtin_remapping() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        for _ in 0..3 {
            picker.set_bindings(Vec::new());
        }
        let defaults = default_bindings().len();
        assert_eq!(picker.config.bindings.len(), defaults);
        let chord = |picker: &Picker<_>, key| {
            picker
                .config
                .bindings
                .iter()
                .find(|binding| binding.trigger == PickerTrigger::Key(key))
                .map(|binding| binding.action)
        };
        assert_eq!(chord(&picker, Key::F), Some(PickerAction::ToggleFullPane));
        let mut modifiers = Modifiers::CTRL | Modifiers::SHIFT;
        modifiers.command = true;
        picker.set_bindings(vec![
            PickerBinding {
                trigger: PickerTrigger::Key(Key::F),
                modifiers,
                action: PickerAction::Custom(1),
                description: None,
            },
            PickerBinding {
                trigger: PickerTrigger::Key(Key::F),
                modifiers,
                action: PickerAction::Custom(2),
                description: None,
            },
            PickerBinding {
                trigger: PickerTrigger::Key(Key::G),
                modifiers: Modifiers::CTRL,
                action: PickerAction::ToggleFullPane,
                description: None,
            },
        ]);
        assert_eq!(picker.config.bindings.len(), defaults + 1);
        assert_eq!(chord(&picker, Key::F), Some(PickerAction::Custom(2)));
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::G, Modifiers::CTRL)],
        );
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
    }

    #[test]
    fn full_pane_shortcut_works_without_preview_and_respects_custom_binding() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        let shortcut = || modified_key(Key::F, Modifiers::CTRL | Modifiers::SHIFT);
        let selected = picker.selection().unwrap().index;
        assert!(picker.preview.is_none() && !picker.external_preview);
        draw(&ctx, &mut picker, vec![shortcut()]);
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
        assert_eq!(picker.selection().unwrap().index, selected);
        assert!(picker.visible_result_rows > picker.config.result_rows);
        let mut release = shortcut();
        if let egui::Event::Key { pressed, .. } = &mut release {
            *pressed = false;
        }
        draw(&ctx, &mut picker, vec![release]);
        picker.set_bindings(vec![PickerBinding {
            trigger: PickerTrigger::Key(Key::F),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            action: PickerAction::Custom(42),
            description: None,
        }]);
        let (_, event) = draw(&ctx, &mut picker, vec![shortcut()]);
        assert!(matches!(
            event,
            Some(PickerEvent::CustomAction { action: 42, .. })
        ));
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
    }

    /// Press and release the default full-pane chord.
    fn toggle_full_pane(ctx: &Context, picker: &mut Picker<StructuredStreamingSnapshot>) {
        let shortcut = modified_key(Key::F, Modifiers::CTRL | Modifiers::SHIFT);
        let mut release = shortcut.clone();
        if let egui::Event::Key { pressed, .. } = &mut release {
            *pressed = false;
        }
        draw(ctx, picker, vec![shortcut]);
        draw(ctx, picker, vec![release]);
    }

    #[test]
    fn full_pane_shows_a_hidden_preview_and_hides_it_again_on_exit() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        picker.config.preview_visible = false;
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.visible_preview_rows, 0);

        toggle_full_pane(&ctx, &mut picker);
        assert_eq!(picker.placement_override, Some(crate::Position::FullPane));
        assert!(picker.preview_visible());
        assert!(picker.visible_preview_rows > 0);

        toggle_full_pane(&ctx, &mut picker);
        assert_eq!(picker.placement_override, None);
        assert!(!picker.preview_visible());
        assert_eq!(picker.visible_preview_rows, 0);
    }

    #[test]
    fn full_pane_keeps_preview_visibility_chosen_outside_the_toggle() {
        // Ctrl+P inside full pane is the user's choice; leaving keeps it.
        for presses in [1, 2] {
            let (ctx, mut picker) = make();
            wait(&mut picker);
            picker.set_external_preview(true);
            picker.config.preview_visible = false;
            toggle_full_pane(&ctx, &mut picker);
            for _ in 0..presses {
                draw(
                    &ctx,
                    &mut picker,
                    vec![modified_key(Key::P, Modifiers::CTRL)],
                );
            }
            let chosen = presses == 2;
            assert_eq!(picker.preview_visible(), chosen);
            toggle_full_pane(&ctx, &mut picker);
            assert_eq!(picker.placement_override, None);
            assert_eq!(picker.preview_visible(), chosen, "after {presses} Ctrl+P");
        }

        // A preview that was already visible stays visible after full pane.
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        picker.config.preview_visible = true;
        toggle_full_pane(&ctx, &mut picker);
        toggle_full_pane(&ctx, &mut picker);
        assert!(picker.preview_visible());

        // Without a preview there is nothing to show.
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = false;
        toggle_full_pane(&ctx, &mut picker);
        assert!(!picker.preview_visible());
    }

    #[test]
    fn preview_layout_reset_retains_results_and_exits_copy_focus() {
        let (ctx, mut picker) = make();
        picker.set_query("a");
        wait(&mut picker);
        picker.selected = 1;
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "one\ntwo\nthree");
        picker.toggle_preview_focus(&ctx);
        assert!(picker.preview_focused());
        let selected = picker.selection().unwrap().index;

        picker.reset_preview_layout(&ctx);
        assert_eq!(picker.query(), "a");
        assert_eq!(picker.selection().unwrap().index, selected);
        assert!(!picker.preview_focused());
        assert!(picker.preview_viewport.is_none());
        assert!(picker.preview.as_ref().unwrap().document.is_none());
        assert!(picker.preview_key.is_none());
        assert!(picker.result_actions_enabled);
    }

    #[test]
    fn source_reload_discards_selection_and_focused_preview_but_keeps_query() {
        let (ctx, mut picker) = make();
        picker.set_query("a");
        wait(&mut picker);
        picker.selected = 1;
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "one\ntwo\nthree");
        picker.toggle_preview_focus(&ctx);
        assert!(picker.preview_focused());
        assert!(picker.selection().is_some());

        picker.clear_for_source_reload(&ctx);
        assert_eq!(picker.query(), "a");
        assert!(picker.selection().is_none());
        assert!(!picker.preview_focused());
        assert!(picker.preview_viewport.is_none());
        assert!(picker.preview.is_none());
        assert!(!picker.result_actions_enabled);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.scroll_offset, 0);
        picker.poll();
        assert!(picker.selection().is_none());

        let mut rows = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into(), "CPU".into()]).unwrap(),
        );
        rows.add_record(&csv::StringRecord::from(vec![
            "alpha reset",
            "Running",
            "1",
        ]))
        .unwrap();
        let store = Arc::new(SnapshotStore::new());
        store.publish(rows.snapshot());
        store.complete();
        picker.replace_source(&ctx, store, picker.query().to_owned());
        picker.set_result_actions_enabled(true);
        wait(&mut picker);
        assert_eq!(picker.query(), "a");
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.selection().unwrap().index, 0);
    }
    fn draw(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        events: Vec<egui::Event>,
    ) -> (
        egui::FullOutput,
        Option<PickerEvent<nfm_picker_sources::structured::StructuredPickerItem>>,
    ) {
        let mut event = None;
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let output = picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
                if output.event.is_some() {
                    event = output.event;
                }
            },
        );
        frame.textures_delta.clear();
        (frame, event)
    }
    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    fn modified_key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn alt_space_quick_select_restores_focus_and_selects_without_accepting() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let text: String = (0..60)
            .map(|row| format!("first{row} second{row}\n"))
            .collect();
        show_text(&ctx, &mut picker, text);
        let selected = picker.selected;
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(event.is_none());
        assert!(picker.preview_focused());
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        draw(&ctx, &mut picker, vec![key(Key::PageDown)]);
        assert!(picker.preview_viewport.as_ref().unwrap().top > 0);
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, 0);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        let (label, target) = picker
            .preview_viewport
            .as_ref()
            .unwrap()
            .first_quick_hint()
            .unwrap();
        let (_, event) = draw(&ctx, &mut picker, vec![egui::Event::Text(label)]);
        assert!(event.is_none());
        assert!(picker.preview_focused());
        assert!(
            !picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            target
        );
        assert_eq!(picker.query(), "");
        assert_eq!(picker.selected, selected);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(picker.preview_focused());
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            target
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(!picker.preview_focused());
    }

    #[test]
    fn host_can_suspend_result_acceptance_without_closing_the_picker() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_result_actions_enabled(false);
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(event.is_none());
        assert!(!picker.is_closed());
        picker.set_result_actions_enabled(true);
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(matches!(event, Some(PickerEvent::Accepted(_))));
    }

    #[test]
    fn leaving_preview_focus_keeps_viewport_and_results_paging_uses_it() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let text: String = (0..100).map(|row| format!("row{row:03}\n")).collect();
        show_text(&ctx, &mut picker, text);
        draw(&ctx, &mut picker, vec![]);
        assert!(!picker.preview_focused());
        assert!(picker.preview_viewport.is_some());
        picker.toggle_preview_focus(&ctx);
        picker.preview_viewport.as_mut().unwrap().top = 40;
        picker.preview_viewport.as_mut().unwrap().mode.cursor.y = 40;
        let document = picker.preview_viewport.as_ref().unwrap() as *const _;
        picker.toggle_preview_focus(&ctx);
        assert!(!picker.preview_focused());
        assert_eq!(picker.scroll_offset, 40);
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, 40);
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap() as *const _,
            document
        );
        picker.scroll_preview(1);
        let top = picker.preview_viewport.as_ref().unwrap().top;
        assert!(top > 40);
        picker.toggle_preview_focus(&ctx);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.preview_focused());
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, top);
    }

    #[test]
    fn ctrl_w_toggles_only_once_when_egui_repeats_a_layout_pass() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "preview text");
        for focused in [true, false] {
            let mut passes = 0;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 900.0),
                    )),
                    events: vec![modified_key(Key::W, Modifiers::CTRL)],
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                    passes += 1;
                    if passes == 1 {
                        ui.ctx().request_discard("focus regression test");
                    }
                },
            );
            output.textures_delta.clear();
            assert!(passes >= 2);
            assert_eq!(picker.preview_focused(), focused);
        }
    }

    #[test]
    fn ctrl_w_focuses_preview_yanks_and_returns_to_query_without_accepting() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "alpha beta\nsecond line\nlast");
        let selected = picker.selected;
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        assert!(event.is_none());
        draw(
            &ctx,
            &mut picker,
            vec![key(Key::W), egui::Event::Text("w".into())],
        );
        assert_eq!(picker.preview_viewport.as_ref().unwrap().mode.cursor.x, 6);
        assert_eq!(picker.query(), "");
        assert_eq!(picker.selected, selected);
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![key(Key::Y), key(Key::I), key(Key::W)],
        );
        assert!(event.is_none());
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("beta".into()))
        );
        draw(&ctx, &mut picker, vec![key(Key::V), key(Key::ArrowRight)]);
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("be".into()))
        );
        // Enter while preview-focused never accepts the picker item.
        assert!(draw(&ctx, &mut picker, vec![key(Key::Enter)]).1.is_none());
        assert!(!picker.is_closed());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        draw(&ctx, &mut picker, vec![egui::Event::Text("typed".into())]);
        assert_eq!(picker.query(), "typed");
        assert!(!picker.is_closed());
    }

    #[test]
    fn results_query_and_preview_share_padded_font_height() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "one\ntwo");
        let mut appearance = Appearance::default();
        appearance.typography.normal = egui::FontId::monospace(15.0);
        appearance.typography.bold = egui::FontId::monospace(15.0);
        appearance.typography.row_height = 30.0;
        let mut bounds = None;
        let mut line_height = 0.0;
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                line_height = ui.fonts_mut(|fonts| fonts.row_height(&appearance.typography.normal));
                bounds = picker
                    .show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &appearance,
                        false,
                    )
                    .preview_rect;
            },
        );
        frame.textures_delta.clear();
        let height = bounds.unwrap().height();
        assert!(
            (height / picker.visible_preview_rows as f32 - (line_height + 6.0)).abs()
                <= 0.51 / ctx.pixels_per_point(),
            "preview height={height}, rows={}, font height={line_height}",
            picker.visible_preview_rows
        );
        assert!(height < picker.visible_preview_rows as f32 * 30.0);
        assert!(
            (picker.query_rect.height() - height / picker.visible_preview_rows as f32).abs() < 1.0
        );
    }

    #[test]
    fn preview_search_escape_hide_and_missing_preview_preserve_picker_focus() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        picker.config.preview_visible = true;
        show_text(&ctx, &mut picker, "one two\nthree two");
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![key(Key::Slash), egui::Event::Text("/".into())],
        );
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("two".into()), key(Key::Enter)],
        );
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            crate::copy_mode::CellPosition { x: 4, y: 0 }
        );
        draw(&ctx, &mut picker, vec![key(Key::N)]);
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            crate::copy_mode::CellPosition { x: 6, y: 1 }
        );
        assert_eq!(picker.query(), "");
        draw(&ctx, &mut picker, vec![key(Key::Escape)]); // Clear search.
        assert!(picker.preview_focused());
        draw(&ctx, &mut picker, vec![key(Key::Escape)]); // Return to query.
        assert!(!picker.preview_focused());
        assert!(!picker.is_closed());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        assert!(!picker.preview_visible());
        assert!(!picker.is_closed());
    }
    #[test]
    fn query_edit_keeps_preview_shortcuts_working_when_selected_item_is_unchanged() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let resolutions = Arc::new(AtomicUsize::new(0));
        let calls = resolutions.clone();
        picker.set_preview_provider(
            &ctx,
            Arc::new(crate::byte_preview::BytePreviewProvider::new(
                move |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::byte_preview::ByteDocument::complete(
                        b"preview words\nsecond line".to_vec(),
                    ))
                },
            )),
        );
        let settle = |picker: &mut Picker<StructuredStreamingSnapshot>| {
            wait(picker);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                draw(&ctx, picker, vec![]);
                if !picker.preview.as_ref().unwrap().loading {
                    break;
                }
                assert!(Instant::now() < deadline, "preview did not finish");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        draw(&ctx, &mut picker, vec![egui::Event::Text("a".into())]);
        settle(&mut picker);
        let selected = picker.selection().unwrap().index;
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());

        draw(&ctx, &mut picker, vec![egui::Event::Text("l".into())]);
        assert_eq!(picker.query(), "al");
        settle(&mut picker);
        assert_eq!(picker.selection().unwrap().index, selected);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(
            picker.preview_focused(),
            "Ctrl+W stopped reopening the unchanged preview"
        );
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(
            picker.preview_focused(),
            "Alt+Space stopped opening quick select"
        );
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        assert_eq!(picker.selection().unwrap().index, selected);
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            1,
            "query edits reran the selected item's preview source"
        );
    }
    #[test]
    fn passive_copy_viewport_does_not_block_resize_for_current_selection() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.visible_preview_rows = 4;
        let provider = Arc::new(crate::byte_preview::BytePreviewProvider::new(
            |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                Ok(crate::byte_preview::ByteDocument::complete(
                    "a".repeat(200).into_bytes(),
                ))
            },
        ));
        picker.set_preview_provider(&ctx, provider.clone());
        let appearance = Appearance::default();
        picker.refresh_preview(400.0, &appearance);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().loading {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        picker.ensure_preview_viewport();
        assert!(picker.preview_viewport.is_some());
        assert!(!picker.preview_focused());
        let selection = picker.preview.as_ref().unwrap().document_selection.unwrap();
        let old_columns = picker.preview_key.unwrap().2;
        picker.toggle_preview_focus(&ctx);
        assert!(picker.preview_focused());
        picker.scroll_offset = 40;
        picker.reset_preview_layout(&ctx);
        assert!(!picker.preview_focused());
        assert_eq!(picker.scroll_offset, 0);
        assert!(
            picker
                .preview
                .as_ref()
                .unwrap()
                .document_selection
                .is_none()
        );
        assert!(picker.preview.as_ref().unwrap().document.is_none());
        picker.refresh_preview(800.0, &appearance);
        assert!(picker.preview_viewport.is_none());
        assert!(picker.preview.as_ref().unwrap().loading);
        let new_columns = picker.preview_key.unwrap().2;
        assert!(new_columns > old_columns);
        while picker.preview.as_ref().unwrap().loading {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            picker.preview.as_ref().unwrap().document_selection,
            Some(selection)
        );
        assert_eq!(
            provider
                .document_layout(selection.0, selection.1)
                .map(|layout| layout.map(|layout| layout.columns() as u16))
                .unwrap(),
            Some(new_columns)
        );
        picker.ensure_preview_viewport();
        assert!(picker.preview_viewport.is_some());
        picker.toggle_preview_focus(&ctx);
        picker.refresh_preview(400.0, &appearance);
        assert!(picker.preview_viewport.is_some());
        assert!(!picker.preview.as_ref().unwrap().loading);
    }

    fn load_preview(picker: &mut Picker<StructuredStreamingSnapshot>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().loading {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Preview every item as `text`, drawn once the selected item's page loads.
    fn show_text(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        text: impl Into<String>,
    ) {
        let text = text.into();
        picker.set_preview_provider(
            ctx,
            Arc::new(
                move |_: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                      _: crate::Cancellation| Ok(Preview::Text(text.clone())),
            ),
        );
        draw(ctx, picker, vec![]);
        load_preview(picker);
        draw(ctx, picker, vec![]);
    }

    /// Paint the preview; return muted gutter numbers as (top, right, text)
    /// sorted by row, and the leftmost content glyph origin.
    fn paint_preview_gutter(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        appearance: &Appearance,
    ) -> (Vec<(f32, f32, String)>, f32) {
        let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            picker.preview_ui(ui, appearance)
        });
        let muted = |text: &egui::epaint::TextShape| {
            text.galley.job.sections[0].format.color == appearance.palette.muted
        };
        let mut numbers: Vec<(f32, f32, String)> = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if muted(text) => {
                    let rect = text.visual_bounding_rect();
                    Some((rect.top(), rect.right(), text.galley.text().to_owned()))
                }
                _ => None,
            })
            .collect();
        numbers.sort_by(|a, b| a.0.total_cmp(&b.0));
        let content_left = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if !muted(text) => Some(text.pos.x),
                _ => None,
            })
            .fold(f32::INFINITY, f32::min);
        frame.drop_without_applying_deltas();
        (numbers, content_left)
    }

    #[test]
    fn changing_selection_keeps_the_gutter_while_the_next_preview_loads() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.visible_preview_rows = 6;
        picker.set_preview_line_numbers(PreviewLineNumbers::Absolute);
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = released.clone();
        let provider = Arc::new(crate::byte_preview::BytePreviewProvider::new(
            move |selection: &Selection<nfm_picker_sources::structured::StructuredPickerItem>,
                  _| {
                while selection.index == 1 && !gate.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let text: String = (0..40)
                    .map(|row| format!("{} {row}\n", selection.text))
                    .collect();
                Ok(crate::byte_preview::ByteDocument::complete(
                    text.into_bytes(),
                ))
            },
        ));
        picker.set_preview_provider(&ctx, provider);
        let appearance = Appearance::default();
        picker.refresh_preview(400.0, &appearance);
        load_preview(&mut picker);
        let (shown, content_left) = paint_preview_gutter(&ctx, &mut picker, &appearance);
        assert_eq!(shown.len(), 6);

        picker.move_selection(1);
        picker.refresh_preview(400.0, &appearance);
        assert!(picker.preview.as_ref().unwrap().loading);
        // The previous page stays put with its numbers until the next arrives.
        for _ in 0..3 {
            let (numbers, left) = paint_preview_gutter(&ctx, &mut picker, &appearance);
            assert_eq!(numbers, shown);
            assert_eq!(left, content_left);
        }
        released.store(true, std::sync::atomic::Ordering::Release);
        load_preview(&mut picker);
        let (numbers, left) = paint_preview_gutter(&ctx, &mut picker, &appearance);
        assert_eq!(numbers, shown);
        assert_eq!(left, content_left);
    }

    #[test]
    fn line_number_gutter_narrows_preview_requests_and_numbers_logical_lines() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.visible_preview_rows = 6;
        let mut text = format!("first\n{}\n", "a".repeat(100));
        for line in 3..=1200 {
            text.push_str(&format!("line {line}\n"));
        }
        let provider = Arc::new(crate::byte_preview::BytePreviewProvider::new(
            move |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                Ok(crate::byte_preview::ByteDocument::complete(
                    text.clone().into_bytes(),
                ))
            },
        ));
        picker.set_preview_provider(&ctx, provider);
        let appearance = Appearance::default();
        let cell = appearance.typography.cell_width;
        let width = 400.0;
        let columns = |gutter: usize| ((width - gutter as f32 * cell) / cell).floor() as u16;
        let paint = |picker: &mut Picker<StructuredStreamingSnapshot>| {
            paint_preview_gutter(&ctx, picker, &appearance)
        };

        picker.refresh_preview(width, &appearance);
        assert_eq!(picker.preview_key.unwrap().2, columns(0));
        picker.set_preview_line_numbers(PreviewLineNumbers::Absolute);
        picker.refresh_preview(width, &appearance);
        // Three digits and a separator before the line count is known.
        assert_eq!(picker.preview_key.unwrap().2, columns(4));
        load_preview(&mut picker);
        let (numbers, _) = paint(&mut picker);
        let texts: Vec<&str> = numbers.iter().map(|(_, _, text)| text.as_str()).collect();
        // Line 2 wraps over three rows; its continuations are unnumbered.
        assert_eq!(texts, ["1", "2", "3", "4"]);

        // 1200 lines need four digits; the next request uses the wider gutter.
        assert_eq!(picker.preview_gutter_columns(), 5);
        picker.refresh_preview(width, &appearance);
        assert_eq!(picker.preview_key.unwrap().2, columns(5));
        assert!(picker.preview_viewport.is_none());
        // While reloading, the same page keeps its numbers.
        let (numbers, _) = paint(&mut picker);
        assert_eq!(numbers.len(), 4);
        load_preview(&mut picker);
        let (numbers, content_left) = paint(&mut picker);
        for (_, right, _) in &numbers {
            assert!(*right <= 4.0 * cell + 0.5, "number overlaps separator");
        }
        assert!(content_left >= 5.0 * cell - 0.5, "content overlaps gutter");

        picker.set_preview_line_numbers(PreviewLineNumbers::Off);
        picker.refresh_preview(width, &appearance);
        assert_eq!(picker.preview_key.unwrap().2, columns(0));
        load_preview(&mut picker);
        let (numbers, content_left) = paint(&mut picker);
        assert!(numbers.is_empty());
        assert!(content_left < cell);
    }

    #[test]
    fn ansi_preview_copy_focus_pins_document_yanks_soft_wraps_and_survives_resize() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let provider = Arc::new(crate::byte_preview::BytePreviewProvider::new(
            |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                Ok(crate::byte_preview::ByteDocument::complete(
                    format!("{}猫e\u{301}tail\nlast", "a".repeat(130)).into_bytes(),
                ))
            },
        ));
        picker.set_preview_provider(&ctx, provider.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let (version, index) = picker.preview.as_ref().unwrap().document_selection.unwrap();
        let revision = provider
            .document_layout(version, index)
            .map(|layout| {
                layout.map(|layout| {
                    crate::copy_document::CopyDocument::metadata(&*layout)
                        .unwrap()
                        .revision
                })
            })
            .unwrap()
            .unwrap();
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y), key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(format!(
                    "{}猫e\u{301}tail\n",
                    "a".repeat(130)
                )))
        );
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(650.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
            },
        );
        assert!(picker.preview_focused());
        assert_eq!(
            provider
                .document_layout(version, index)
                .map(
                    |layout| layout.map(|layout| crate::copy_document::CopyDocument::metadata(
                        &*layout
                    )
                    .unwrap()
                    .revision)
                )
                .unwrap(),
            Some(revision)
        );
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y), key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(format!(
                    "{}猫e\u{301}tail\n",
                    "a".repeat(130)
                )))
        );
        picker.set_query("changed");
        assert!(!picker.preview_focused());
    }

    #[test]
    fn source_transition_retains_rows_and_preview_but_blocks_stale_acceptance() {
        let (ctx, mut picker) = make();
        picker.config.preview_visible = true;
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |request: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::Text(request.selection.item.fields["Name"].clone()))
                },
            ),
        );
        wait(&mut picker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let old_preview = picker
            .preview
            .as_ref()
            .unwrap()
            .document
            .clone()
            .unwrap()
            .unwrap();
        let old_text = picker.displayed_selection().unwrap().text;
        // Enter closes the worker; replacing its source must reopen safely.
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(matches!(event, Some(PickerEvent::Accepted(_))));
        let store = Arc::new(SnapshotStore::new());
        store.publish(StructuredStreamingSnapshot::from_rows(vec!["Name".into()], vec![]).unwrap());
        picker.replace_source(&ctx, store.clone(), "");
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.selection().is_none());
        assert_eq!(picker.displayed_selection().unwrap().text, old_text);
        assert_eq!(
            preview::plain_text(
                picker
                    .preview
                    .as_ref()
                    .unwrap()
                    .document
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
            ),
            preview::plain_text(&old_preview)
        );
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(event.is_none());
        assert!(!picker.is_closed());
        store.publish(
            StructuredStreamingSnapshot::from_rows(
                vec!["Name".into()],
                vec![vec!["replacement".into()]],
            )
            .unwrap(),
        );
        store.complete();
        wait(&mut picker);
        assert_eq!(
            picker.selection().unwrap().item.fields["Name"],
            "replacement"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&ctx, &mut picker, vec![]);
            if picker
                .preview
                .as_ref()
                .unwrap()
                .document
                .as_ref()
                .and_then(|d| d.as_ref().ok())
                .and_then(preview::plain_text)
                .as_deref()
                == Some("replacement")
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        // An empty completed directory must eventually remove retained content.
        let empty = Arc::new(SnapshotStore::new());
        empty.publish(StructuredStreamingSnapshot::from_rows(vec!["Name".into()], vec![]).unwrap());
        empty.complete();
        picker.replace_source(&ctx, empty, "");
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.selection().is_none());
        assert!(picker.displayed.as_ref().unwrap().output.results.is_empty());
        assert!(picker.preview.as_ref().unwrap().document.is_none());
    }
    #[test]
    fn shifted_slash_opens_help_without_editing_or_cancelling_the_query() {
        let (ctx, mut picker) = make();
        picker.set_query("beta");
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        let shifted_slash = egui::Event::Key {
            key: Key::Questionmark,
            physical_key: Some(Key::Slash),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
        };
        let (_, event) = draw(&ctx, &mut picker, vec![shifted_slash.clone()]);
        assert!(event.is_none());
        assert!(picker.keybinding_help_visible());
        assert_eq!(picker.query(), "beta");
        let (_, event) = draw(&ctx, &mut picker, vec![shifted_slash]);
        assert!(event.is_none());
        assert!(!picker.keybinding_help_visible());
        assert_eq!(picker.query(), "beta");
        assert!(!picker.is_closed());
    }
    /// Add forty described custom commands, enough to scroll the palette.
    fn add_host_commands(picker: &mut Picker<StructuredStreamingSnapshot>) {
        picker.set_bindings(
            Key::ALL
                .iter()
                .take(40)
                .enumerate()
                .map(|(index, key)| PickerBinding {
                    trigger: PickerTrigger::Key(*key),
                    modifiers: Modifiers::ALT | Modifiers::SHIFT,
                    action: PickerAction::Custom(index),
                    description: Some(format!("Host action {index}")),
                })
                .collect(),
        );
    }
    #[test]
    fn palette_fits_and_keeps_its_size_and_top_while_filtering() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        let mut appearance = Appearance::default();
        appearance.typography.normal = egui::FontId::monospace(15.0);
        appearance.typography.bold = egui::FontId::monospace(15.0);
        appearance.typography.row_height = 30.0;
        add_host_commands(&mut picker);
        picker.execute(&ctx, PickerAction::ToggleCommandPalette);
        for (width, height) in [
            (1600.0, 900.0),
            (1600.0, 296.0),
            (600.0, 296.0),
            (1600.0, 900.0),
        ] {
            let bounds = egui::Rect::from_min_size(egui::pos2(8.0, 8.0), egui::vec2(width, height));
            let frame = |picker: &mut Picker<StructuredStreamingSnapshot>| {
                let mut outer = egui::Rect::NOTHING;
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            bounds.max.to_vec2() + egui::vec2(8.0, 8.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        outer = picker
                            .show(ui.ctx(), Placement::new(bounds), &appearance, false)
                            .rect;
                    },
                )
                .drop_without_applying_deltas();
                outer
            };
            let mut panels = Vec::new();
            for filter in ["", "host action 3", "zzz"] {
                frame(&mut picker);
                picker.palette.as_mut().unwrap().picker.set_query(filter);
                palette_matches(&mut picker);
                frame(&mut picker);
                let outer = frame(&mut picker);
                let palette_id = picker.palette.as_ref().unwrap().picker.id;
                let panel = ctx.memory(|memory| memory.area_rect(palette_id)).unwrap();
                // The palette sits at the top of the picker, inside it.
                assert!(outer.contains_rect(panel), "{outer:?} {panel:?}");
                assert_eq!(panel.top(), outer.top() + PALETTE_MARGIN);
                panels.push(panel);
            }
            assert!(panels.iter().all(|panel| *panel == panels[0]), "{panels:?}");
        }
    }
    #[test]
    fn preview_copy_and_help_shortcuts_preserve_the_menu_and_selection() {
        let (ctx, mut picker) = make();
        picker.config.preview_visible = true;
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |_: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::Text("colored 猫\nsecond line".into()))
                },
            ),
        );
        wait(&mut picker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        add_host_commands(&mut picker);
        let selection = picker.selection().unwrap().index;
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![
                modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT),
                egui::Event::Copy,
            ],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        assert!(
            !output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(event.is_none());
        assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "colored 猫\nsecond line")));
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL)],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Slash, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(event.is_none());
        assert!(picker.keybinding_help_visible());
        assert!(!output.shapes.is_empty());
        // Keys go to the palette, not the picker under it.
        draw(
            &ctx,
            &mut picker,
            vec![
                key(Key::PageDown),
                key(Key::ArrowDown),
                egui::Event::Text("zzz ignored".into()),
            ],
        );
        assert!(palette_matches(&mut picker).is_empty());
        draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(picker.keybinding_help_visible());
        assert_eq!(picker.selection().unwrap().index, selection);
        assert_eq!(picker.query, "");
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(event.is_none());
        assert!(!picker.keybinding_help_visible());
        assert!(!picker.is_closed());
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(matches!(event, Some(PickerEvent::Cancelled)));
    }
    #[test]
    fn typed_command_filter_accepts_the_command_name() {
        let ctx = Context::default();
        let snapshot = StructuredStreamingSnapshot::from_rows(
            ["Name", "Type", "KeyBinding", "Description"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec![
                vec![
                    "fixture_first".into(),
                    "Action".into(),
                    "Ctrl+A".into(),
                    "First fixture".into(),
                ],
                vec![
                    "fixture_selected".into(),
                    "Action".into(),
                    "Ctrl+B".into(),
                    "Selected fixture".into(),
                ],
            ],
        )
        .unwrap();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot);
        store.complete();
        let mut picker = Picker::new(
            "swm-commands-test",
            &ctx,
            store,
            PickerConfig {
                preview_visible: false,
                ..Default::default()
            },
        );
        draw(&ctx, &mut picker, vec![]);
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("/:Name==fixture_selected ".into())],
        );
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.query, "/:Name==fixture_selected ");
        assert!(
            picker.suggestions.is_empty(),
            "suggestions {:?}",
            picker.suggestions
        );
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(
            matches!(event, Some(PickerEvent::Accepted(selection)) if selection.item.fields["Name"] == "fixture_selected")
        );
    }
    #[test]
    fn text_preview_paints_numbers_and_copies_as_a_document() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_preview_line_numbers(PreviewLineNumbers::Absolute);
        show_text(&ctx, &mut picker, "host preview fixture\nsecond");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        let painted: String = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(painted.contains("host preview fixture"));
        let muted = Appearance::default().palette.muted;
        let numbers: Vec<_> = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.sections[0].format.color == muted => {
                    Some(text.galley.text().to_owned())
                }
                _ => None,
            })
            // Other muted text, such as the match count, is not a line number.
            .filter(|text: &String| text.chars().all(|ch| ch.is_ascii_digit()))
            .collect();
        assert_eq!(numbers, ["1", "2"]);
        let (unfocused, _) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(
            !unfocused
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (frame, _) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(
                    "host preview fixture\nsecond".into()
                ))
        );
    }
    #[test]
    fn external_preview_bounds_follow_toggle_help_and_close_without_a_worker() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        picker.config.preview_visible = true;
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.preview.is_none());
        assert!(picker.preview_rect.unwrap().is_positive());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.preview_rect.is_none());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.preview_rect.unwrap().is_positive());
        picker.open_palette(&ctx);
        let mut bounds = Some(egui::Rect::EVERYTHING);
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                bounds = picker
                    .show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    )
                    .preview_rect;
            },
        );
        frame.textures_delta.clear();
        assert!(bounds.is_none(), "native preview must not cover help");
        picker.close();
        let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            bounds = picker
                .show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                )
                .preview_rect;
        });
        frame.textures_delta.clear();
        assert!(bounds.is_none());
    }
    #[test]
    fn preview_toggle_scroll_and_missing_preview_copy_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .all(|command| !matches!(command, egui::OutputCommand::CopyText(_)))
        );
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |_: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::Text(
                        (0..100).map(|row| format!("line {row}\n")).collect(),
                    ))
                },
            ),
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.config.preview_visible);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::PageDown, Modifiers::CTRL)],
        );
        assert_eq!(
            picker.scroll_offset,
            picker.visible_preview_rows.saturating_sub(1).max(1)
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::PageUp, Modifiers::CTRL)],
        );
        assert_eq!(picker.scroll_offset, 0);
        let selected = picker.selection().map(|s| s.index);
        draw(
            &ctx,
            &mut picker,
            vec![
                modified_key(Key::ArrowDown, Modifiers::CTRL),
                modified_key(Key::ArrowDown, Modifiers::CTRL),
            ],
        );
        assert_eq!(picker.scroll_offset, 2);
        assert_eq!(picker.selection().map(|s| s.index), selected);
        draw(
            &ctx,
            &mut picker,
            vec![
                modified_key(Key::ArrowUp, Modifiers::CTRL),
                modified_key(Key::ArrowUp, Modifiers::CTRL),
                modified_key(Key::ArrowUp, Modifiers::CTRL),
            ],
        );
        assert_eq!(picker.scroll_offset, 0);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(!picker.config.preview_visible);
    }
    #[test]
    fn all_skia_expression_operators_sorts_display_and_quoted_values_use_shared_parser() {
        let (_, mut picker) = make();
        for (query, expected) in [
            ("/:Status==Running ", 2),
            ("/:Status!=Stopped ", 2),
            ("/:Name=~^beta ", 1),
            ("/:Name!~^beta ", 2),
            ("/:CPU>20 ", 1),
            ("/:CPU>=20 ", 2),
            ("/:CPU<20 ", 1),
            ("/:CPU<=20 ", 2),
            ("/:Status==Running,Stopped ", 3),
            ("/:Name==\"猫 server\" ", 1),
            ("/:Status==Running /:CPU>=20 ", 1),
            ("server /:Status==Running ", 1),
        ] {
            picker.set_query(query);
            wait(&mut picker);
            assert_eq!(
                picker.displayed.as_ref().unwrap().output.matched,
                expected,
                "{query}"
            );
        }
        for (query, expected) in [("/!Ascending==CPU ", "10"), ("/!Descending==CPU ", "30")] {
            picker.set_query(query);
            wait(&mut picker);
            assert_eq!(picker.selection().unwrap().item.fields["CPU"], expected);
        }
        picker.set_query("/#Display==PID,CPU /!Descending==CPU ");
        wait(&mut picker);
        // Unknown display columns are ignored by the shared parser.
        let displayed = picker.displayed.as_ref().unwrap();
        assert!(displayed.snapshot.header(&picker.query).contains("CPU"));
        assert!(!displayed.snapshot.header(&picker.query).contains("Status"));
    }
    #[test]
    fn popup_navigation_enter_tab_and_escape_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![egui::Event::Text("/".into())]);
        assert_eq!(picker.suggestions.len(), 3);
        assert!(frame.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains("Display columns"))));
        frame.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.suggestion_selected, 1);
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "/!");
        assert!(!picker.is_closed());
        draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::Tab)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/!Descending==");
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert!(picker.suggestions.is_empty());
        assert!(!picker.is_closed());
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(event, Some(PickerEvent::Cancelled)));
    }
    fn commands() -> Vec<QueryCommand> {
        vec![
            QueryCommand {
                name: "GrepClaudeSessions".into(),
                description: "Search Claude transcripts".into(),
                argument: Some(crate::CommandArgument {
                    name: "search text".into(),
                    description: "Literal text".into(),
                    required: true,
                }),
            },
            QueryCommand {
                name: "Reload".into(),
                description: "Reload the list".into(),
                argument: None,
            },
        ]
    }
    fn shows_text(frame: &egui::FullOutput, needle: &str) -> bool {
        frame.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains(needle))
        })
    }
    fn shows_exact(frame: &egui::FullOutput, needle: &str) -> bool {
        frame.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == needle)
        })
    }
    /// The prompt's color, when the frame draws `prompt` on its own.
    fn prompt_color(frame: &egui::FullOutput, prompt: char) -> Option<egui::Color32> {
        frame.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == prompt.to_string() => {
                Some(text.galley.job.sections[0].format.color)
            }
            _ => None,
        })
    }
    /// Ctrl+; pressed and released; a toggle ignores a press egui reports
    /// as a repeat because it never saw the last one released.
    fn toggle() -> Vec<egui::Event> {
        let press = modified_key(Key::Semicolon, Modifiers::CTRL);
        let mut release = press.clone();
        if let egui::Event::Key { pressed, .. } = &mut release {
            *pressed = false;
        }
        vec![press, release]
    }
    #[test]
    fn command_mode_completes_describes_and_runs_without_searching() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        picker.set_query("beta");
        wait(&mut picker);
        let revision = picker.revision;
        let palette = Appearance::default().palette;
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert_eq!(prompt_color(&frame, '>'), Some(palette.accent));
        frame.drop_without_applying_deltas();

        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert_eq!(prompt_color(&frame, ':'), Some(palette.success));
        assert_eq!(prompt_color(&frame, '>'), None);
        assert!(picker.suggestions.is_empty(), "no popup");
        assert_eq!(picker.listed_commands().len(), 2, "every command, before typing");
        assert!(shows_text(&frame, "GrepClaudeSessions <search text>"));
        assert!(shows_text(&frame, "Reload the list"));
        assert!(!shows_text(&frame, "beta"), "the commands replace the results");
        assert!(!picker.busy());
        assert!(picker.current_results());
        frame.drop_without_applying_deltas();

        draw(&ctx, &mut picker, vec![egui::Event::Text("gr".into())])
            .0
            .drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.revision, revision, "command mode does not search");
        assert_eq!(picker.displayed.as_ref().unwrap().output.matched, 1);
        assert_eq!(picker.listed_commands().len(), 1);
        assert!(shows_text(&frame, "Search Claude transcripts"));
        assert!(!shows_text(&frame, "Reload the list"));
        frame.drop_without_applying_deltas();

        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "GrepClaudeSessions ");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "<search text>"), "placeholder");
        assert!(
            shows_text(&frame, "<search text> Literal text"),
            "the named command's row describes its argument"
        );
        frame.drop_without_applying_deltas();

        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.command_error.as_deref(), Some("search text is required"));
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        let error = frame.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == "search text is required" => {
                Some(text.galley.job.sections[0].format.color)
            }
            _ => None,
        });
        assert_eq!(error, Some(palette.error), "the reason shows as ghost text");
        assert!(!shows_exact(&frame, "<search text>"));
        frame.drop_without_applying_deltas();

        // Text typed in the same frame as Enter is part of the argument.
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("fn \"main\"".into()), key(Key::Enter)],
        );
        frame.drop_without_applying_deltas();
        assert!(matches!(
            event,
            Some(PickerEvent::Command { name, argument, alternate: false })
                if name == "GrepClaudeSessions" && argument == "fn \"main\""
        ));
        assert!(!picker.is_closed());
        assert_eq!(picker.revision, revision);

        // Each mode keeps its text for the next switch.
        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert!(!picker.command_mode());
        assert_eq!(picker.query(), "beta");
        assert_eq!(picker.revision, revision, "the search results are still current");
        assert!(picker.selection().is_some());
        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert_eq!(picker.query(), "GrepClaudeSessions fn \"main\"");

        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none(), "Escape leaves command mode first");
        assert!(!picker.command_mode());
        assert_eq!(picker.query(), "beta");
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(event, Some(PickerEvent::Cancelled)));
    }
    #[test]
    fn typing_gt_space_into_an_empty_search_enters_command_mode() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        let revision = picker.revision;
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![egui::Event::Text(">".into())])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), ">");
        assert_eq!(picker.revision, revision, "a lone > may start the shortcut");
        draw(&ctx, &mut picker, vec![egui::Event::Text(" ".into())])
            .0
            .drop_without_applying_deltas();
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "");
        draw(&ctx, &mut picker, vec![egui::Event::Text("Rel".into())])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "Rel", "the cursor moved to the new text");
        draw(&ctx, &mut picker, vec![key(Key::Escape)])
            .0
            .drop_without_applying_deltas();
        assert!(!picker.command_mode(), "no popup to close first");
        assert_eq!(picker.query(), "");
        assert_eq!(picker.revision, revision);

        // Pasted into an empty search, the rest is the command line.
        picker.set_query("> Reload");
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "Reload");
        picker.set_command_mode(false);

        // `: ` does the same, typed or pasted.
        picker.set_query(":");
        assert_eq!(picker.revision, revision, "a lone : may start the shortcut");
        picker.set_query(": ");
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "");
        picker.set_command_mode(false);
        picker.set_query(": Reload");
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "Reload");
        picker.set_command_mode(false);

        // Elsewhere, `>` and `:` are search text.
        picker.set_query(">x");
        assert_ne!(picker.revision, revision);
        picker.set_query("beta");
        picker.set_query("> beta");
        assert!(!picker.command_mode());
        picker.set_query("x");
        picker.set_query("x: ");
        assert!(!picker.command_mode());
    }
    #[test]
    fn accepting_a_command_row_starts_its_command_as_completing_it_would() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        picker.set_command_rows(Some(Arc::new(|selection| {
            match selection.item.fields["Name"].as_str() {
                "beta" => Some("GrepClaudeSessions".into()),
                "猫 server" => Some("Reload".into()),
                _ => None,
            }
        })));
        let accept = |picker: &mut Picker<StructuredStreamingSnapshot>, query: &str| {
            picker.set_query(query);
            wait(picker);
            draw(&ctx, picker, vec![]).0.drop_without_applying_deltas();
            let (frame, event) = draw(&ctx, picker, vec![key(Key::Enter)]);
            frame.drop_without_applying_deltas();
            event
        };

        assert!(accept(&mut picker, "beta").is_none());
        assert!(!picker.is_closed());
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "GrepClaudeSessions ");
        assert!(!picker.busy(), "the search before the command line is done");
        assert_eq!(picker.query_cursor, picker.query().len());
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_text(&frame, "<search text> Literal text"));
        frame.drop_without_applying_deltas();
        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert_eq!(picker.query(), "beta", "the search is kept");

        let event = accept(&mut picker, "server");
        assert!(matches!(
            event,
            Some(PickerEvent::Command { name, argument, alternate: false }) if name == "Reload" && argument.is_empty()
        ));
        assert!(!picker.command_mode());

        assert!(matches!(
            accept(&mut picker, "alpha"),
            Some(PickerEvent::Accepted(selection)) if selection.item.fields["Name"] == "alpha one"
        ));
    }
    #[test]
    fn row_icons_move_the_column_header_with_its_columns() {
        // Where the header and the first row's text start.
        let starts = |icon: bool| {
            let (ctx, mut picker) = make();
            if icon {
                picker.set_row_icon(Some(Arc::new(
                    |item: &nfm_picker_sources::structured::StructuredPickerItem| {
                        (item.fields["Name"] == "beta").then(|| "glyph::@#010203".to_string())
                    },
                )));
            }
            wait(&mut picker);
            draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
            let (frame, _) = draw(&ctx, &mut picker, vec![]);
            let start = |prefix: &str| {
                frame
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.job.text.starts_with(prefix) => {
                            Some(text.pos.x)
                        }
                        _ => None,
                    })
                    .fold(f32::INFINITY, f32::min)
            };
            let starts = (start("Name"), start("alpha"));
            frame.drop_without_applying_deltas();
            starts
        };
        let (plain, iconed) = (starts(false), starts(true));
        assert!(plain.0.is_finite() && plain.1.is_finite(), "{plain:?}");
        let header_shift = iconed.0 - plain.0;
        assert!(header_shift > 10.0, "{plain:?} {iconed:?}");
        assert!((header_shift - (iconed.1 - plain.1)).abs() < 0.5, "{plain:?} {iconed:?}");
    }
    #[test]
    fn enter_on_a_listed_command_without_an_argument_runs_it() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.set_command_mode(true);
        picker.set_query("rel");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        assert_eq!(picker.listed_commands()[0].name, "Reload");
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(
            event,
            Some(PickerEvent::Command { name, argument, alternate: false }) if name == "Reload" && argument.is_empty()
        ));
        assert_eq!(picker.query(), "Reload");
    }
    #[test]
    fn ctrl_enter_runs_a_command_line_as_its_alternate() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.set_command_mode(true);
        picker.set_query("GrepClaudeSessions main");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Enter, Modifiers::CTRL)],
        );
        frame.drop_without_applying_deltas();
        assert!(matches!(
            event,
            Some(PickerEvent::Command { name, argument, alternate: true })
                if name == "GrepClaudeSessions" && argument == "main"
        ));
        // As Enter does, it types a listed command's name first.
        picker.set_query("rel");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Enter, Modifiers::CTRL)],
        );
        frame.drop_without_applying_deltas();
        assert!(matches!(
            event,
            Some(PickerEvent::Command { name, alternate: true, .. }) if name == "Reload"
        ));

        // A host's reason for not running the line shows until it changes.
        picker.set_command_error(&ctx, "no result");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_text(&frame, "no result"));
        frame.drop_without_applying_deltas();
        picker.set_query("Reloa");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(!shows_text(&frame, "no result"));
        frame.drop_without_applying_deltas();
    }
    #[test]
    fn arrows_tab_and_clicks_pick_listed_commands() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        let selected = picker.selected;
        picker.set_command_mode(true);
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        // Away from the query, as results go.
        let away = if picker.config.layout == Layout::QueryBottom {
            Key::ArrowUp
        } else {
            Key::ArrowDown
        };
        draw(&ctx, &mut picker, vec![key(away)]).0.drop_without_applying_deltas();
        assert_eq!(picker.command_selected, 1);
        assert_eq!(picker.selected, selected, "the search keeps its selection");
        // Tab types a command's name without running it.
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Tab)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "Reload");

        // A click picks as Enter does.
        picker.set_query("");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        let row = frame
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text)
                    if text.galley.job.text.starts_with("GrepClaudeSessions") =>
                {
                    Some(text.pos + egui::vec2(4.0, 4.0))
                }
                _ => None,
            })
            .unwrap();
        frame.drop_without_applying_deltas();
        let click = |pressed| egui::Event::PointerButton {
            pos: row,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        draw(&ctx, &mut picker, vec![egui::Event::PointerMoved(row), click(true)])
            .0
            .drop_without_applying_deltas();
        let (frame, event) = draw(&ctx, &mut picker, vec![click(false)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "GrepClaudeSessions ");

        // An unknown name lists nothing and says so.
        picker.set_query("zz");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_text(&frame, "No command matches zz"));
        frame.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::Enter)]).0.drop_without_applying_deltas();
        assert_eq!(picker.command_error.as_deref(), Some("Unknown command zz"));
    }
    #[test]
    fn choices_list_under_the_command_and_enter_uses_or_ctrl_enter_copies_one() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.set_command_preview(Some(Arc::new(|_: &str, argument: &str| {
            Some(crate::CommandHint {
                text: String::new(),
                completion: None,
                choices: vec![
                    crate::CommandChoice {
                        text: format!("{argument} one"),
                        value: format!("{argument}1"),
                    },
                    crate::CommandChoice {
                        text: format!("{argument} two"),
                        value: format!("{argument}2"),
                    },
                ],
            })
        })));
        picker.set_command_mode(true);
        picker.set_query("GrepClaudeSessions a");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "a one") && shows_exact(&frame, "a two"));
        frame.drop_without_applying_deltas();
        // Away from the query, as results go.
        let away = if picker.config.layout == Layout::QueryBottom {
            Key::ArrowUp
        } else {
            Key::ArrowDown
        };
        draw(&ctx, &mut picker, vec![key(away)]).0.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(away)]).0.drop_without_applying_deltas();
        assert_eq!(picker.command_selected, 2);
        draw(&ctx, &mut picker, vec![key(away)]).0.drop_without_applying_deltas();
        assert_eq!(picker.command_selected, 2, "the last choice is the last row");

        // Ctrl+Enter copies the selected choice's value and reports nothing.
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Enter, Modifiers::CTRL)],
        );
        let copied: Vec<_> = frame
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(copied, ["a2"]);
        assert_eq!(picker.query(), "GrepClaudeSessions a");

        // Enter makes it the argument, which lists that argument's choices.
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "GrepClaudeSessions a2");
        assert_eq!(picker.command_selected, 0, "the command row again");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "a2 one"));
        frame.drop_without_applying_deltas();
        // Enter on the command row runs the line as before.
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(
            event,
            Some(PickerEvent::Command { argument, alternate: false, .. }) if argument == "a2"
        ));
    }
    #[test]
    fn right_arrow_at_the_end_completes_the_ghost_text() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.set_command_preview(Some(Arc::new(|_: &str, argument: &str| {
            Some(crate::CommandHint {
                text: format!("= {}", argument.len()),
                completion: Some(argument.len().to_string()),
                choices: Vec::new(),
            })
        })));
        picker.set_command_mode(true);
        let right = |picker: &mut Picker<StructuredStreamingSnapshot>| {
            draw(&ctx, picker, vec![key(Key::ArrowRight)]).0.drop_without_applying_deltas();
        };
        // The rest of the selected command's name, in its own case.
        picker.set_query("grep");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "ClaudeSessions"));
        frame.drop_without_applying_deltas();
        right(&mut picker);
        assert_eq!(picker.query(), "GrepClaudeSessions ");
        assert_eq!(picker.query_cursor, picker.query().len());

        // The host's completion of the argument.
        draw(&ctx, &mut picker, vec![egui::Event::Text("main".into())])
            .0
            .drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "  = 4"));
        frame.drop_without_applying_deltas();
        right(&mut picker);
        assert_eq!(picker.query(), "GrepClaudeSessions 4");
        assert!(!picker.is_closed());

        // Short of the end, Right moves the cursor.
        picker.set_query("rel");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::ArrowLeft)]).0.drop_without_applying_deltas();
        right(&mut picker);
        assert_eq!(picker.query(), "rel");
        right(&mut picker);
        assert_eq!(picker.query(), "Reload");
    }
    #[test]
    fn the_host_previews_a_command_line_once_per_line() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        picker.set_command_preview(Some(Arc::new(move |name: &str, argument: &str| {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (name == "GrepClaudeSessions" && argument != "nothing").then(|| crate::CommandHint {
                text: format!("= {}", argument.len()),
                completion: None,
                choices: Vec::new(),
            })
        })));
        picker.set_command_mode(true);
        let ghost = |picker: &mut Picker<StructuredStreamingSnapshot>, line: &str| {
            picker.set_query(line);
            draw(&ctx, picker, vec![]).0.drop_without_applying_deltas();
            let (frame, _) = draw(&ctx, picker, vec![]);
            let shown = shows_exact(&frame, "  = 4");
            frame.drop_without_applying_deltas();
            shown
        };
        assert!(ghost(&mut picker, "GrepClaudeSessions main"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1, "not every frame");
        assert!(!ghost(&mut picker, "GrepClaudeSessions nothing"));
        let calls_before = calls.load(std::sync::atomic::Ordering::Relaxed);
        // An empty argument shows its placeholder, without asking the host.
        picker.set_query("GrepClaudeSessions ");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_exact(&frame, "<search text>"));
        frame.drop_without_applying_deltas();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), calls_before);
        // The confirmation of a copy shows over the listed commands.
        picker.notify_copied_value(&ctx, "255");
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        assert!(shows_text(&frame, "Copied: 255"));
        frame.drop_without_applying_deltas();
    }
    /// `rows` numbered lines, as `row-007`.
    fn numbered(rows: usize) -> Arc<crate::styled_document::StyledDocument> {
        let text: Vec<String> = (0..rows).map(|row| format!("row-{row:03}")).collect();
        crate::styled_document::StyledDocument::from_text(&text.join("\n"))
    }
    /// A command output and a flag its closing sets.
    fn watched_output() -> (crate::CommandOutput, Arc<std::sync::atomic::AtomicBool>) {
        let output = crate::CommandOutput::new();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = closed.clone();
        output.on_close(move || flag.store(true, std::sync::atomic::Ordering::SeqCst));
        (output, closed)
    }
    fn shows_frame(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        events: Vec<egui::Event>,
        needles: &[&str],
    ) -> Vec<bool> {
        let (frame, _) = draw(ctx, picker, events);
        let shown = needles.iter().map(|needle| shows_text(&frame, needle)).collect();
        frame.drop_without_applying_deltas();
        shown
    }
    /// After a frame, the output's top row and rows in the preview pane.
    fn output_view(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
    ) -> Option<(usize, usize)> {
        draw(ctx, picker, vec![]).0.drop_without_applying_deltas();
        let viewport = picker.preview_viewport.as_ref()?;
        Some((viewport.top as usize, viewport.total_rows() as usize))
    }
    #[test]
    fn command_output_streams_into_the_preview_and_esc_returns_to_the_line() {
        use std::sync::atomic::Ordering;
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        assert!(!picker.preview_visible());
        picker.start_command(&ctx, "GrepClaudeSessions x");
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(event, Some(PickerEvent::Command { .. })));
        let (output, closed) = watched_output();
        picker.show_command_output(&ctx, output.clone());
        assert!(picker.preview_visible(), "the hidden pane opens");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        assert!(picker.busy(), "loading until finished");
        assert_eq!(output_view(&ctx, &mut picker), None, "nothing yet");

        // It follows the end as it grows.
        output.publish(numbered(100));
        let rows = picker.visible_preview_rows;
        assert!(rows > 1);
        assert_eq!(output_view(&ctx, &mut picker), Some((100 - rows, 100)));
        output.publish(numbered(150));
        assert_eq!(output_view(&ctx, &mut picker), Some((150 - rows, 150)));
        assert!(shows_frame(&ctx, &mut picker, vec![], &["GrepClaudeSessions x"])[0]);

        // Scrolled up, it stays put.
        let up = modified_key(Key::ArrowUp, Modifiers::CTRL);
        draw(&ctx, &mut picker, vec![up]).0.drop_without_applying_deltas();
        output.publish(numbered(200));
        assert_eq!(output_view(&ctx, &mut picker), Some((149 - rows, 200)));
        output.finish();
        assert!(!picker.busy());

        // Ctrl+C copies all of it while nothing on the line is selected.
        let (frame, _) = draw(&ctx, &mut picker, vec![modified_key(Key::C, Modifiers::CTRL)]);
        let copied = frame.platform_output.commands.iter().find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        });
        frame.drop_without_applying_deltas();
        let copied = copied.expect("Ctrl+C copies the output");
        assert!(copied.starts_with("row-000\nrow-001\n"), "{copied:?}");
        assert!(copied.trim_end().ends_with("row-199"));

        // Esc goes back to the line as it was typed and stops the command.
        draw(&ctx, &mut picker, vec![key(Key::Escape)]).0.drop_without_applying_deltas();
        assert!(!picker.command_output_shown());
        assert!(closed.load(Ordering::SeqCst));
        assert!(output.is_closed());
        assert!(picker.command_mode());
        assert_eq!(picker.query(), "GrepClaudeSessions x");
        assert!(!picker.preview_visible(), "the pane closes again");
        output.publish(numbered(10));
        assert!(!picker.command_output_shown(), "a closed output stays closed");
    }
    #[test]
    fn command_output_covers_the_command_list_down_to_the_query() {
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.start_command(&ctx, "GrepClaudeSessions x");
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let id = picker.id;
        let panel = || ctx.memory(|memory| memory.area_rect(id)).unwrap();
        let before = panel();
        let listed = shows_frame(&ctx, &mut picker, vec![], &["Search Claude transcripts"])[0];
        assert!(listed, "the command list shows before any output");

        let output = crate::CommandOutput::new();
        picker.show_command_output(&ctx, output.clone());
        output.publish(numbered(200));
        let listed = shows_frame(&ctx, &mut picker, vec![], &["Search Claude transcripts"])[0];
        assert!(!listed, "the output stands in for the command list");
        // The pane runs from the panel's top to the divider above the query.
        let (pane, query, shown) = (picker.preview_rect.unwrap(), picker.query_rect, panel());
        let top = shown.top() + PANEL_MARGIN.top as f32 + PANEL_BORDER;
        assert!((pane.top() - top).abs() < 1.0, "{pane:?} in {shown:?}");
        assert!(
            (query.top() - DIVIDER_HEIGHT - pane.bottom()).abs() < 1.0,
            "the pane reaches the divider above the query: {pane:?} {query:?}"
        );
        let covering = picker.visible_preview_rows;
        let configured = picker.config.preview_rows;
        assert!(covering > configured, "{covering} rows, from {configured}");
        assert_eq!(output_view(&ctx, &mut picker), Some((200 - covering, 200)));

        // Up and Down scroll the output rather than pick a hidden command.
        let selected = picker.command_selected;
        for _ in 0..2 {
            draw(&ctx, &mut picker, vec![key(Key::ArrowUp)])
                .0
                .drop_without_applying_deltas();
        }
        assert_eq!(output_view(&ctx, &mut picker), Some((198 - covering, 200)));
        draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(output_view(&ctx, &mut picker), Some((199 - covering, 200)));
        draw(&ctx, &mut picker, vec![key(Key::PageUp)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(output_view(&ctx, &mut picker), Some((199 - 2 * covering, 200)));
        assert_eq!(picker.command_selected, selected);

        // Esc brings the command list back.
        draw(&ctx, &mut picker, vec![key(Key::Escape)])
            .0
            .drop_without_applying_deltas();
        assert!(shows_frame(&ctx, &mut picker, vec![], &["Search Claude transcripts"])[0]);
        assert_eq!(picker.visible_preview_rows, 0, "the pane closes again");
        assert_eq!(panel(), before);
    }
    #[test]
    fn with_the_query_on_top_command_output_runs_to_the_panels_end() {
        let (ctx, mut picker) = make();
        picker.config.layout = Layout::QueryTop;
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.start_command(&ctx, "GrepClaudeSessions x");
        let output = crate::CommandOutput::new();
        picker.show_command_output(&ctx, output.clone());
        output.publish(numbered(200));
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![]).0.drop_without_applying_deltas();
        let shown = ctx.memory(|memory| memory.area_rect(picker.id)).unwrap();
        let (pane, query) = (picker.preview_rect.unwrap(), picker.query_rect);
        assert!((pane.top() - (query.bottom() + DIVIDER_HEIGHT)).abs() < 1.0);
        // Within the query's sizing allowance of the panel's inner edge.
        let end = shown.bottom() - PANEL_INSET_BOTTOM;
        assert!(
            pane.bottom() <= end + 0.5 && pane.bottom() >= end - QUERY_HEIGHT_ALLOWANCE - 0.5,
            "{pane:?} in {shown:?}"
        );
    }
    #[test]
    fn command_output_is_closed_when_replaced_left_or_dropped() {
        use std::sync::atomic::Ordering;
        let (ctx, mut picker) = make();
        picker.set_query_commands(commands());
        wait(&mut picker);
        picker.config.preview_visible = true;
        // Outside command mode there is nowhere to show it.
        let (output, closed) = watched_output();
        picker.show_command_output(&ctx, output);
        assert!(closed.load(Ordering::SeqCst));

        picker.start_command(&ctx, "Reload");
        let (first, first_closed) = watched_output();
        picker.show_command_output(&ctx, first);
        let (second, second_closed) = watched_output();
        picker.show_command_output(&ctx, second);
        assert!(first_closed.load(Ordering::SeqCst));
        assert!(!second_closed.load(Ordering::SeqCst));
        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert!(!picker.command_mode());
        assert!(second_closed.load(Ordering::SeqCst), "leaving command mode closes it");
        assert!(picker.preview_visible(), "a pane that was open stays open");

        picker.start_command(&ctx, "Reload");
        let (third, third_closed) = watched_output();
        picker.show_command_output(&ctx, third);
        drop(picker);
        assert!(third_closed.load(Ordering::SeqCst));
    }
    #[test]
    fn without_commands_there_is_no_command_mode() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        let revision = picker.revision;
        draw(&ctx, &mut picker, toggle()).0.drop_without_applying_deltas();
        assert!(!picker.command_mode());
        picker.set_command_mode(true);
        assert!(!picker.command_mode());
        picker.set_query("> x");
        assert!(!picker.command_mode());
        assert_ne!(picker.revision, revision);
        assert!(picker.completions(picker.query().len()).is_empty());
        // Removing the commands leaves command mode.
        picker.set_query("");
        picker.set_query_commands(commands());
        picker.set_command_mode(true);
        picker.set_query_commands(Vec::new());
        assert!(!picker.command_mode());
    }
    #[test]
    fn mouse_completion_and_six_row_popup_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![egui::Event::Text("/".into())])
            .0
            .drop_without_applying_deltas();
        let rect = ctx
            .memory(|memory| memory.area_rect(picker.id.with("autocomplete")))
            .unwrap();
        let pos = rect.min
            + egui::vec2(
                30.0,
                8.0 + 1.5 * Appearance::default().typography.row_height,
            );
        draw(&ctx, &mut picker, vec![egui::Event::PointerMoved(pos)])
            .0
            .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        )
        .0
        .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
        )
        .0
        .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/!");
        let (_, mut picker) = make();
        wait(&mut picker);
        let ctx = Context::default();
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![egui::Event::Text("/:Name=".into())])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.suggestions.len(), 8);
        let rect = ctx
            .memory(|memory| memory.area_rect(picker.id.with("autocomplete")))
            .unwrap();
        assert!(
            rect.height() <= 6.0 * Appearance::default().typography.preview_row_height(&ctx) + 20.0
        );
        for _ in 0..7 {
            draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
                .0
                .drop_without_applying_deltas();
        }
        assert_eq!(picker.suggestion_selected, 7);
        draw(&ctx, &mut picker, vec![key(Key::Enter)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/:Name<");
    }
    #[test]
    fn completing_in_middle_of_unicode_query_preserves_suffix_and_cursor() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("猫 /:Sta tail".into())],
        )
        .0
        .drop_without_applying_deltas();
        for _ in 0..5 {
            draw(&ctx, &mut picker, vec![key(Key::ArrowLeft)])
                .0
                .drop_without_applying_deltas();
        }
        assert_eq!(picker.suggestions[0].text, "Status");
        draw(&ctx, &mut picker, vec![key(Key::Tab)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "猫 /:Status= tail");
        assert_eq!(picker.query_cursor, "猫 /:Status=".len());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::time::{Duration, Instant};

    struct Versioned {
        version: u64,
        data: FlatSnapshot<u64>,
    }
    impl ItemsSource for Versioned {
        type Item = u64;
        fn version(&self) -> u64 {
            self.version
        }
        fn len(&self) -> usize {
            self.data.len()
        }
        fn is_empty(&self) -> bool {
            self.data.is_empty()
        }
        fn get_string<'a>(
            &'a self,
            index: usize,
            stack: &'a mut [u8],
            heap: &'a mut Vec<u8>,
        ) -> &'a [u8] {
            self.data.get_string(index, stack, heap)
        }
        fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
            self.data.get_string_lossy(index, out)
        }
        fn item(&self, index: usize) -> Option<u64> {
            self.data.item(index)
        }
    }
    fn snapshot(version: u64, text: &str, payload: u64) -> Arc<Versioned> {
        Arc::new(Versioned {
            version,
            data: FlatSnapshot::from_items([(text, payload)]),
        })
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn make() -> (Context, Arc<SnapshotStore<Versioned>>, Picker<Versioned>) {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot(1, "alpha", 42));
        store.complete();
        let picker = Picker::new("test", &ctx, store.clone(), PickerConfig::default());
        (ctx, store, picker)
    }
    #[test]
    fn skipped_source_records_reach_the_counter() {
        let (ctx, store, mut picker) = make();
        assert_eq!(picker.session.skipped(), 0);
        store.add_skipped(3);
        assert_eq!(picker.session.skipped(), 3);
        // The counter is painted with the note.
        let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            picker.show(
                ui.ctx(),
                crate::Placement::new(ui.ctx().content_rect()),
                &crate::Appearance::default(),
                false,
            );
        });
        let mut frame = frame;
        frame.textures_delta.clear();
        let painted = format!("{:?}", frame.shapes);
        assert!(painted.contains("3 skipped"));
    }
    #[test]
    fn results_retain_payloads_from_their_actual_snapshot() {
        let (_, store, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let old = Arc::downgrade(&picker.displayed.as_ref().unwrap().snapshot);
        store.publish(snapshot(2, "beta", 99));
        // Before the UI receives a new result, alpha still belongs to snapshot 1.
        assert_eq!(picker.selection().unwrap().item, 42);
        assert!(old.upgrade().is_some());
        wait(|| {
            picker.poll();
            picker.selection().is_some_and(|s| s.source_version == 2)
        });
        assert_eq!(picker.selection().unwrap().item, 99);
        picker.close();
    }
    #[test]
    fn edits_invalidate_selection_and_reject_stale_updates() {
        let (_, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let stale = picker.displayed.take().unwrap();
        picker.set_query("beta");
        assert!(picker.selection().is_none());
        picker.displayed = Some(stale);
        assert!(picker.selection().is_none());
        wait(|| {
            picker.poll();
            picker
                .displayed
                .as_ref()
                .is_some_and(|d| d.revision == picker.revision)
        });
        assert!(picker.selection().is_none());
    }
    fn draw(
        ctx: &Context,
        picker: &mut Picker<Versioned>,
        events: Vec<egui::Event>,
    ) -> PickerOutput<u64> {
        let mut result = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let ctx = ui.ctx();
                let output = picker.show(
                    ctx,
                    Placement::new(ctx.content_rect()),
                    &Appearance::default(),
                    false,
                );
                if result
                    .as_ref()
                    .is_none_or(|old: &PickerOutput<u64>| old.event.is_none())
                {
                    result = Some(output);
                }
            },
        );
        output.drop_without_applying_deltas();
        result.unwrap()
    }
    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    #[test]
    fn keyboard_accept_returns_payload_and_closes_once() {
        let (ctx, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        draw(&ctx, &mut picker, vec![]);
        let output = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(matches!(
            output.event,
            Some(PickerEvent::Accepted(Selection { item: 42, .. }))
        ));
        assert!(picker.is_closed());
        assert!(
            draw(&ctx, &mut picker, vec![key(Key::Enter)])
                .event
                .is_none()
        );
    }
    #[test]
    fn escape_wins_over_enter_and_query_edits_cannot_accept_old_results() {
        let (ctx, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        draw(&ctx, &mut picker, vec![]);
        let output = draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("zz".into()), key(Key::Enter)],
        );
        assert!(output.event.is_none());
        assert!(!picker.is_closed());
        assert!(picker.selection().is_none());
        assert!(matches!(
            draw(&ctx, &mut picker, vec![key(Key::Escape), key(Key::Enter)]).event,
            Some(PickerEvent::Cancelled)
        ));
    }
    /// A preview of `lines` numbered lines, as a streamed answer grows.
    fn growing_document_provider() -> Arc<crate::styled_document::DocumentPreviewProvider<u64>> {
        Arc::new(crate::styled_document::DocumentPreviewProvider::new(
            |selection: &Selection<u64>| {
                let text: Vec<String> = (1..=selection.item).map(|n| format!("line {n}")).collect();
                Ok((
                    crate::styled_document::StyledDocument::from_text(&text.join("\n")),
                    None,
                ))
            },
            egui::Color32::YELLOW,
        ))
    }
    /// The preview's first shown row and its rows in all, once the preview
    /// of `lines` lines has loaded.
    fn shown_rows(ctx: &Context, picker: &mut Picker<Versioned>, lines: usize) -> (usize, usize) {
        let mut shown = None;
        wait(|| {
            draw(ctx, picker, vec![]);
            shown = match picker.preview.as_ref().and_then(|p| p.document.as_ref()) {
                Some(Ok(Preview::Grid { total_rows, .. })) if *total_rows == lines => {
                    Some((picker.scroll_offset, *total_rows))
                }
                _ => None,
            };
            shown.is_some() && !picker.preview.as_ref().unwrap().loading
        });
        shown.unwrap()
    }
    #[test]
    fn a_growing_preview_follows_its_end_until_scrolled_away() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot(1, "answer", 3));
        let mut picker = Picker::new(
            "test",
            &ctx,
            store.clone(),
            PickerConfig {
                preview_rows: 10,
                ..PickerConfig::default()
            },
        );
        picker.set_preview_provider(&ctx, growing_document_provider());
        picker.set_preview_follows_end(true);
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert_eq!(shown_rows(&ctx, &mut picker, 3), (0, 3));
        // As the answer grows past the pane, the end stays in view.
        store.publish(snapshot(2, "answer", 50));
        assert_eq!(shown_rows(&ctx, &mut picker, 50), (40, 50));
        store.publish(snapshot(3, "answer", 80));
        assert_eq!(shown_rows(&ctx, &mut picker, 80), (70, 80));

        // Scrolled up to read, the place is kept as more arrives.
        let up = egui::Event::Key {
            key: Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        draw(&ctx, &mut picker, vec![up]);
        let (first, _) = shown_rows(&ctx, &mut picker, 80);
        assert_eq!(first, 69);
        store.publish(snapshot(4, "answer", 120));
        assert_eq!(shown_rows(&ctx, &mut picker, 120), (69, 120));

        // Back at the end, it follows again.
        let end = egui::Event::Key {
            key: Key::PageDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        for _ in 0..8 {
            draw(&ctx, &mut picker, vec![end.clone()]);
        }
        // Copy-mode scrolling stops a row short of the last page.
        assert_eq!(shown_rows(&ctx, &mut picker, 120).0, 109);
        store.publish(snapshot(5, "answer", 150));
        assert_eq!(shown_rows(&ctx, &mut picker, 150), (140, 150));

        // Copy mode keeps its document while more arrives, and the preview
        // catches up once it is left.
        picker.execute(&ctx, PickerAction::TogglePreviewFocus);
        draw(&ctx, &mut picker, vec![]);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.preview_focused());
        store.publish(snapshot(6, "answer", 170));
        wait(|| {
            draw(&ctx, &mut picker, vec![]);
            picker.selection().is_some_and(|s| s.source_version == 6)
        });
        assert!(picker.preview_focused(), "a new version does not leave copy mode");
        picker.execute(&ctx, PickerAction::TogglePreviewFocus);
        draw(&ctx, &mut picker, vec![]);
        assert!(!picker.preview_focused());
        assert_eq!(shown_rows(&ctx, &mut picker, 170).1, 170);
        store.complete();
    }
    #[test]
    fn without_following_a_new_version_starts_at_the_top() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot(1, "answer", 50));
        let mut picker = Picker::new(
            "test",
            &ctx,
            store.clone(),
            PickerConfig {
                preview_rows: 10,
                ..PickerConfig::default()
            },
        );
        picker.set_preview_provider(&ctx, growing_document_provider());
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert_eq!(shown_rows(&ctx, &mut picker, 50), (0, 50));
        store.publish(snapshot(2, "answer", 80));
        assert_eq!(shown_rows(&ctx, &mut picker, 80), (0, 80));
        store.complete();
    }
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    use nfm_picker_sources::structured::{StructuredSchema, StructuredStreamingStore};
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::time::{Duration, Instant};
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn structured_sources_supply_headers_completions_and_owned_payloads() {
        let mut data = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into()]).unwrap(),
        );
        data.add_record(&csv::StringRecord::from(vec!["alpha", "Running"]))
            .unwrap();
        let store = Arc::new(SnapshotStore::new());
        store.publish(data.snapshot());
        store.complete();
        let mut picker = Picker::new(
            "structured",
            &Context::default(),
            store,
            PickerConfig::default(),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        picker.set_query("/:Status==Run");
        let completion = picker.completions(picker.query().len()).remove(0);
        assert!(picker.apply_completion(&completion));
        assert_eq!(picker.query(), "/:Status==Running ");
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert_eq!(picker.selection().unwrap().item.fields["Name"], "alpha");
        let invalid = nfm_search_core::store::SearchCompletion {
            text: String::new(),
            positions: vec![],
            detail: String::new(),
            replacement: "bad".into(),
            replace: 1..usize::MAX,
        };
        assert!(!picker.apply_completion(&invalid));
    }
    #[test]
    fn preview_owns_searched_snapshot_after_picker_and_store_drop() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("alpha", 42u64)])));
        store.complete();
        let (started, rx) = crossbeam_channel::bounded(1);
        let (release, gate) = crossbeam_channel::bounded(1);
        let mut picker = Picker::new("owned", &ctx, store.clone(), PickerConfig::default());
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                move |request: PreviewRequest<u64>, cancel: crate::Cancellation| {
                    let snapshot = request
                        .selection
                        .source_snapshot::<FlatSnapshot<u64>>()
                        .unwrap();
                    started
                        .send((Arc::downgrade(&snapshot), cancel.clone()))
                        .unwrap();
                    gate.recv().unwrap();
                    assert_eq!(snapshot.item(request.selection.index), Some(42));
                    Ok(Preview::Empty)
                },
            ),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        picker.refresh_preview(400.0, &Appearance::default());
        let (snapshot, token) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(store);
        drop(picker);
        assert!(token.is_cancelled());
        assert!(snapshot.upgrade().is_some());
        release.send(()).unwrap();
        wait(|| snapshot.upgrade().is_none());
    }
    #[test]
    fn small_viewports_reduce_rows_and_hide_preview_before_query() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("alpha", 42u64)])));
        store.complete();
        let mut picker = Picker::new("resize", &ctx, store, PickerConfig::default());
        picker.set_preview_provider(
            &ctx,
            Arc::new(|_: PreviewRequest<u64>, _: crate::Cancellation| Ok(Preview::Empty)),
        );
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            },
            |ui| {
                let response = picker.show(
                    ui.ctx(),
                    Placement::new(bounds),
                    &Appearance::default(),
                    false,
                );
                assert!(bounds.contains_rect(response.rect));
            },
        );
        output.drop_without_applying_deltas();
        assert_eq!(picker.visible_preview_rows, 0);
        assert!(picker.visible_result_rows < 7);
    }

    #[test]
    fn full_pane_reserves_result_rows_and_fills_preview_on_resize_and_toggle() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("alpha", 42u64)])));
        store.complete();
        let mut picker = Picker::new(
            "full-pane",
            &ctx,
            store,
            PickerConfig {
                result_rows: 5,
                preview_rows: 1,
                ..Default::default()
            },
        );
        picker.set_external_preview(true);
        let appearance = Appearance::default();
        let mut previous_rows = 0;
        for (height, visible) in [
            (600.0, true),
            (900.0, true),
            (900.0, false),
            (600.0, true),
            (150.0, true),
        ] {
            picker.config.preview_visible = visible;
            let bounds =
                egui::Rect::from_min_size(egui::pos2(30.0, 40.0), egui::vec2(800.0, height));
            let mut placement = Placement::new(bounds);
            placement.position = crate::Position::FullPane;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 1100.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    let result = picker.show(ui.ctx(), placement, &appearance, false);
                    assert_eq!(result.rect, bounds);
                    assert!(
                        (picker.query_rect.bottom() - (bounds.bottom() - PANEL_INSET_BOTTOM)).abs()
                            < 1.0,
                        "query must stay at the bottom with preview visible or hidden: {:?}",
                        picker.query_rect
                    );
                    if height >= 600.0 {
                        if visible {
                            assert_eq!(picker.visible_result_rows, 5);
                            let row_height = appearance.typography.preview_row_height(ui.ctx());
                            let expected = ((height - 6.0 * row_height - PREVIEW_OVERHEAD)
                                / row_height)
                                .floor() as usize;
                            assert_eq!(picker.visible_preview_rows, expected);
                            if height == 900.0 {
                                assert!(picker.visible_preview_rows > previous_rows);
                            }
                            let preview = result.preview_rect.unwrap();
                            assert!(bounds.contains_rect(preview));
                            assert!((preview.height() - expected as f32 * row_height).abs() < 1.0);
                        } else {
                            let row_height = appearance.typography.preview_row_height(ui.ctx());
                            let expected =
                                ((height - PANEL_FRAME_HEIGHT - DIVIDER_HEIGHT) / row_height - 1.0)
                                    .floor() as usize;
                            assert_eq!(picker.visible_result_rows, expected);
                            assert!(picker.visible_result_rows > 5);
                            assert_eq!(picker.visible_preview_rows, 0);
                            assert!(result.preview_rect.is_none());
                        }
                    } else {
                        assert!(picker.visible_result_rows < 5);
                        assert_eq!(picker.visible_preview_rows, 0);
                    }
                },
            );
            output.drop_without_applying_deltas();
            if visible && height >= 600.0 {
                previous_rows = picker.visible_preview_rows;
            }
        }
    }
}

#[cfg(test)]
mod streaming_presentation_tests {
    use super::*;
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted {
        version: u64,
        data: FlatSnapshot<u64>,
        reads: Arc<AtomicUsize>,
    }
    impl ItemsSource for Counted {
        type Item = u64;
        fn version(&self) -> u64 {
            self.version
        }
        fn len(&self) -> usize {
            self.data.len()
        }
        fn is_empty(&self) -> bool {
            self.data.is_empty()
        }
        fn get_string<'a>(
            &'a self,
            index: usize,
            stack: &'a mut [u8],
            heap: &'a mut Vec<u8>,
        ) -> &'a [u8] {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.data.get_string(index, stack, heap)
        }
        fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.data.get_string_lossy(index, out)
        }
        fn item(&self, index: usize) -> Option<u64> {
            self.data.item(index)
        }
    }
    fn snapshot(version: u64, len: u64, reads: &Arc<AtomicUsize>) -> Arc<Counted> {
        Arc::new(Counted {
            version,
            data: FlatSnapshot::from_items((0..len).map(|i| (format!("item {i:03}"), i))),
            reads: reads.clone(),
        })
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn draw(ctx: &Context, picker: &mut Picker<Counted>) -> (egui::Rect, egui::Rect) {
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
        let frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            },
            |ui| {
                picker.show(
                    ui.ctx(),
                    Placement::new(bounds),
                    &Appearance::default(),
                    false,
                );
            },
        );
        frame.drop_without_applying_deltas();
        (
            ctx.memory(|m| m.area_rect(picker.id)).unwrap(),
            ctx.read_response(picker.id.with("query")).unwrap().rect,
        )
    }
    #[test]
    fn streams_before_completion_searches_only_delta_and_keeps_selection_preview() {
        let ctx = Context::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(SnapshotStore::new_append_only());
        store.publish(snapshot(1, 100, &reads));
        let mut picker = Picker::new(
            "stream",
            &ctx,
            store.clone(),
            PickerConfig {
                initial_query: "item".into(),
                ..Default::default()
            },
        );
        let previews = Arc::new(AtomicUsize::new(0));
        let calls = previews.clone();
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                move |request: PreviewRequest<u64>, _: crate::Cancellation| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Preview::Text(request.selection.text))
                },
            ),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert!(!picker.displayed.as_ref().unwrap().done);
        picker.move_selection(50);
        picker.refresh_preview(600.0, &Appearance::default());
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        let geometry = draw(&ctx, &mut picker);
        // Draw may resize the initial preview request to the actual viewport.
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        let initial_calls = previews.load(Ordering::SeqCst);
        reads.store(0, Ordering::SeqCst);
        store.publish(snapshot(2, 101, &reads));
        wait(|| {
            picker.poll();
            picker
                .displayed
                .as_ref()
                .is_some_and(|d| d.snapshot.version() == 2)
        });
        assert_eq!(picker.selection().unwrap().item, 50);
        assert_eq!(picker.displayed.as_ref().unwrap().output.matched, 101);
        assert!(
            reads.load(Ordering::SeqCst) < 10,
            "stream append rescanned previous items"
        );
        let next_geometry = draw(&ctx, &mut picker);
        assert_eq!(geometry.0, next_geometry.0);
        assert_eq!(geometry.1.min.y, next_geometry.1.min.y);
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        assert_eq!(
            previews.load(Ordering::SeqCst),
            initial_calls,
            "unrelated appended candidates restarted the selected preview"
        );
        store.complete();
        wait(|| {
            picker.poll();
            picker.displayed.as_ref().is_some_and(|d| d.done)
        });
        assert_eq!(picker.selection().unwrap().item, 50);
    }
    #[test]
    fn pending_query_retains_rows_and_geometry_but_blocks_acceptance() {
        let ctx = Context::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(SnapshotStore::new_append_only());
        store.publish(snapshot(1, 3, &reads));
        store.complete();
        let mut picker = Picker::new("pending", &ctx, store, PickerConfig::default());
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let before = draw(&ctx, &mut picker);
        picker.set_query("does-not-match");
        assert_eq!(picker.displayed.as_ref().unwrap().output.results.len(), 3);
        assert!(picker.selection().is_none());
        assert!(picker.busy());
        wait(|| {
            picker.poll();
            picker.current_results()
        });
        assert!(picker.displayed.as_ref().unwrap().output.results.is_empty());
        let after = draw(&ctx, &mut picker);
        assert_eq!(before.0, after.0);
        assert_eq!(before.1.min.y, after.1.min.y);
    }
}

#[cfg(test)]
mod output_selection_tests {
    use super::*;

    fn selection(item: usize) -> Selection<usize> {
        Selection {
            item,
            text: item.to_string(),
            index: item,
            source_version: item as u64,
            snapshot: Arc::new(item),
        }
    }
    fn output(event: Option<PickerEvent<usize>>) -> PickerOutput<usize> {
        PickerOutput {
            event,
            frame_selection: Some(selection(1)),
            preview_rendered: false,
            preview_selection: None,
            preview_rect: None,
            wants_keyboard: false,
            busy: false,
            matched: 0,
            total: 0,
            rect: egui::Rect::NOTHING,
        }
    }
    #[test]
    fn selection_prefers_event_snapshot_and_borrows_without_cloning() {
        for event in [
            PickerEvent::Accepted(selection(2)),
            PickerEvent::CustomAction {
                action: 42,
                selection: selection(2),
            },
            PickerEvent::CopyRequested(selection(2)),
        ] {
            let output = output(Some(event));
            let selected = output.selection().unwrap();
            let event_selection = match output.event.as_ref().unwrap() {
                PickerEvent::Accepted(s)
                | PickerEvent::CustomAction { selection: s, .. }
                | PickerEvent::CopyRequested(s) => s,
                _ => unreachable!(),
            };
            assert!(std::ptr::eq(selected, event_selection));
            assert_eq!(*selected.source_snapshot::<usize>().unwrap(), 2);
        }
    }
    #[test]
    fn selection_uses_frame_snapshot_without_a_selection_event() {
        for event in [None, Some(PickerEvent::Cancelled)] {
            let mut output = output(event);
            assert_eq!(output.selection().unwrap().item, 1);
            output.frame_selection = None;
            assert!(output.selection().is_none());
        }
    }
}

#[cfg(test)]
mod long_result_tests {
    use super::*;
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};

    /// One point per character; wide CJK characters take two.
    fn cells(c: char, _matched: bool) -> f32 {
        if c == '猫' { 2.0 } else { 1.0 }
    }

    fn window(start: usize, end: usize, leading: bool, trailing: bool) -> ResultWindow {
        ResultWindow {
            start,
            end,
            leading,
            trailing,
        }
    }

    #[test]
    fn short_text_and_matches_near_the_start_keep_the_start() {
        assert_eq!(
            result_window("abc", &[], 3.0, cells),
            window(0, 3, false, false)
        );
        // One cell is reserved for the trailing ellipsis.
        assert_eq!(
            result_window("abcdefghij", &[], 5.0, cells),
            window(0, 4, false, true)
        );
        assert_eq!(
            result_window("abcdefghij", &[1, 3], 5.0, cells),
            window(0, 4, false, true)
        );
    }

    #[test]
    fn matches_past_the_edge_shift_the_text_and_keep_some_context() {
        let text = format!("{}X{}", "a".repeat(20), "b".repeat(20));
        let shifted = result_window(&text, &[20], 12.0, cells);
        // Context is at most a quarter of the row: three cells here.
        assert_eq!(shifted, window(14, 24, true, true));
        assert_eq!(&text[shifted.start..shifted.end], "aaaaaaXbbb");

        // A match at the very end needs no trailing ellipsis.
        let text = format!("{}X", "a".repeat(20));
        assert_eq!(
            result_window(&text, &[20], 12.0, cells),
            window(10, 21, true, false)
        );

        // Wide characters are measured in their own width.
        let text = format!("{}X", "猫".repeat(10));
        let wide = result_window(&text, &[30], 9.0, cells);
        assert_eq!(wide, window(21, 31, true, false));
        assert_eq!(&text[wide.start..wide.end], "猫猫猫X");
    }

    #[test]
    fn positions_outside_the_text_fall_back_to_the_start() {
        assert_eq!(
            result_window("abcdefghij", &[99], 5.0, cells),
            window(0, 4, false, true)
        );
    }

    fn sections(job: &egui::text::LayoutJob) -> Vec<(&str, bool)> {
        let appearance = Appearance::default();
        job.sections
            .iter()
            .map(|section| {
                (
                    &job.text[section.byte_range.start.0..section.byte_range.end.0],
                    section.format.color == appearance.palette.match_highlight,
                )
            })
            .collect()
    }

    #[test]
    fn layout_groups_matched_runs_and_marks_hidden_text() {
        let appearance = Appearance::default();
        let whole = highlighted_layout("abcdef", &[2, 1, 2], false, &appearance);
        assert_eq!(
            sections(&whole),
            [("a", false), ("bc", true), ("def", false)]
        );
        let partial = result_window_layout(
            "abcdef",
            window(1, 4, true, true),
            &[1, 2],
            false,
            &appearance,
        );
        assert_eq!(
            sections(&partial),
            [("…", false), ("bc", true), ("d", false), ("…", false)]
        );
        assert_eq!(partial.sections[0].format.color, appearance.palette.muted);
    }

    #[test]
    fn control_characters_are_painted_on_one_line() {
        let job = highlighted_layout("a\tb\u{7}\nc", &[], false, &Appearance::default());
        assert_eq!(job.text, "a b\u{FFFD}\u{FFFD}c");
    }

    #[test]
    fn long_results_show_their_match_between_ellipses() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        let long = format!("{}needle{}", "x".repeat(400), "y".repeat(50));
        store.publish(Arc::new(FlatSnapshot::from_items([(long.as_str(), 1u64)])));
        store.complete();
        let mut picker = Picker::new(
            "long-results",
            &ctx,
            store,
            PickerConfig {
                preview_visible: false,
                ..Default::default()
            },
        );
        picker.set_query("needle");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(picker.current_results() && picker.selection().is_some()) {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let appearance = Appearance::default();
        let width = 600.0;
        let mut paint = || {
            let frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 400.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &appearance,
                        false,
                    );
                },
            );
            let row = frame
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text().contains("needle") => {
                        Some((text.galley.text().to_owned(), text.visual_bounding_rect()))
                    }
                    _ => None,
                })
                .expect("painted result row");
            frame.drop_without_applying_deltas();
            row
        };
        let (text, rect) = paint();
        assert!(text.starts_with('…'), "{text}");
        assert!(text.ends_with('…'), "{text}");
        assert!(text.len() < long.len());
        assert!(rect.right() <= width, "row overflows: {rect:?}");
        // Later frames rebuild the row and paint the same text.
        assert_eq!(paint().0, text);
    }
}

#[cfg(test)]
mod card_tests {
    use super::*;
    use nfm_picker_sources::structured::{
        StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore,
    };
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};

    fn make(rows: &[[&str; 4]]) -> (Context, Picker<StructuredStreamingSnapshot>) {
        let ctx = Context::default();
        // `updated` holds seconds ago; the host words them.
        let schema = StructuredSchema::new(
            ["project", "updated", "title", "status"]
                .map(String::from)
                .to_vec(),
        )
        .unwrap()
        .with_formatter("updated", |raw| match raw.parse::<u64>() {
            Ok(seconds) if seconds < 3600 => format!("{} minutes ago", seconds / 60),
            Ok(seconds) => format!("{} hours ago", seconds / 3600),
            Err(_) => raw.into(),
        })
        .unwrap();
        let template = schema
            .row_template("project:muted|updated:right;title:title", None)
            .unwrap();
        let mut store = StructuredStreamingStore::new(schema)
            .with_row_template(template)
            .unwrap();
        for row in rows {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let source = Arc::new(SnapshotStore::new());
        source.publish(store.snapshot());
        source.complete();
        let picker = Picker::new(
            "cards",
            &ctx,
            source,
            PickerConfig {
                preview_visible: false,
                result_rows: 3,
                ..Default::default()
            },
        );
        (ctx, picker)
    }

    fn texts(ctx: &Context, picker: &mut Picker<StructuredStreamingSnapshot>) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
            },
        );
        frame.textures_delta.clear();
        frame
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_window_sized_by_results_height_shows_that_many_cards() {
        let rows: Vec<[&str; 4]> = (0..10).map(|_| ["p", "60", "title", "s"]).collect();
        let (ctx, mut picker) = make(&rows);
        // The window, not result_rows, limits the cards.
        picker.config.result_rows = 6;
        texts(&ctx, &mut picker);
        let appearance = Appearance::default();
        let template = picker
            .displayed
            .as_ref()
            .and_then(|displayed| displayed.snapshot.row_template().cloned())
            .unwrap();
        for rows in [1, 4, 6] {
            let placement = Placement::new(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    600.0,
                    results_height(&ctx, &appearance, Some(&template), rows) + 20.0,
                ),
            ));
            let frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(placement.bounds),
                    ..Default::default()
                },
                |ui| {
                    picker.show(ui.ctx(), placement, &appearance, false);
                },
            );
            frame.drop_without_applying_deltas();
            assert_eq!(picker.visible_result_rows, rows);
        }
    }

    #[test]
    fn cards_paint_fields_host_formatted_times_and_size_the_panel_by_card() {
        let nine = (9 * 60).to_string();
        let three = (3 * 3600 + 60).to_string();
        let (ctx, mut picker) = make(&[
            ["src/t3code", nine.as_str(), "/resume", "running"],
            [
                "src/nterm",
                three.as_str(),
                "what worktrees do we have",
                "done",
            ],
        ]);
        let painted = texts(&ctx, &mut picker);
        for expected in [
            "src/t3code",
            "9 minutes ago",
            "/resume",
            "src/nterm",
            "3 hours ago",
        ] {
            assert!(
                painted.iter().any(|text| text == expected),
                "{expected:?} not painted in {painted:?}"
            );
        }
        // Cards have no column header and do not paint the CSV display text.
        assert!(!painted.iter().any(|text| text.contains("project")));
        assert!(!painted.iter().any(|text| text.contains(',')));
        let line = Appearance::default().typography.preview_row_height(&ctx);
        let card = picker.result_row_height(&ctx, &Appearance::default());
        assert!(card > 2.0 * line, "card {card} vs line {line}");
        assert_eq!(picker.visible_result_rows, 3);
    }

    #[test]
    fn matches_across_fields_are_highlighted_in_each_field() {
        let template = RowTemplate::parse("project|updated:right:nohl;title", None, |name| {
            ["project", "updated", "title"]
                .iter()
                .position(|n| *n == name)
        })
        .unwrap();
        let fields = RowFields {
            values: vec![
                "src/t3code".into(),
                "9 minutes ago".into(),
                "/resume".into(),
            ],
        };
        let positions = card_match_positions(&template, &fields, "t3 resume");
        assert_eq!(positions.get(&0), Some(&vec![4, 5]));
        assert_eq!(positions.get(&2), Some(&vec![1, 2, 3, 4, 5, 6]));
        // `nohl` slots are not highlighted.
        assert!(!positions.contains_key(&1));
        assert!(card_match_positions(&template, &fields, "").is_empty());
    }

    #[test]
    fn values_a_formatter_leaves_alone_are_shown_as_they_are() {
        let (ctx, mut picker) = make(&[["p", "sometime", "t", ""]]);
        let painted = texts(&ctx, &mut picker);
        assert!(painted.iter().any(|text| text == "sometime"));
    }

    /// A picker over `columns` with a card `template` and optional leading
    /// icon slot.
    fn make_cards(
        columns: &[&str],
        template: &str,
        leading: Option<&str>,
        rows: &[&[&str]],
    ) -> (Context, Picker<StructuredStreamingSnapshot>) {
        let ctx = Context::default();
        let schema = StructuredSchema::new(columns.iter().map(|c| c.to_string()).collect()).unwrap();
        let template = schema.row_template(template, leading).unwrap();
        let mut store = StructuredStreamingStore::new(schema)
            .with_row_template(template)
            .unwrap();
        for row in rows {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let source = Arc::new(SnapshotStore::new());
        source.publish(store.snapshot());
        source.complete();
        let picker = Picker::new(
            "icon-cards",
            &ctx,
            source,
            PickerConfig {
                preview_visible: false,
                result_rows: 3,
                ..Default::default()
            },
        );
        (ctx, picker)
    }

    /// Run `frames` frames and return the last frame's shapes.
    fn shapes(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        frames: usize,
    ) -> Vec<egui::Shape> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut shapes = Vec::new();
        for _ in 0..frames {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                },
            );
            frame.textures_delta.clear();
            shapes = frame.shapes.into_iter().map(|clipped| clipped.shape).collect();
        }
        shapes
    }

    /// The color of each `glyph` painted in `shapes`.
    fn glyph_colors(shapes: &[egui::Shape], glyph: &str) -> Vec<egui::Color32> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) if text.galley.text() == glyph => text
                    .galley
                    .job
                    .sections
                    .first()
                    .map(|section| section.format.color),
                _ => None,
            })
            .collect()
    }

    fn glyph_fonts(shapes: &[egui::Shape], glyph: char) -> Vec<egui::FontFamily> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) if text.galley.text() == glyph.to_string() => text
                    .galley
                    .job
                    .sections
                    .first()
                    .map(|section| section.format.font_id.family.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn leading_glyphs_use_their_color_or_the_text_color_and_empty_values_paint_none() {
        let (ctx, mut picker) = make_cards(
            &["name", "icon"],
            "name:title",
            Some("icon"),
            &[&["a", "glyph:●@#ff0000"], &["b", "glyph:●"], &["c", ""]],
        );
        let mut colors = glyph_colors(&shapes(&ctx, &mut picker, 1), "●");
        colors.sort_by_key(|color| color.to_array());
        let mut expected = vec![
            egui::Color32::from_rgb(255, 0, 0),
            Appearance::default().palette.text,
        ];
        expected.sort_by_key(|color| color.to_array());
        assert_eq!(colors, expected);
    }

    #[test]
    fn devicon_glyphs_draw_with_the_bundled_symbols_font() {
        let (ctx, mut picker) = make_cards(
            &["path"],
            "path:icon:devicon|path",
            None,
            &[&["src/main.rs"]],
        );
        let glyph = nfm_picker_sources::devicon::lookup("main.rs").glyph;
        // The font registers on the first frame and is used from the next.
        let families = glyph_fonts(&shapes(&ctx, &mut picker, 3), glyph);
        assert_eq!(
            families,
            vec![egui::FontFamily::Name(crate::icons::SYMBOLS_FONT_FAMILY.into())]
        );
    }

    #[test]
    fn host_references_need_a_resolver_and_resolve_off_the_ui_thread() {
        use crate::icons::IconSource;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rows: &[&[&str]] = &[&["a", "host:dot"], &["b", "hwnd:0x10"]];
        // Without resolvers, data cannot reach anything but glyphs.
        let (ctx, mut picker) = make_cards(&["name", "icon"], "icon:icon|name", None, rows);
        assert!(glyph_colors(&shapes(&ctx, &mut picker, 2), "H").is_empty());

        let (ctx, mut picker) = make_cards(&["name", "icon"], "icon:icon|name", None, rows);
        let asked = Arc::new(AtomicUsize::new(0));
        let seen = asked.clone();
        let ui_thread = std::thread::current().id();
        picker.add_icon_resolver(Arc::new(move |reference: &str, size: u32| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert_ne!(std::thread::current().id(), ui_thread);
            assert!(size > 0);
            (reference == "host:dot").then(|| IconSource::Glyph {
                text: "H".into(),
                color: Some(egui::Color32::from_rgb(1, 2, 3)),
            })
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        let fills = loop {
            let fills = glyph_colors(&shapes(&ctx, &mut picker, 1), "H");
            if !fills.is_empty() || Instant::now() > deadline {
                break fills;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(fills, vec![egui::Color32::from_rgb(1, 2, 3)]);
        // Each reference is resolved once, then cached.
        let _ = shapes(&ctx, &mut picker, 3);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn painted_host_icons_are_drawn_each_frame() {
        use crate::icons::IconSource;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (ctx, mut picker) =
            make_cards(&["name", "state"], "name:title", Some("state"), &[&["a", "swm:running"]]);
        let painted = Arc::new(AtomicUsize::new(0));
        let count = painted.clone();
        picker.add_icon_resolver(Arc::new(move |reference: &str, _: u32| {
            let count = count.clone();
            (reference == "swm:running").then(|| IconSource::Painted {
                paint: Arc::new(move |_: &egui::Painter, rect: egui::Rect, _: f64| {
                    assert!(rect.width() > 0.0 && (rect.width() - rect.height()).abs() < 0.01);
                    count.fetch_add(1, Ordering::SeqCst);
                }),
                animated: false,
            })
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while painted.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            let _ = shapes(&ctx, &mut picker, 1);
            std::thread::sleep(Duration::from_millis(5));
        }
        let before = painted.load(Ordering::SeqCst);
        assert!(before > 0);
        let _ = shapes(&ctx, &mut picker, 2);
        assert_eq!(painted.load(Ordering::SeqCst), before + 2);
    }

    /// The last frame's shapes and the x of each result's text, for a picker
    /// over plain `rows`.
    fn plain_rows(
        rows: &[(&str, u32)],
        setup: impl FnOnce(&mut Picker<FlatSnapshot<u32>>),
    ) -> (Vec<egui::Shape>, Vec<(String, f32)>) {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items(rows.iter().copied())));
        store.complete();
        let mut picker = Picker::new(
            "row-icons",
            &ctx,
            store,
            PickerConfig {
                preview_visible: false,
                result_rows: rows.len(),
                ..Default::default()
            },
        );
        setup(&mut picker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                },
            );
            frame.textures_delta.clear();
            shapes = frame.shapes.into_iter().map(|clipped| clipped.shape).collect();
        }
        let texts = shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text)
                    if rows.iter().any(|(row, _)| *row == text.galley.text()) =>
                {
                    Some((text.galley.text().to_string(), text.pos.x))
                }
                _ => None,
            })
            .collect();
        (shapes, texts)
    }

    #[test]
    fn row_icons_draw_beside_one_line_results_and_keep_text_aligned() {
        let rows = [("alpha", 1u32), ("beta", 2)];
        let (_, plain) = plain_rows(&rows, |_| {});
        let (shapes, iconed) = plain_rows(&rows, |picker| {
            picker.set_row_icon(Some(Arc::new(|item: &u32| {
                (*item == 1).then(|| "glyph:★@#010203".to_string())
            })));
        });
        assert_eq!(
            glyph_colors(&shapes, "★"),
            vec![egui::Color32::from_rgb(1, 2, 3)]
        );
        assert_eq!(plain.len(), 2);
        assert_eq!(iconed.len(), 2);
        // Both rows move right by the same column, with or without an icon.
        let shift: Vec<f32> = plain
            .iter()
            .zip(&iconed)
            .map(|((_, before), (_, after))| after - before)
            .collect();
        assert!(shift[0] > 10.0, "{shift:?}");
        assert_eq!(shift[0], shift[1]);
    }

    #[test]
    fn hovering_an_unselected_result_fills_it_without_moving_the_selection() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items(
            [("alpha", 1u32), ("beta", 2), ("gamma", 3)],
        )));
        store.complete();
        let mut picker = Picker::new(
            "hover",
            &ctx,
            store,
            PickerConfig {
                preview_visible: false,
                result_rows: 3,
                ..Default::default()
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let hover = Appearance::default().palette.hover_background();
        let mut frame = |events: Vec<egui::Event>| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                },
            );
            output.textures_delta.clear();
            output
                .shapes
                .into_iter()
                .map(|clipped| clipped.shape)
                .collect::<Vec<_>>()
        };
        let text_pos = |shapes: &[egui::Shape], wanted: &str| {
            shapes
                .iter()
                .find_map(|shape| match shape {
                    egui::Shape::Text(text) if text.galley.text() == wanted => {
                        Some(text.pos + egui::vec2(2.0, 4.0))
                    }
                    _ => None,
                })
                .unwrap()
        };
        let hover_fills = |shapes: &[egui::Shape]| {
            shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Rect(rect) if rect.fill == hover => Some(rect.rect),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        frame(vec![]);
        let shapes = frame(vec![]);
        assert!(hover_fills(&shapes).is_empty());
        let beta = text_pos(&shapes, "beta");
        let alpha = text_pos(&shapes, "alpha");

        frame(vec![egui::Event::PointerMoved(beta)]);
        let shapes = frame(vec![]);
        let fills = hover_fills(&shapes);
        assert_eq!(fills.len(), 1, "{fills:?}");
        assert!(fills[0].contains(beta));

        // The selected row keeps its own fill.
        frame(vec![egui::Event::PointerMoved(alpha)]);
        assert!(hover_fills(&frame(vec![])).is_empty());

        frame(vec![egui::Event::PointerGone]);
        assert!(hover_fills(&frame(vec![])).is_empty());
        assert_eq!(picker.selected, 0);
    }
}
