use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

use nfm_search_core::search::{SearchOutput, search};
use nfm_search_core::store::{StreamingItemSnapshot, StreamingItemStore};

#[repr(C)]
pub struct NfmSearchSession {
    store: StreamingItemStore,
    snapshot: Arc<StreamingItemSnapshot>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NfmSearchResult {
    pub item_index: usize,
    pub score: u32,
    pub text: *const u8,
    pub text_len: usize,
    pub positions: *const usize,
    pub position_count: usize,
}

#[repr(C)]
pub struct NfmSearchResults {
    pub results: *const NfmSearchResult,
    pub result_count: usize,
    pub matched: usize,
    pub total: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NfmSearchResultUtf16 {
    pub item_index: usize,
    pub score: u32,
    pub text: *const u16,
    pub text_len: usize,
    pub positions: *const usize,
    pub position_count: usize,
}

#[repr(C)]
pub struct NfmSearchResultsUtf16 {
    pub results: *const NfmSearchResultUtf16,
    pub result_count: usize,
    pub matched: usize,
    pub total: usize,
}

#[repr(C)]
#[allow(dead_code)]
struct OwnedSearchResults {
    header: NfmSearchResults,
    results: Vec<NfmSearchResult>,
    texts: Vec<Vec<u8>>,
    positions: Vec<Vec<usize>>,
}

impl OwnedSearchResults {
    fn new(output: SearchOutput) -> Box<Self> {
        let mut texts = Vec::with_capacity(output.results.len());
        let mut positions = Vec::with_capacity(output.results.len());

        for result in &output.results {
            texts.push(result.path.as_bytes().to_vec());
            positions.push(result.positions.clone());
        }

        let mut results = Vec::with_capacity(output.results.len());
        for (index, result) in output.results.iter().enumerate() {
            results.push(NfmSearchResult {
                item_index: result.node_index,
                score: result.score,
                text: texts[index].as_ptr(),
                text_len: texts[index].len(),
                positions: positions[index].as_ptr(),
                position_count: positions[index].len(),
            });
        }

        let header = NfmSearchResults {
            results: results.as_ptr(),
            result_count: results.len(),
            matched: output.matched,
            total: output.total,
        };

        Box::new(Self {
            header,
            results,
            texts,
            positions,
        })
    }
}

#[repr(C)]
#[allow(dead_code)]
struct OwnedSearchResultsUtf16 {
    header: NfmSearchResultsUtf16,
    results: Vec<NfmSearchResultUtf16>,
    texts: Vec<Vec<u16>>,
    positions: Vec<Vec<usize>>,
}

impl OwnedSearchResultsUtf16 {
    fn new(output: SearchOutput) -> Box<Self> {
        let mut texts: Vec<Vec<u16>> = Vec::with_capacity(output.results.len());
        let mut positions: Vec<Vec<usize>> = Vec::with_capacity(output.results.len());

        for result in &output.results {
            texts.push(result.path.encode_utf16().collect::<Vec<u16>>());
            positions.push(utf8_positions_to_utf16_offsets(
                &result.path,
                &result.positions,
            ));
        }

        let mut results = Vec::with_capacity(output.results.len());
        for (index, result) in output.results.iter().enumerate() {
            results.push(NfmSearchResultUtf16 {
                item_index: result.node_index,
                score: result.score,
                text: texts[index].as_ptr(),
                text_len: texts[index].len(),
                positions: positions[index].as_ptr(),
                position_count: positions[index].len(),
            });
        }

        let header = NfmSearchResultsUtf16 {
            results: results.as_ptr(),
            result_count: results.len(),
            matched: output.matched,
            total: output.total,
        };

        Box::new(Self {
            header,
            results,
            texts,
            positions,
        })
    }
}

fn utf8_positions_to_utf16_offsets(text: &str, positions: &[usize]) -> Vec<usize> {
    positions
        .iter()
        .map(|&position| utf16_offset_for_utf8_position(text, position))
        .collect()
}

fn utf16_offset_for_utf8_position(text: &str, position: usize) -> usize {
    let mut byte_index = position.min(text.len());
    while !text.is_char_boundary(byte_index) {
        byte_index -= 1;
    }

    text[..byte_index].encode_utf16().count()
}

#[unsafe(no_mangle)]
pub extern "C" fn nfm_search_session_create() -> *mut NfmSearchSession {
    catch_unwind(AssertUnwindSafe(|| {
        let store = StreamingItemStore::new();
        let snapshot = store.snapshot();
        Box::into_raw(Box::new(NfmSearchSession { store, snapshot }))
    }))
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_destroy(session: *mut NfmSearchSession) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !session.is_null() {
            drop(unsafe { Box::from_raw(session) });
        }
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_add_item(
    session: *mut NfmSearchSession,
    item: *const u8,
    item_len: usize,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_mut() }) else {
            return false;
        };
        if item.is_null() && item_len != 0 {
            return false;
        }

