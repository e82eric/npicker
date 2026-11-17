use crate::{ItemsSource, SearchSnapshotProvider, SearchSortMode};
use egui::Context;
use nfm_search_core::{
    fuzzy_search_session::FuzzySearchSession,
    search::{SearchOutput, SearchResult},
    store::SearchCompletion,
};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Clone, Default, Debug)]
pub struct SearchTimings {
    pub started: Option<Instant>,
    pub first_snapshot_ready: Option<Instant>,
    pub first_update_ready: Option<Instant>,
    pub first_matches_ready: Option<Instant>,
    pub first_matches_taken: Option<Instant>,
}
type WakeCallback = Arc<dyn Fn(SearchTimings) + Send + Sync>;

/// A source a picker can be given along with others; see [`Session`].
pub type Member<S> = Arc<dyn SearchSnapshotProvider<S>>;

/// How many members a picker can have: a row's number says which it is of
/// in its high bits, and where it is in that member in the rest. So a row
/// keeps its number whatever the other members come to have.
const MEMBER_BITS: u32 = 8;
const INDEX_BITS: u32 = usize::BITS - MEMBER_BITS;
pub const MAX_MEMBERS: usize = 1 << MEMBER_BITS;

pub(crate) fn row(member: usize, index: usize) -> usize {
    member << INDEX_BITS | index
}
/// The member a row is of, and where it is in it.
pub(crate) fn place(row: usize) -> (usize, usize) {
    (row >> INDEX_BITS, row & ((1 << INDEX_BITS) - 1))
}

/// What a member's rows were last found in.
pub(crate) struct Part<S> {
    pub snapshot: Arc<S>,
    /// Which source this is of the ones the member has had: a member given
    /// another source has other rows under the same numbers.
    epoch: u64,
    append_only: bool,
}

/// What the rows a picker shows are rows of: each member's source as it
/// was last searched, or none for one not searched yet. Rows are known by
/// the numbers `row` makes.
pub(crate) struct Joined<S> {
    parts: Vec<Option<Part<S>>>,
}
impl<S: ItemsSource> Joined<S> {
    fn part(&self, row: usize) -> Option<(&Part<S>, usize)> {
        let (member, index) = place(row);
        Some((self.parts.get(member)?.as_ref()?, index))
    }
    /// The source a row is of.
    pub fn source(&self, row: usize) -> Option<Arc<S>> {
        self.part(row).map(|(part, _)| part.snapshot.clone())
    }
    pub fn item(&self, row: usize) -> Option<S::Item> {
        let (part, index) = self.part(row)?;
        part.snapshot.item(index)
    }
    pub fn display_text(&self, row: usize, query: &str) -> Option<String> {
        let (part, index) = self.part(row)?;
        part.snapshot.display_text(index, query)
    }
    pub fn row_fields(&self, row: usize) -> Option<nfm_search_core::row::RowFields> {
        let (part, index) = self.part(row)?;
        part.snapshot.row_fields(index)
    }
    /// What of the query a row's source matches its rows against.
    pub fn effective_query(&self, row: usize, query: &str) -> Option<String> {
        self.part(row).map(|(part, _)| part.snapshot.effective_query(query))
    }
    /// Whether what a row says and is cannot change while its source goes
    /// on being added to.
    pub fn append_only(&self, row: usize) -> bool {
        self.part(row).is_some_and(|(part, _)| part.append_only)
    }
    /// Whether a row of `before` is the same row here: its member has the
    /// source it had, and that source is the same or has only been added to.
    pub fn keeps(&self, before: &Self, row: usize) -> bool {
        let (Some((now, _)), Some((was, _))) = (self.part(row), before.part(row)) else {
            return false;
        };
        now.epoch == was.epoch
            && (now.append_only || now.snapshot.version() == was.snapshot.version())
    }
    fn only(&self) -> Option<&Part<S>> {
        match self.parts.as_slice() {
            [part] => part.as_ref(),
            _ => None,
        }
    }
    /// Whether any member's rows have a header, which there is then room
    /// kept for.
    pub fn has_header(&self, query: &str) -> bool {
        self.parts.iter().flatten().any(|part| part.snapshot.header(query).is_some())
    }
    /// The header to show over `rows`, the ones on screen: that of the
    /// first member with both a header and a row among them. The one
    /// member of a picker with no others shows its header regardless.
    pub fn header(&self, query: &str, rows: impl Iterator<Item = usize> + Clone) -> Option<String> {
        if let Some(part) = self.only() {
            return part.snapshot.header(query);
        }
        let shown = |member: usize| rows.clone().any(|row| place(row).0 == member);
        let mut parts = self.parts.iter().enumerate();
        parts.find_map(|(member, part)| {
            let header = part.as_ref()?.snapshot.header(query)?;
            shown(member).then_some(header)
        })
    }
    /// Cards are for a picker with one member: rows of several members are
    /// lines, which are all of a height.
    pub fn row_template(&self) -> Option<&nfm_search_core::row::RowTemplate> {
        self.only()?.snapshot.row_template()
    }
    /// What can be completed at the cursor, by the first member that has
    /// anything to.
    pub fn completions(&self, input: &str, cursor: usize) -> Vec<SearchCompletion> {
        let parts = self.parts.iter().flatten();
        parts
            .map(|part| part.snapshot.completions(input, cursor))
            .find(|completions| !completions.is_empty())
            .unwrap_or_default()
    }
}

