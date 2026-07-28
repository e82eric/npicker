use std::sync::Arc;

use nfm_search_core::store::{ChunkedSnapshot, ChunkedStorage, ItemsSource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelimitedTextSelector {
    FullLine,
    Field(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DelimitedValueSelector {
    FullLine,
    Field(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelimitedItemMetadata {
    pub value: String,
    pub preview_item: Option<String>,
    pub preview_center_line: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ByteRange {
    offset: usize,
    length: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct RelativeRange {
    offset: usize,
    length: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct DelimitedItem {
    full_line: ByteRange,
    search_text: RelativeRange,
}

pub struct DelimitedStreamingStore {
    items: ChunkedStorage<DelimitedItem>,
    bytes: ChunkedStorage<u8>,
    version: u64,
    delimiter: char,
    text: DelimitedTextSelector,
    value: DelimitedValueSelector,
    preview_file_field: Option<usize>,
    preview_center_line_field: Option<usize>,
}

impl DelimitedStreamingStore {
    pub fn new(
        delimiter: char,
        text: DelimitedTextSelector,
        value: DelimitedValueSelector,
        preview_file_field: Option<usize>,
        preview_center_line_field: Option<usize>,
    ) -> Self {
        Self {
            items: ChunkedStorage::new(64 * 1024),
            bytes: ChunkedStorage::new(1024 * 1024),
            version: 0,
            delimiter,
            text,
            value,
            preview_file_field,
            preview_center_line_field,
        }
    }

    pub fn add_item(&mut self, full_line: &[u8], search_offset: usize, search_length: usize) {
        debug_assert!(search_offset + search_length <= full_line.len());
        let full_line_range = ByteRange {
            offset: self.bytes.len(),
            length: full_line.len(),
        };
        self.bytes.extend_from_slice(full_line);
        self.items.push(DelimitedItem {
            full_line: full_line_range,
            search_text: RelativeRange {
                offset: search_offset,
                length: search_length,
            },
        });
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn snapshot(&mut self) -> Arc<DelimitedStreamingSnapshot> {
        self.version += 1;
        Arc::new(DelimitedStreamingSnapshot {
            items: self.items.snapshot(),
            items_count: self.items.len(),
            bytes: self.bytes.snapshot(),
            byte_count: self.bytes.len(),
            version: self.version,
            delimiter: self.delimiter,
            text: self.text,
            value: self.value,
            preview_file_field: self.preview_file_field,
            preview_center_line_field: self.preview_center_line_field,
        })
    }
}

pub struct DelimitedStreamingSnapshot {
    items: ChunkedSnapshot<DelimitedItem>,
    items_count: usize,
    bytes: ChunkedSnapshot<u8>,
    byte_count: usize,
    version: u64,
    delimiter: char,
    text: DelimitedTextSelector,
    value: DelimitedValueSelector,
    preview_file_field: Option<usize>,
    preview_center_line_field: Option<usize>,
}

impl DelimitedStreamingSnapshot {
    fn copy_range(&self, range: ByteRange, out: &mut [u8]) {
        let mut remaining = range.length;
        let mut offset = range.offset;
        let mut written = 0;
        while remaining > 0 {
            let (chunk_index, chunk_offset) = self.bytes.locate_direct(offset);
            let chunk = &self.bytes.chunks[chunk_index];
            let readable = remaining.min(chunk.len() - chunk_offset);
            out[written..written + readable]
                .copy_from_slice(&chunk[chunk_offset..chunk_offset + readable]);
            remaining -= readable;
            offset += readable;
            written += readable;
        }
    }

    fn get_range<'a>(
        &'a self,
        range: ByteRange,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        debug_assert!(range.offset + range.length <= self.byte_count);
        if range.length == 0 {
            return &stack_buffer[..0];
        }
        let (chunk_index, chunk_offset) = self.bytes.locate_direct(range.offset);
        let chunk = &self.bytes.chunks[chunk_index];
        if chunk_offset + range.length <= chunk.len() {
            return &chunk[chunk_offset..chunk_offset + range.length];
        }
        if range.length > stack_buffer.len() {
            heap_buffer.resize(range.length, 0);
            self.copy_range(range, heap_buffer);
            return heap_buffer;
        }
        self.copy_range(range, &mut stack_buffer[..range.length]);
        &stack_buffer[..range.length]
    }

    fn full_line(&self, index: usize, out: &mut Vec<u8>) -> String {
        let item = self.items[index];
        let mut stack = [0; 4096];
        String::from_utf8_lossy(self.get_range(item.full_line, &mut stack, out)).into_owned()
    }

    fn field<'a>(&self, line: &'a str, field: usize) -> Option<&'a str> {
        if self.text == DelimitedTextSelector::Field(field) {
            line.splitn(field + 1, self.delimiter).nth(field)
        } else {
            line.split(self.delimiter).nth(field)
        }
    }

    pub fn metadata(&self, index: usize) -> DelimitedItemMetadata {
        let mut buffer = Vec::new();
        let line = self.full_line(index, &mut buffer);
        let value = match self.value {
            DelimitedValueSelector::FullLine => line.clone(),
            DelimitedValueSelector::Field(field) => {
                self.field(&line, field).unwrap_or_default().to_owned()
            }
        };
        DelimitedItemMetadata {
            value,
            preview_item: self
                .preview_file_field
                .and_then(|field| self.field(&line, field))
                .map(str::to_owned),
            preview_center_line: self
                .preview_center_line_field
                .and_then(|field| self.field(&line, field))
                .and_then(|value| value.parse().ok())
                .filter(|line| *line > 0),
        }
    }
}

impl ItemsSource for DelimitedStreamingSnapshot {
    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.items_count
    }

    fn is_empty(&self) -> bool {
        self.items_count == 0
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        let item = self.items[index];
        self.get_range(
            ByteRange {
                offset: item.full_line.offset + item.search_text.offset,
                length: item.search_text.length,
            },
            stack_buffer,
            heap_buffer,
        )
    }

    fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
        let mut stack = [0; 4096];
        String::from_utf8_lossy(self.get_string(index, &mut stack, out)).into_owned()
    }
}
