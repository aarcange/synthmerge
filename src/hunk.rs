// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use anyhow::Result;

/// Represents a single hunk from a git diff
#[derive(Clone, PartialEq)]
pub struct Hunk {
    /// The text after the second @@
    pub header: String,
    /// The body of the hunk containing context lines (prefixed with space)
    /// All lines in body must be prefixed with a space.
    pub body: Vec<String>,
    /// Start line in the base file
    pub base_start: usize,
    /// Length of the base section (lines with ' ' or '-')
    pub base_len: usize,
    /// Start line in the remote file
    pub remote_start: usize,
    /// Length of the remote section (lines with ' ' or '+')
    pub remote_len: usize,
    pub clean: bool,
}

impl std::fmt::Debug for Hunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hunk")
            .field("clean", &self.clean)
            .field("body", &format_args!("\n{}", self))
            .finish()
    }
}

impl std::fmt::Display for Hunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = if self.base_len == 1 {
            format!("-{}", self.base_start)
        } else {
            format!("-{},{}", self.base_start, self.base_len)
        };
        let remote = if self.remote_len == 1 {
            format!("+{}", self.remote_start)
        } else {
            format!("+{},{}", self.remote_start, self.remote_len)
        };
        write!(f, "@@ {} {} @@{}", base, remote, self.header)?;
        for line in &self.body {
            write!(f, "{}", line)?;
        }
        Ok(())
    }
}

impl Hunk {
    const KEEP_ALL_CONTEXT: bool = false;

    /// Returns the base and remote ranges for this hunk, accounting for head/tail context.
    pub fn get_ranges(&self) -> (usize, usize, usize, usize) {
        let head = self.get_head_context().len();
        let tail = self.get_tail_context().len();
        let hunk_base_start = if self.base_len == 0 {
            assert_eq!(head, 0);
            self.base_start
        } else {
            self.base_start - 1 + head
        };
        let hunk_base_end = hunk_base_start + self.base_len - head - tail;
        let hunk_remote_start = if self.remote_len == 0 {
            assert_eq!(head, 0);
            self.remote_start
        } else {
            self.remote_start - 1 + head
        };
        let hunk_remote_end = hunk_remote_start + self.remote_len - head - tail;
        (
            hunk_base_start,
            hunk_base_end,
            hunk_remote_start,
            hunk_remote_end,
        )
    }

    /// Returns the head context lines for this hunk.
    /// Head context lines are the leading lines that start with a space (context)
    /// The returned lines are trimmed (leading whitespace stripped).
    pub fn get_head_context(&self) -> Vec<String> {
        self.__get_context(true)
    }

    /// Returns the tail context lines for this hunk.
    /// Tail context lines are the trailing lines that start with a space (context)
    /// The returned lines are trimmed (leading whitespace stripped).
    pub fn get_tail_context(&self) -> Vec<String> {
        self.__get_context(false)
    }

    /// Common implementation for get_head_context and get_tail_context.
    /// If `head` is true, collects from the beginning of the body.
    /// If `head` is false, collects from the end of the body.
    fn __get_context(&self, head: bool) -> Vec<String> {
        let mut result = Vec::new();
        let body_len = self.body.len();
        if body_len == 0 {
            return result;
        }

        let iter: Box<dyn Iterator<Item = &String>> = if head {
            Box::new(self.body.iter())
        } else {
            Box::new(self.body.iter().rev())
        };

        for line in iter {
            // Context lines start with a space.
            // We stop if we encounter a line that doesn't start with a space.
            if let Some(remainder) = line.strip_prefix(' ') {
                result.push(remainder.to_string());
            } else {
                // If it doesn't start with whitespace, break
                break;
            }
        }

        if !head {
            result.reverse();
        }

        result
    }

    /// Extracts the patch conflict code from the hunk by removing the head and tail context.
    ///
    /// # Returns
    /// The patch conflict code string with head and tail context removed.
    pub fn get_patch_conflict(&self) -> &[String] {
        let head = self.get_head_context().len();
        let tail = self.get_tail_context().len();

        assert!(self.body.len() >= head + tail);
        &self.body[head..self.body.len() - tail]
    }