pub(crate) struct Update<S> {
    pub revision: u64,
    pub query: String,
    pub snapshot: Arc<Joined<S>>,
    pub output: SearchOutput,
    pub done: bool,
}
struct Provider<S> {
    snapshot: Box<dyn Fn() -> Option<Arc<S>> + Send + Sync>,
    version: Box<dyn Fn() -> u64 + Send + Sync>,
    done: Box<dyn Fn() -> bool + Send + Sync>,
    append_only: bool,
    timings: Arc<Mutex<SearchTimings>>,
    subscribe: Box<dyn Fn(crossbeam_channel::Sender<()>) -> bool + Send + Sync>,
}
impl<S: ItemsSource + Send + Sync + 'static> SearchSnapshotProvider<S> for Provider<S> {
    fn snapshot(&self) -> Option<Arc<S>> {
        let snapshot = (self.snapshot)();
        if snapshot.as_ref().is_some_and(|s| s.len() > 0) {
            self.timings
                .lock()
                .unwrap()
                .first_snapshot_ready
                .get_or_insert_with(Instant::now);
        }
        snapshot
    }
    fn snapshot_version(&self) -> u64 {
        (self.version)()
    }
    fn is_done(&self) -> bool {
        (self.done)()
    }
    fn is_append_only(&self) -> bool {
        self.append_only
    }
    fn subscribe_updates(&self, wake: crossbeam_channel::Sender<()>) -> bool {
        (self.subscribe)(wake)
    }
}

/// What one member's search last found.
struct Found<S> {
    /// The session's revision the search was of.
    revision: u64,
    query: String,
    part: Part<S>,
    output: SearchOutput,
    done: bool,
}

/// One member: a source and its own search, which says what it finds
/// whenever its source or the query changes.
struct Searching<S: ItemsSource + Send + Sync + 'static> {
    search: FuzzySearchSession<S, Provider<S>>,
    latest: Arc<Mutex<Option<Found<S>>>>,
    /// What it last found that the session has taken. A member given
    /// another source goes on showing what the one before found until the
    /// new one has been searched.
    found: Option<Found<S>>,
    done: Box<dyn Fn() -> bool + Send + Sync>,
    skipped: Box<dyn Fn() -> usize + Send + Sync>,
}

