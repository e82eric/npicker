use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::fuzzy_search_session::{FuzzySearchSession, FuzzySearchUpdate, SearchSnapshotProvider};
use crate::store::ItemsSource;

pub trait SearchSource: Send + Sync {
    type Item: Clone + Send + Sync + 'static;

    fn start_search(&self, session_id: u64, query: String, updates: Sender<FuzzySearchUpdate>);
    fn set_query(&self, query: String);
    fn stop_search(&self);

    fn snapshot(&self) -> Option<Arc<dyn ItemsSource<Item = Self::Item>>>;
    fn is_done(&self) -> bool;
}

pub struct TypedSearchSource<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    provider: Arc<P>,
    search: Mutex<Option<FuzzySearchSession<S, P>>>,
    _source: PhantomData<fn() -> S>,
}

impl<S, P> TypedSearchSource<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    pub fn new(provider: Arc<P>) -> Self {
        Self {
            provider,
            search: Mutex::new(None),
            _source: PhantomData,
        }
    }
}

impl<S, P> SearchSource for TypedSearchSource<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Clone + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    type Item = S::Item;

    fn start_search(&self, session_id: u64, query: String, updates: Sender<FuzzySearchUpdate>) {
        let session =
            FuzzySearchSession::new(session_id, Arc::clone(&self.provider), query, updates);
        let previous = self
            .search
            .lock()
            .expect("search source poisoned")
            .replace(session.clone());
        if let Some(previous) = previous {
            previous.stop();
        }
        session.start();
    }

    fn set_query(&self, query: String) {
        if let Some(search) = self.search.lock().expect("search source poisoned").as_ref() {
            search.set_query(query);
        }
    }

    fn stop_search(&self) {
        if let Some(search) = self.search.lock().expect("search source poisoned").take() {
            search.stop();
        }
    }

    fn snapshot(&self) -> Option<Arc<dyn ItemsSource<Item = Self::Item>>> {
        self.provider
            .snapshot()
            .map(|source| source as Arc<dyn ItemsSource<Item = Self::Item>>)
    }

    fn is_done(&self) -> bool {
        self.provider.is_done()
    }
}