        let bytes = if item_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(item, item_len) }
        };
        session.store.add_item(bytes);
        true
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_add_item_utf16(
    session: *mut NfmSearchSession,
    item: *const u16,
    item_len: usize,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_mut() }) else {
            return false;
        };
        if item.is_null() && item_len != 0 {
            return false;
        }

        let units = if item_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(item, item_len) }
        };
        let text = String::from_utf16_lossy(units);
        session.store.add_item(text.as_bytes());
        true
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_publish(session: *mut NfmSearchSession) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_mut() }) else {
            return false;
        };
        session.store.publish();
        session.snapshot = session.store.snapshot();
        true
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_complete(session: *mut NfmSearchSession) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_mut() }) else {
            return false;
        };
        session.store.complete_adding();
        session.snapshot = session.store.snapshot();
        true
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_search(
    session: *const NfmSearchSession,
    query: *const u8,
    query_len: usize,
) -> *mut NfmSearchResults {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_ref() }) else {
            return ptr::null_mut();
        };
        if query.is_null() && query_len != 0 {
            return ptr::null_mut();
        }

        let query_bytes = if query_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(query, query_len) }
        };
        let Ok(query) = std::str::from_utf8(query_bytes) else {
            return ptr::null_mut();
        };

        let Some(output) = search(Arc::clone(&session.snapshot), query, || false) else {
            return ptr::null_mut();
        };
        let owned = OwnedSearchResults::new(output);
        Box::into_raw(owned) as *mut NfmSearchResults
    }))
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_session_search_utf16(
    session: *const NfmSearchSession,
    query: *const u16,
    query_len: usize,
) -> *mut NfmSearchResultsUtf16 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_ref() }) else {
            return ptr::null_mut();
        };
        if query.is_null() && query_len != 0 {
            return ptr::null_mut();
        }

        let units = if query_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(query, query_len) }
        };
        let query = String::from_utf16_lossy(units);

        let Some(output) = search(Arc::clone(&session.snapshot), &query, || false) else {
            return ptr::null_mut();
        };
        let owned = OwnedSearchResultsUtf16::new(output);
        Box::into_raw(owned) as *mut NfmSearchResultsUtf16
    }))
    .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_results_free(results: *mut NfmSearchResults) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !results.is_null() {
            drop(unsafe { Box::from_raw(results as *mut OwnedSearchResults) });
        }
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nfm_search_results_utf16_free(results: *mut NfmSearchResultsUtf16) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !results.is_null() {
            drop(unsafe { Box::from_raw(results as *mut OwnedSearchResultsUtf16) });
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_returns_owned_results() {
        let session = nfm_search_session_create();
        assert!(!session.is_null());

        unsafe {
            assert!(nfm_search_session_add_item(session, b"alpha".as_ptr(), 5));
            assert!(nfm_search_session_add_item(session, b"beta".as_ptr(), 4));
            assert!(nfm_search_session_complete(session));

            let results = nfm_search_session_search(session, ptr::null(), 0);
            assert!(!results.is_null());
            assert_eq!((*results).matched, 2);
            assert_eq!((*results).result_count, 2);

            let first = *(*results).results;
            assert_eq!(first.item_index, 0);
            assert_eq!(
                std::slice::from_raw_parts(first.text, first.text_len),
                b"alpha"
            );

            nfm_search_results_free(results);
            nfm_search_session_destroy(session);
        }
    }

    #[test]
    fn utf16_search_returns_utf16_text() {
        let session = nfm_search_session_create();
        assert!(!session.is_null());

        let alpha: Vec<u16> = "alpha".encode_utf16().collect();
        let beta: Vec<u16> = "βeta".encode_utf16().collect();

        unsafe {
            assert!(nfm_search_session_add_item_utf16(
                session,
                alpha.as_ptr(),
                alpha.len()
            ));
            assert!(nfm_search_session_add_item_utf16(
                session,
                beta.as_ptr(),
                beta.len()
            ));
            assert!(nfm_search_session_complete(session));

            let results = nfm_search_session_search_utf16(session, ptr::null(), 0);
            assert!(!results.is_null());
            assert_eq!((*results).matched, 2);
            assert_eq!((*results).result_count, 2);

            let second = *(*results).results.add(1);
            let text = std::slice::from_raw_parts(second.text, second.text_len);
            assert_eq!(String::from_utf16(text).unwrap(), "βeta");

            nfm_search_results_utf16_free(results);
            nfm_search_session_destroy(session);
        }
    }

    #[test]
    fn converts_utf8_byte_positions_to_utf16_offsets() {
        assert_eq!(
            utf8_positions_to_utf16_offsets("aβ𝄞z", &[0, 1, 3, 7]),
            [0, 1, 2, 4]
        );
    }
}