/// A picker's search: one for each of its members, each over its own
/// source with its own ranking and its own limit on how many matches it
/// keeps, and each found again only when its own source changes. What they
/// find is shown as one list, the members' rows in the order of the
/// members.
///
/// Uses the same incremental search session as Ghostty for each, with a
/// coalescing snapshot-aware observer rather than a second full-rescan
/// worker.
pub(crate) struct Session<S: ItemsSource + Send + Sync + 'static> {
    members: Vec<Searching<S>>,
    query: String,
    sort: SearchSortMode,
    ctx: Context,
    revision: u64,
    epochs: u64,
    timings: Arc<Mutex<SearchTimings>>,
    wake_callback: Arc<Mutex<Option<WakeCallback>>>,
}
impl<S: ItemsSource + Send + Sync + 'static> Session<S> {
    pub fn new<P: SearchSnapshotProvider<S> + ?Sized>(
        provider: Arc<P>,
        query: String,
        sort: SearchSortMode,
        ctx: Context,
    ) -> Self {
        let mut session = Self::empty(query, sort, ctx);
        let member = session.searching(provider);
        session.members.push(member);
        session
    }
    /// A search of several members; see [`Session`].
    pub fn of(members: Vec<Member<S>>, query: String, sort: SearchSortMode, ctx: Context) -> Self {
        assert!(members.len() <= MAX_MEMBERS, "a picker has at most {MAX_MEMBERS} members");
        let mut session = Self::empty(query, sort, ctx);
        for provider in members {
            let member = session.searching(provider);
            session.members.push(member);
        }
        session
    }
    fn empty(query: String, sort: SearchSortMode, ctx: Context) -> Self {
        Self {
            members: Vec::new(),
            query,
            sort,
            ctx,
            revision: 0,
            epochs: 0,
            timings: Arc::new(Mutex::new(SearchTimings {
                started: Some(Instant::now()),
                ..Default::default()
            })),
            wake_callback: Arc::new(Mutex::new(None)),
        }
    }
    /// Give a member another source, leaving the others as they are: their
    /// rows keep their numbers, and are not searched again.
    pub fn replace_member<P: SearchSnapshotProvider<S> + ?Sized>(
        &mut self,
        member: usize,
        provider: Arc<P>,
    ) {
        if member >= self.members.len() {
            return;
        }
        let searching = self.searching(provider);
        let before = std::mem::replace(&mut self.members[member], searching);
        before.search.stop();
        self.members[member].found = before.found;
    }
    /// Start searching a source, with the query as it is now.
    fn searching<P: SearchSnapshotProvider<S> + ?Sized>(&mut self, provider: Arc<P>) -> Searching<S> {
        self.epochs += 1;
        let epoch = self.epochs;
        // The search counts its own queries from none; the session's
        // revision is that many past what it is now.
        let base = self.revision;
        let append_only = provider.is_append_only();
        let (skipped_provider, done_provider) = (provider.clone(), provider.clone());
        let (snapshot_provider, version_provider, searched_done, notification_provider) = (
            provider.clone(),
            provider.clone(),
            provider.clone(),
            provider,
        );
        let provider = Arc::new(Provider {
            snapshot: Box::new(move || snapshot_provider.snapshot()),
            version: Box::new(move || version_provider.snapshot_version()),
            done: Box::new(move || searched_done.is_done()),
            append_only,
            timings: self.timings.clone(),
            subscribe: Box::new(move |wake| notification_provider.subscribe_updates(wake)),
        });
        let latest = Arc::new(Mutex::new(None));
        let worker_latest = latest.clone();
        let worker_timings = self.timings.clone();
        let worker_wake = self.wake_callback.clone();
        let ctx = self.ctx.clone();
        let search = FuzzySearchSession::new_with_snapshot_observer(
            0,
            provider,
            self.query.clone(),
            self.sort,
            move |update, snapshot| {
                let mut output = SearchOutput::default();
                output.results = update.results;
                output.matched = update.matched;
                output.total = update.total;
                let has_matches = output.matched > 0;
                let ready = {
                    let mut timings = worker_timings.lock().unwrap();
                    timings.first_update_ready.get_or_insert_with(Instant::now);
                    if has_matches {
                        timings.first_matches_ready.get_or_insert_with(Instant::now);
                    }
                    timings.clone()
                };
                let incoming = Found {
                    revision: base + update.generation,
                    query: update.query,
                    part: Part { snapshot, epoch, append_only },
                    output,
                    done: update.source_done,
                };
                // Release the UI mailbox before destroying superseded results.
                drop(replace_pending(&worker_latest, incoming));
                let callback = worker_wake.lock().unwrap().clone();
                if let Some(callback) = callback {
                    callback(ready);
                }
                ctx.request_repaint();
            },
        );
        search.start();
        Searching {
            search,
            latest,
            found: None,
            done: Box::new(move || done_provider.is_done()),
            skipped: Box::new(move || skipped_provider.skipped_items()),
        }
    }
    /// Source records left out, such as overlong lines.
    pub fn skipped(&self) -> usize {
        self.members.iter().map(|member| (member.skipped)()).sum()
    }
    pub fn set_query(&mut self, query: String) -> u64 {
        self.revision += 1;
        self.query = query;
        for member in &self.members {
            member.search.set_query(self.query.clone());
        }
        self.revision
    }
    /// What the members have found, if any has found anything since this
    /// was last asked. After a query changes there is nothing until every
    /// member that has found anything has found for the new query: until
    /// then the rows shown stay, as one member's do.
    pub fn take(&mut self) -> Option<Update<S>> {
        let mut found = false;
        for member in &mut self.members {
            let latest = member.latest.lock().unwrap().take();
            if let Some(latest) = latest {
                // Outside the mailbox's lock, as the worker does.
                drop(member.found.replace(latest));
                found = true;
            }
        }
        if !found {
            return None;
        }
        let revision = self.members.iter().flat_map(|member| &member.found).map(|f| f.revision);
        let revision = revision.max()?;
        if self.members.iter().flat_map(|member| &member.found).any(|f| f.revision != revision) {
            return None;
        }
        let mut output = SearchOutput::default();
        let mut parts = Vec::with_capacity(self.members.len());
        let mut done = true;
        let mut query = None;
        for (at, member) in self.members.iter().enumerate() {
            let Some(found) = &member.found else {
                // Not searched yet: it has no source to search so far.
                done &= (member.done)();
                parts.push(None);
                continue;
            };
            done &= found.done;
            query.get_or_insert_with(|| found.query.clone());
            output.matched += found.output.matched;
            output.total += found.output.total;
            output.results.extend(found.output.results.iter().map(|result| SearchResult {
                node_index: row(at, result.node_index),
                score: result.score,
                path: result.path.clone(),
            }));
            parts.push(Some(Part {
                snapshot: found.part.snapshot.clone(),
                epoch: found.part.epoch,
                append_only: found.part.append_only,
            }));
        }
        if output.matched > 0 {
            self.timings
                .lock()
                .unwrap()
                .first_matches_taken
                .get_or_insert_with(Instant::now);
        }
        Some(Update {
            revision,
            query: query?,
            snapshot: Arc::new(Joined { parts }),
            output,
            done,
        })
    }
    pub fn timings(&self) -> SearchTimings {
        self.timings.lock().unwrap().clone()
    }
    pub fn set_wake_callback(&self, callback: WakeCallback) {
        *self.wake_callback.lock().unwrap() = Some(callback.clone());
        // A result may have arrived before the host attached its wake callback.
        let timings = self.timings();
        if timings.first_update_ready.is_some() {
            callback(timings);
        }
    }
    pub fn stop(&self) {
        for member in &self.members {
            member.search.stop();
        }
    }
}
impl<S: ItemsSource + Send + Sync + 'static> Drop for Session<S> {
    fn drop(&mut self) {
        self.stop();
    }
}

// Return ownership after releasing the mailbox guard. The caller must dispose
// of old results outside the lock used by the UI.
fn replace_pending<T>(latest: &Mutex<Option<T>>, incoming: T) -> Option<T> {
    latest.lock().unwrap().replace(incoming)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    };
    struct CheckDrop {
        latest: Weak<Mutex<Option<CheckDrop>>>,
        unlocked: Arc<AtomicBool>,
    }
    impl Drop for CheckDrop {
        fn drop(&mut self) {
            if let Some(latest) = self.latest.upgrade() {
                self.unlocked
                    .store(latest.try_lock().is_ok(), Ordering::Release);
            }
        }
    }
    #[test]
    fn superseded_results_are_destroyed_without_holding_the_ui_mailbox() {
        let latest = Arc::new(Mutex::new(None));
        let unlocked = Arc::new(AtomicBool::new(false));
        let item = || CheckDrop {
            latest: Arc::downgrade(&latest),
            unlocked: unlocked.clone(),
        };
        *latest.lock().unwrap() = Some(item());
        drop(replace_pending(&latest, item()));
        assert!(unlocked.load(Ordering::Acquire));
    }
}
