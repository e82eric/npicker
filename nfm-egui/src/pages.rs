//! Pages: pickers shown one on top of another. A host keeps a [`PageStack`]
//! of whatever it shows a picker with, draws and gives input to the top page
//! only, and pushes a page when the one showing asks for what comes next:
//! a command's argument, then the results of running it. Going back drops the
//! top page and shows the one beneath exactly as it was left, with its
//! query, results and selection.
//!
//! A page that others are pushed on top of sets
//! [`PickerConfig::close_on_accept`](crate::PickerConfig::close_on_accept) to
//! false, so it is still open when the user comes back to it. Pages may be
//! pickers made with the same name: each live picker has its own egui id, so
//! one keeps its query cursor and undo history while another is on top of it.

/// The pages a host is showing, the last one on top.
pub struct PageStack<P> {
    pages: Vec<P>,
}
impl<P> Default for PageStack<P> {
    fn default() -> Self {
        Self { pages: Vec::new() }
    }
}
impl<P> PageStack<P> {
    pub fn new() -> Self {
        Self::default()
    }
    /// Show `root` alone, in place of every page before it.
    pub fn open(&mut self, root: P) {
        self.pages.clear();
        self.pages.push(root);
    }
    /// Show `page` on top of the others.
    pub fn push(&mut self, page: P) {
        self.pages.push(page);
    }
    /// Go back to the page beneath the top one. With no page beneath, the
    /// top one stays and this returns false: going back from the first page
    /// is the host's to handle, usually by closing.
    pub fn back(&mut self) -> bool {
        if self.pages.len() > 1 {
            self.pages.pop();
            true
        } else {
            false
        }
    }
    /// Drop every page.
    pub fn clear(&mut self) {
        self.pages.clear();
    }
    /// The page showing.
    pub fn top(&self) -> Option<&P> {
        self.pages.last()
    }
    pub fn top_mut(&mut self) -> Option<&mut P> {
        self.pages.last_mut()
    }
    /// How many pages there are.
    pub fn len(&self) -> usize {
        self.pages.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn going_back_shows_the_page_beneath_and_stops_at_the_first() {
        let mut pages = PageStack::new();
        assert!(pages.top().is_none());
        assert!(!pages.back());
        pages.open("commands");
        pages.push("argument");
        pages.push("results");
        assert_eq!((pages.len(), pages.top()), (3, Some(&"results")));
        assert!(pages.back());
        assert_eq!(pages.top(), Some(&"argument"));
        assert!(pages.back());
        assert!(!pages.back(), "the first page stays");
        assert_eq!(pages.top(), Some(&"commands"));
        // Opening replaces every page.
        pages.push("argument");
        pages.open("windows");
        assert_eq!((pages.len(), pages.top()), (1, Some(&"windows")));
        pages.clear();
        assert!(pages.is_empty());
    }
}
