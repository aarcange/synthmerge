// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastSearchResult {
    NotFound,
    /// Between 1 and max_hits line-aligned matches were found
    Found(Vec<usize>),
    /// More than max_hits line-aligned matches were found
    Multiple,
}

/// Convert a byte offset to a line number (0-based)
pub fn bytes_to_lines(bytes: &[u8], offset: usize) -> usize {
    memchr::memchr_iter(b'\n', &bytes[..offset.min(bytes.len())]).count()
}

/// Convert a line number (0-based) to a byte offset
pub fn _lines_to_bytes(bytes: &[u8], line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut count = 0;
    for pos in memchr::memchr_iter(b'\n', bytes) {
        count += 1;
        if count == line {
            return pos + 1;
        }
    }
    bytes.len()
}

pub fn fast_search(snippet: &[u8], local_bytes: &[u8], max_hits: usize) -> FastSearchResult {
    if snippet.len() > local_bytes.len() {
        return FastSearchResult::NotFound;
    }

    let mut offsets = Vec::new();

    for start in memchr::memmem::find_iter(local_bytes, snippet) {
        if start > 0 && local_bytes[start - 1] != b'\n' {
            continue;
        }
        if offsets.len() >= max_hits {
            // Too many matches found
            return FastSearchResult::Multiple;
        }
        offsets.push(start);
    }

    if offsets.is_empty() {
        FastSearchResult::NotFound
    } else {
        FastSearchResult::Found(offsets)
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
