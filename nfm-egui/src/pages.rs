//! Pages: pickers shown one on top of another. A host keeps a [`PageStack`]
//! of whatever it shows a picker with, draws and gives input to the top page
//! only, and pushes a page when the one showing asks for what comes next:
//! a command's argument, then the results of running it. Going back drops the
//! top page and shows the one beneath exactly as it was left, with its
//! query, results and selection.
//!
//! A picker never closes itself. It reports what the user did, the host
//! answers with an [`Outcome`], and [`PageStack::apply`] is the one place a
//! page is shown, left or dropped.
//!
//! Pages may be pickers made with the same name: each live picker has its own
//! egui id, so one keeps its query cursor and undo history while another is
//! on top of it.

/// What comes next for the pages, decided by the host from what the page
/// showing reported: an accepted result, text entered, Escape.
pub enum Outcome<P> {
    /// The page showing stays, as it is.
    Stay,
    /// Go back to the page beneath. From the first page this closes.
    Back,
    /// Every page goes.
    Close,
    /// Show this page on top of the one showing.
    Push(P),
}

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
    /// Do what the host decided comes next, and hand back the pages that
    /// left, the top one first, for a host with something to tidy up after
    /// them. The pages are closed when none is left.
    pub fn apply(&mut self, outcome: Outcome<P>) -> Vec<P> {
        match outcome {
            Outcome::Stay => Vec::new(),
            Outcome::Back => self.pages.pop().into_iter().collect(),
            Outcome::Close => self.pages.drain(..).rev().collect(),
            Outcome::Push(page) => {
                self.pages.push(page);
                Vec::new()
            }
        }
    }
    /// The top page: the one to draw and give input to.
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
    fn an_outcome_says_what_comes_next_and_hands_back_the_pages_that_left() {
        let mut pages = PageStack::new();
        assert!(pages.top().is_none());
        pages.open("commands");
        assert!(pages.apply(Outcome::Stay).is_empty());
        assert!(pages.apply(Outcome::Push("argument")).is_empty());
        assert!(pages.apply(Outcome::Push("results")).is_empty());
        assert_eq!((pages.len(), pages.top()), (3, Some(&"results")));
        assert_eq!(pages.apply(Outcome::Back), ["results"]);
        assert_eq!(pages.top(), Some(&"argument"));
        pages.apply(Outcome::Push("results"));
        assert_eq!(
            pages.apply(Outcome::Close),
            ["results", "argument", "commands"]
        );
        assert!(pages.is_empty());
        // Going back from the first page closes.
        pages.open("commands");
        assert_eq!(pages.apply(Outcome::Back), ["commands"]);
        assert!(pages.is_empty());
        assert!(pages.apply(Outcome::Back).is_empty());
        // Opening replaces every page.
        pages.open("commands");
        pages.apply(Outcome::Push("argument"));
        pages.open("windows");
        assert_eq!((pages.len(), pages.top()), (1, Some(&"windows")));
    }
}
