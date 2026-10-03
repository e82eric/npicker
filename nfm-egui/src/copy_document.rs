//! Host-backed immutable documents for preview copy mode.
use crate::copy_mode::{CellPosition, CopySelection, TextRow};
use crate::{Appearance, Preview};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentMetadata {
    pub revision: u64,
    pub total_rows: u64,
    pub columns: u32,
}
pub struct DocumentRow {
    pub text: TextRow,
    /// Physical cell widths; wide-glyph continuation cells have width zero.
    pub widths: Vec<u8>,
}
/// Implementations own data or a closable lease that serializes close with reads.
/// Metadata must stay fixed while valid. Reads must return owned data and fail
/// after invalidation; an Arc around an unguarded native pointer is insufficient.
pub trait CopyDocument: Send + Sync + 'static {
    fn metadata(&self) -> Result<DocumentMetadata, String>;
    fn text_row(&self, row: u64) -> Result<Option<DocumentRow>, String>;
    fn resolve_cell(&self, cell: CellPosition, snap_right: bool) -> Result<CellPosition, String>;
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String>;
    /// Return physical cells at precisely first_row (no centering/reflow).
    fn viewport(
        &self,
        first_row: u64,
        rows: usize,
        appearance: &Appearance,
    ) -> Result<Preview, String>;
}