    pub fn get_conflict_base(&self) -> Result<Vec<String>> {
        self.__get_conflict(true)
    }

    pub fn get_conflict_remote(&self) -> Result<Vec<String>> {
        self.__get_conflict(false)
    }

    /// # Arguments
    /// * `base` - If true, extract base lines
    ///   If false, extract remote lines
    ///
    /// # Returns
    /// The conflict code string with head and tail context removed.
    fn __get_conflict(&self, base: bool) -> Result<Vec<String>> {
        let (selector_char, skip_char) = if base { ('-', '+') } else { ('+', '-') };
        let mut result = Vec::new();
        for line in self.get_patch_conflict() {
            if let Some(stripped) = line.strip_prefix(' ') {
                result.push(stripped.to_string());
            } else if let Some(stripped) = line.strip_prefix(selector_char) {
                result.push(stripped.to_string());
            } else if !line.starts_with(skip_char) {
                return Err(anyhow::anyhow!(
                    "Invalid hunk body line: '{}'. All body lines must start with ' ', '-', or '+'.",
                    line
                ));
            }
        }
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        let mut base_len = 0;
        let mut remote_len = 0;
        // Validate that all body lines start with a space, - or +
        for line in &self.body {
            match line.as_bytes().first() {
                Some(b' ') => {
                    base_len += 1;
                    remote_len += 1;
                }
                Some(b'-') => base_len += 1,
                Some(b'+') => remote_len += 1,
                // By design empty newline isn't expected to ever happen
                //Some(b'\\') if line.trim_end() == r"\ No newline at end of file" => {}
                _ => {
                    anyhow::bail!(
                        "Invalid hunk body line: '{}'. All body lines must start with ' ', '-', or '+'.",
                        line
                    );
                }
            }
        }

        if base_len != self.base_len || remote_len != self.remote_len {
            anyhow::bail!(
                "Invalid hunk: base_len={}, remote_len={}",
                base_len,
                remote_len
            );
        }

        Ok(())
    }

    /// Splits the hunk body into multiple strings, each containing at
    /// least `patch_context_lines` context lines before and after the
    /// changed lines (+ or -).
    ///
    /// The split occurs only between changes, not at the very start
    /// or end of the hunk body. Each resulting string will start
    /// with up to `patch_context_lines` context lines and end with up
    /// to `patch_context_lines` context lines surrounding the
    /// changes.
    pub fn split(
        &self,
        patch_context_lines: usize,
        hard_patch_context_lines: usize,
    ) -> Result<Vec<Hunk>> {
        let mut hunks: Vec<Hunk> = Vec::new();
        let mut current_body: Vec<String> = Vec::new();
        let mut pending_context: Vec<&str> = Vec::new();
        let mut seen_first_change = false;

        // Track line counts for the current chunk
        let mut base_count = 0;
        let mut remote_count = 0;

        // Track how much of the original hunk has been skipped (not
        // included in current snippet)
        let mut skipped_base = 0;
        let mut skipped_remote = 0;

        for line in &self.body {
            if !seen_first_change {
                // Accumulate lines until we hit the first change
                current_body.push(line.clone());
                if line.starts_with(' ') {
                    base_count += 1;
                    remote_count += 1;
                } else if line.starts_with('-') {
                    base_count += 1;
                } else if line.starts_with('+') {
                    remote_count += 1;
                }

                if line.starts_with('+') || line.starts_with('-') {
                    seen_first_change = true;
                    pending_context.clear();
                }
                continue;
            }

            if line.starts_with(' ') {
                // Accumulate whitespace lines in a separate vector
                pending_context.push(line);
            } else {
                // Hit a non-whitespace line (another change)
                // If we accumulated at least patch_context_lines
                // whitespace context lines,
                // finalize the current snippet and start a new one.
                if pending_context.len() >= patch_context_lines {
                    if pending_context.len() > hard_patch_context_lines.saturating_mul(2) {
                        return Err(anyhow::anyhow!(
                            "Unexpected number of pending context lines ({}) for patch_context_lines ({})",
                            pending_context.len(),
                            patch_context_lines
                        ));
                    }

                    let patch_context_lines = if !Self::KEEP_ALL_CONTEXT {
                        hard_patch_context_lines
                            .min(pending_context.len())
                            .max(patch_context_lines)
                    } else {
                        pending_context.len()
                    };

                    // Append the first patch_context_lines of context
                    // to the current snippet
                    current_body.extend_from_slice(
                        &pending_context[..patch_context_lines]
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    );

                    // Update counts for the appended context lines
                    base_count += patch_context_lines;
                    remote_count += patch_context_lines;

                    // Finalize current hunk
                    if !current_body.is_empty() {
                        let hunk = Hunk {
                            header: self.header.clone(),
                            body: current_body.clone(),
                            base_start: self.base_start + skipped_base,
                            base_len: base_count,
                            remote_start: self.remote_start + skipped_remote,
                            remote_len: remote_count,
                            clean: self.clean,
                        };
                        hunk.validate()?;
                        hunks.push(hunk);
                    }

                    let start_idx = pending_context.len() - patch_context_lines;

                    // Calculate how many lines we advanced in the base and remote
                    skipped_base += base_count + start_idx - patch_context_lines;
                    skipped_remote += remote_count + start_idx - patch_context_lines;

                    // Start new snippet with the last patch_context_lines of context
                    current_body.clear();
                    current_body.extend_from_slice(
                        &pending_context[start_idx..]
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    );

                    // Reset counts for new snippet
                    base_count = patch_context_lines;
                    remote_count = patch_context_lines;
                } else {
                    current_body.extend(pending_context.iter().map(|s| s.to_string()));
                    base_count += pending_context.len();
                    remote_count += pending_context.len();
                }

                // Reset pending context
                pending_context.clear();

                // Process the current change line
                current_body.push(line.clone());
                if line.starts_with('-') {
                    base_count += 1;
                } else if line.starts_with('+') {
                    remote_count += 1;
                }
            }
        }

        // Add any remaining pending context to the current snippet
        current_body.extend_from_slice(
            &pending_context
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>(),
        );

        // Update counts for remaining context
        for line in &pending_context {
            if !line.starts_with(' ') {
                return Err(anyhow::anyhow!(
                    "Invalid pending context line: '{}'. All context lines must start with a space.",
                    line
                ));
            }
            base_count += 1;
            remote_count += 1;
        }

        // Add the final hunk
        if !current_body.is_empty() {
            let hunk = Hunk {
                header: self.header.clone(),
                body: current_body,
                base_start: self.base_start + skipped_base,
                base_len: base_count,
                remote_start: self.remote_start + skipped_remote,
                remote_len: remote_count,
                clean: self.clean,
            };
            hunk.validate()?;
            hunks.push(hunk);
        }

        Ok(hunks)
    }

    /// Extracts the "base" code snippet from a single hunk for patch_locator
    /// Returns a string being the concatenation of base_lines for the hunk
    /// Base lines are: lines with a leading whitespace (context) or leading '-' (removed)
    /// The leading whitespace and '-' are stripped from the lines.
    pub fn extract_base_snippet(&self) -> Result<String> {
        self.__extract_snippet(true, false)
    }

    /// Extracts the "remote" code snippet from a single hunk for patch_locator
    /// Returns a string being the concatenation of remote_lines for the hunk
    /// Remote lines are: lines with a leading whitespace (context) or leading '+' (added)
    /// The leading whitespace and '+' are stripped from the lines.
    pub fn extract_remote_snippet(&self) -> Result<String> {
        self.__extract_snippet(false, false)
    }

    /// Extracts all code lines from a single hunk for patch_locator
    /// Returns a string being the concatenation of all lines
    /// The leading whitespace, '+' or '-' are stripped from the lines.
    #[cfg(false)]
    pub fn extract_base_remote_snippet(&self) -> Result<String> {
        self.__extract_snippet(false, true)
    }

    /// Extracts a code snippet from a single hunk for patch_locator
    ///
    /// # Arguments
    /// * `remote` - If true, extract remote lines (context and added lines).
    ///   If false, extract base lines (context and removed lines).
    /// * `all` - If true, extract all lines (context, added, and removed).
    fn __extract_snippet(&self, base: bool, all: bool) -> Result<String> {
        let (selector_char, skip_char) = if base { ('-', '+') } else { ('+', '-') };
        let mut lines = Vec::new();

        for line in &self.body {
            if let Some(stripped) = line.strip_prefix(' ') {
                lines.push(stripped.to_string());
            } else if let Some(stripped) = line.strip_prefix(selector_char) {
                lines.push(stripped.to_string());
            } else if all {
                if let Some(stripped) = line.strip_prefix(skip_char) {
                    lines.push(stripped.to_string());
                } else {
                    return Err(anyhow::anyhow!(
                        "Invalid hunk body line: '{}'. All body lines must start with ' ', '-', or '+'.",
                        line
                    ));
                }
            } else if !line.starts_with(skip_char) {
                return Err(anyhow::anyhow!(
                    "Invalid hunk body line: '{}'. All body lines must start with ' ', '-', or '+'.",
                    line
                ));
            }
        }
        Ok(lines.join(""))
    }

    // Only context can overlap
    pub fn merge_overlapping_hunks(mut hunks: Vec<Hunk>) -> Result<Vec<Hunk>> {
        hunks.sort_by_key(|hunk| (hunk.base_start, hunk.remote_start));

        let mut merged: Vec<Hunk> = Vec::with_capacity(hunks.len());
        for right in hunks {
            if right.base_len == 0 || right.remote_len == 0 {
                merged.push(right);
                continue;
            }
            if let Some(left) = merged.last_mut()
                && left.clean == right.clean
                && left.base_len != 0
                && left.remote_len != 0
            {
                let left_base_end = left.base_start + left.base_len;
                let left_remote_end = left.remote_start + left.remote_len;

                let right_base_end = right.base_start + right.base_len;
                let right_remote_end = right.remote_start + right.remote_len;

                let base_overlaps =
                    left.base_start < right_base_end && right.base_start < left_base_end;
                let remote_overlaps =
                    left.remote_start < right_remote_end && right.remote_start < left_remote_end;

                if base_overlaps || remote_overlaps {
                    let overlap_len = left_base_end.saturating_sub(right.base_start);
                    let left_tail_len = left.get_tail_context().len();
                    let right_head_len = right.get_head_context().len();
                    if overlap_len == 0
                        || left_remote_end.checked_sub(right.remote_start) != Some(overlap_len)
                        || overlap_len > left_tail_len
                        || overlap_len > right_head_len
                    {
                        return Err(anyhow::anyhow!(
                            "Hunk ranges overlap outside tail and head context: left base=[{},{}), remote=[{},{}), tail={}; right base=[{},{}), remote=[{},{}), head={}",
                            left.base_start,
                            left_base_end,
                            left.remote_start,
                            left_remote_end,
                            left_tail_len,
                            right.base_start,
                            right_base_end,
                            right.remote_start,
                            right_remote_end,
                            right_head_len,
                        ));
                    }
                    if left.body[left.body.len() - overlap_len..] != right.body[..overlap_len] {
                        return Err(anyhow::anyhow!(
                            "Hunk ranges overlap but their overlapping payloads differ: left base=[{},{}), remote=[{},{}), tail={}; right base=[{},{}), remote=[{},{}), head={}",
                            left.base_start,
                            left_base_end,
                            left.remote_start,
                            left_remote_end,
                            left_tail_len,
                            right.base_start,
                            right_base_end,
                            right.remote_start,
                            right_remote_end,
                            right_head_len,
                        ));
                    }

                    left.body.extend(right.body.into_iter().skip(overlap_len));
                    left.base_len += right.base_len - overlap_len;
                    left.remote_len += right.remote_len - overlap_len;
                    left.validate()?;
                    continue;
                }
            }

            merged.push(right);
        }

        Ok(merged)
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
