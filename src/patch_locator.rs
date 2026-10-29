// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use crate::conflict_resolver::{CommitType, Conflict, ResolvedConflict, Snippet};
use crate::fast_search::*;
use crate::git_utils::ContextLines;
use crate::hunk::Hunk;
use crate::lmdb_cache::{LmdbCacheImpl, PatchLocatorCache};
use anyhow::Result;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use textdistance::nstr;

pub struct PatchLocator {
    header_regex: Regex,
    pub local_content: Arc<String>,
    pub merged_local_content: Arc<String>,
    pub merged_local_lines: Arc<Vec<String>>,
    pub clean_diff: Arc<String>,
    pub conflict_diff: Arc<String>,
    pub lmdb_cache: Option<Arc<LmdbCacheImpl>>,
    pub context_lines: ContextLines,
    pub max_context_size: u32,
    pub conflict_relocation: bool,
}

impl PatchLocator {
    const MAX_RELOCATION_DISTANCE: f64 = 0.2;
    const MAX_BODY_LEN: usize = 1000;
    const MAX_SCAN: usize = 2000;
    const MAX_BASE_SCAN: usize = 100;
    const MAX_BASE_DISTANCE: f64 = 0.1;
    const MARKERS_CONTEXT_LINES: usize = 1;
    const MISPLACED_CONTEXT_LINES: usize = 4;
    const EXTEND_CONFLICT_ON_CLEAN_MERGE: bool = true;
    const RELOCATE_BOTH_LENGTH_FACTOR: usize = 100;
    const EXTEND_IF_WITHIN_LINES: usize = 100;

    // https://github.com/rust-lang/rust-clippy/issues/1576
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local_content: Arc<String>,
        merged_local_content: Arc<String>,
        merged_local_lines: Arc<Vec<String>>,
        clean_diff: Arc<String>,
        conflict_diff: Arc<String>,
        lmdb_cache: Option<Arc<LmdbCacheImpl>>,
        context_lines: ContextLines,
        max_context_size: u32,
        conflict_relocation: bool,
    ) -> Self {
        PatchLocator {
            header_regex: Regex::new(r"^@@ -(\d+)(,\d+)? \+(\d+)(,\d+)? @@").unwrap(),
            local_content,
            merged_local_content,
            merged_local_lines,
            clean_diff,
            conflict_diff,
            lmdb_cache,
            context_lines,
            max_context_size,
            conflict_relocation,
        }
    }

    /// Parses git diff output into a list of Hunk structures
    pub fn diff_to_hunks(&self, diff_output: &str, clean: bool) -> Result<Vec<Hunk>> {
        let mut hunks = Vec::new();
        let mut header = String::new();
        let mut body_lines: Vec<String> = Vec::new();
        let mut in_hunk = false;
        let mut base_start: usize = 0;
        let mut remote_start: usize = 0;
        let mut base_count: usize = 0;
        let mut remote_count: usize = 0;

        let finalize_hunk = |header: String,
                             body: Vec<String>,
                             base_start: usize,
                             base_count: usize,
                             remote_start: usize,
                             remote_count: usize|
         -> Result<Hunk> {
            let hunk = Hunk {
                header,
                body,
                base_start,
                base_len: base_count,
                remote_start,
                remote_len: remote_count,
                clean,
            };
            hunk.validate()?;
            Ok(hunk)
        };

        for line in diff_output.split_inclusive('\n') {
            if line.starts_with("@@") {
                // Save previous hunk if exists
                if in_hunk {
                    hunks.push(finalize_hunk(
                        header.clone(),
                        body_lines.clone(),
                        base_start,
                        base_count,
                        remote_start,
                        remote_count,
                    )?);
                    body_lines.clear();
                }

                // Validate new header and extract fields
                let caps = self
                    .header_regex
                    .captures(line)
                    .ok_or_else(|| anyhow::anyhow!("Invalid hunk header: '{}'", line.trim()))?;
                base_start = caps[1].parse()?;
                base_count = match caps.get(2) {
                    Some(m) => m.as_str().strip_prefix(',').unwrap_or(m.as_str()).parse()?,
                    None => 1,
                };
                remote_start = caps[3].parse()?;
                remote_count = match caps.get(4) {
                    Some(m) => m.as_str().strip_prefix(',').unwrap_or(m.as_str()).parse()?,
                    None => 1,
                };

                // Capture the header text after the @@ ... @@
                // pattern, preserving the newline
                let header_start = caps.get(0).unwrap().end();
                header = line[header_start..].to_string();
                in_hunk = true;
            } else if in_hunk && !line.starts_with(r"\ No newline at end of file") {
                body_lines.push(line.to_string());
            }
        }

        // Save last hunk
        if in_hunk {
            hunks.push(finalize_hunk(
                header.clone(),
                body_lines.clone(),
                base_start,
                base_count,
                remote_start,
                remote_count,
            )?);
        }

        Ok(hunks)
    }

    fn is_conflict(
        &self,
        hunk: &Hunk,
        minus_lines: &[String],
        minus_lines_hasher: &Sha256,
    ) -> Result<bool> {
        let mut hasher = minus_lines_hasher.clone();
        hasher.update(Self::MAX_RELOCATION_DISTANCE.to_string());
        let patch = hunk.body.join("");
        hasher.update(patch.len().to_string());
        hasher.update(patch);
        let hash = hasher.finalize();
        let hash = format!("{:x}", hash);

        if let Some(cache) = self.lmdb_cache.as_deref()
            && let Some(success) = cache.get_cached_minus_lines(&hash)
        {
            return Ok(success);
        }

        let total_iterations = minus_lines.len();
        let success = if total_iterations > 0 {
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(total_iterations)
                .max(1);
            let chunk_size = total_iterations.div_ceil(num_threads);
            let found = AtomicBool::new(false);

            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(num_threads);
                for chunk_idx in 0..num_threads {
                    let start_idx = chunk_idx * chunk_size;
                    if start_idx >= total_iterations {
                        break;
                    }
                    let end_idx = (start_idx + chunk_size).min(total_iterations);
                    let found_ref = &found;

                    let handle = s.spawn(move || {
                        minus_lines[start_idx..end_idx].iter().any(|minus_line| {
                            if found_ref.load(Relaxed) {
                                return true;
                            }
                            hunk.body.iter().any(|x| {
                                if let Some(stripped) = x.strip_prefix('+') {
                                    let stripped = stripped.trim();
                                    if stripped.is_empty() {
                                        return false;
                                    }
                                    let distance = nstr::levenshtein(stripped, minus_line);
                                    if distance <= Self::MAX_RELOCATION_DISTANCE {
                                        found_ref.store(true, Relaxed);
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            })
                        })
                    });
                    handles.push(handle);
                }

                handles.into_iter().any(|handle| handle.join().unwrap())
            })
        } else {
            false
        };

        if let Some(cache) = self.lmdb_cache.as_deref() {
            cache.cache_minus_lines(&hash, success)?;
        }

        Ok(success)
    }

    fn ranges_overlap(
        start1: usize,
        end1: usize,
        start2: usize,
        end2: usize,
        allow_adjacent: bool,
    ) -> bool {
        (start1 < end2 && start2 < end1)
            || start1 == start2
            || end1 == end2
            || (!allow_adjacent && start1 <= end2 && start2 <= end1)
    }

    fn log_conflict_hunk(msg: &str, conflict: &Conflict, hunk: &Hunk) {
        if log::log_enabled!(log::Level::Debug) {
            let (hunk_base_start, hunk_base_end, hunk_remote_start, hunk_remote_end) =
                hunk.get_ranges();
            log::debug!(
                "{}: {:?}, local=[{},{}), base=[{},{}), remote=[{},{}), new_local=[{},{}), hunk: base=[{},{}), remote=[{},{})",
                msg,
                conflict.commit_type,
                conflict.local_start,
                conflict.local_end,
                conflict.base_start,
                conflict.base_end,
                conflict.remote_start,
                conflict.remote_end,
                conflict.new_local_start,
                conflict.new_local_end,
                hunk_base_start,
                hunk_base_end,
                hunk_remote_start,
                hunk_remote_end
            );
        }
    }

    fn process_clean_hunk_single(
        &self,
        conflicts: &mut Vec<Conflict>,
        hunk: &Hunk,
        minus_lines: &[String],
        minus_lines_hasher: &Sha256,
    ) -> Result<()> {
        if hunk.body.len() > Self::MAX_BODY_LEN {
            return Ok(());
        }

        let base = hunk.extract_base_snippet()?;
        if base.is_empty() {
            return Ok(());
        }
        let remote = hunk.extract_remote_snippet()?;
        if remote.is_empty() {
            return Ok(());
        }

        let mut commit_type = CommitType::Clean;
        if !self.is_conflict(hunk, minus_lines, minus_lines_hasher)? {
            commit_type = CommitType::CleanDisabled;
        }

        let local = &self.local_content;
        let local_b = local.as_bytes();
        let merged_local = &self.merged_local_content;
        let merged_local_b = merged_local.as_bytes();

        if let FastSearchResult::Found(offsets) = fast_search(remote.as_bytes(), merged_local_b, 1)
            && let FastSearchResult::Found(_) = fast_search(base.as_bytes(), local_b, 1)
        {
            let local_start =
                bytes_to_lines(merged_local_b, offsets[0]) + hunk.get_head_context().len();
            let local_end = local_start + hunk.get_conflict_remote()?.len();

            let extra_conflict_lines = self.context_lines.extra_conflict_lines as usize;
            let new_local_start = local_start.saturating_sub(extra_conflict_lines);
            let new_local_end = local_end
                .saturating_add(extra_conflict_lines)
                .min(self.merged_local_lines.len());

            let conflict_code = hunk.get_conflict_base()?.join("");

            let hunks = vec![hunk.clone()];

            let conflict = Conflict {
                file_path: conflicts[0].file_path.clone(),
                conflict_code,
                local_start,
                local_end,
                new_local_start,
                new_local_end,
                commit_type,
                hunks,
                merged_local_lines: self.merged_local_lines.clone(),
                ..Default::default()
            };

            // Check if the new conflict overlaps with any existing conflict
            // Also check for duplicate start or end positions to avoid
            // infinite empty ranges adjacent to non-empty ones.
            let overlaps = conflicts.iter().any(|existing_conflict| {
                Self::ranges_overlap(
                    conflict.local_start,
                    conflict.local_end,
                    existing_conflict.local_start,
                    existing_conflict.local_end,
                    true,
                )
            });

            if overlaps {
                log::warn!(
                    "Skipping clean hunk conflict at lines {}-{} because it overlaps with an existing conflict",
                    conflict.local_start,
                    conflict.local_end
                );
            } else {
                log::debug!("Clean: {hunk:?}");
                Self::log_conflict_hunk("Clean hunk", &conflict, hunk);
                conflicts.push(conflict);
            }
        }

        Ok(())
    }

    fn convert_clean_hunk_offsets(&self, hunks: &mut [Hunk], conflicts: &[Conflict]) {
        let mut clean_base_line = 0;
        let mut clean_remote_line = 0;

        for hunk in hunks {
            let head_context_len = hunk.get_head_context().len();
            let ml_line = if hunk.remote_len == 0 {
                assert_eq!(head_context_len, 0);
                hunk.remote_start
            } else {
                hunk.remote_start - 1 + head_context_len
            };
            let mut local_line = clean_remote_line;
            let mut base_line = ml_line + clean_base_line;
            let mut remote_line = ml_line + clean_remote_line;

            for conflict in conflicts {
                if conflict.commit_type != CommitType::Conflict {
                    continue;
                }
                if ml_line >= conflict.local_end {
                    let local_len = conflict.local_end - conflict.local_start;
                    let base_len = conflict.base_end - conflict.base_start;
                    let remote_len = conflict.remote_end - conflict.remote_start;

                    local_line += local_len;
                    base_line += base_len;
                    remote_line += remote_len;
                } else {
                    break;
                }
            }

            hunk.base_start = base_line - local_line + 1 - head_context_len;
            hunk.remote_start = remote_line - local_line + 1 - head_context_len;

            clean_base_line += hunk.base_len;
            clean_remote_line += hunk.remote_len;
        }
    }

    fn process_clean_hunks(
        &self,
        conflicts: &mut Vec<Conflict>,
        pre_relocation_conflicts: &[Conflict],
        hunks: Vec<Hunk>,
    ) -> Result<()> {
        conflicts.sort_by_key(|c| c.local_start);

        // Build local snippets
        let mut minus_lines_hasher = Sha256::new();
        let minus_lines = self.create_minus_lines(conflicts, &mut minus_lines_hasher);

        // Split hunks into smaller snippets and flatten them
        let patch_context_lines = self.context_lines.patch_context_lines as usize;
        let mut splitted_hunks: Vec<Hunk> = Vec::with_capacity(hunks.len());
        for h in hunks {
            splitted_hunks.extend(h.split(1, patch_context_lines)?);
        }

        self.convert_clean_hunk_offsets(&mut splitted_hunks, pre_relocation_conflicts);
        for hunk in splitted_hunks {
            self.process_clean_hunk_single(conflicts, &hunk, &minus_lines, &minus_lines_hasher)?;
        }

        Ok(())
    }

    fn match_conflicting_hunk_single(&self, conflicts: &mut [Conflict], hunk: &Hunk) -> Result<()> {
        log::debug!("Conflict: {hunk:?}");
        let (hunk_base_start, hunk_base_end, hunk_remote_start, hunk_remote_end) =
            hunk.get_ranges();

        let mut matched = false;
        for conflict in conflicts.iter_mut() {
            // Check base range overlap: [conflict.base_start,
            // conflict.base_end) vs [hunk_base_start, hunk_base_end)
            let base_overlaps = Self::ranges_overlap(
                conflict.base_start,
                conflict.base_end,
                hunk_base_start,
                hunk_base_end,
                true,
            );

            if log::log_enabled!(log::Level::Trace) {
                Self::log_conflict_hunk("Conflict candidate", conflict, hunk);
            }

            // Check remote range overlap: [conflict.remote_start,
            // conflict.remote_end) vs [hunk_remote_start,
            // hunk_remote_end)
            let remote_overlaps = Self::ranges_overlap(
                conflict.remote_start,
                conflict.remote_end,
                hunk_remote_start,
                hunk_remote_end,
                true,
            );

            if !base_overlaps && !remote_overlaps {
                continue;
            }

            Self::log_conflict_hunk("Conflict match", conflict, hunk);

            // Add the hunk to the conflict's hunks
            conflict.hunks.push(hunk.clone());
            matched = true;
        }

        if !matched {
            anyhow::bail!(
                "Hunk not matched to any conflict in file '{}', hunk: base=[{},{}), remote=[{},{})",
                conflicts[0].file_path,
                hunk_base_start,
                hunk_base_end,
                hunk_remote_start,
                hunk_remote_end
            );
        }

        Ok(())
    }

    fn match_conflicting_hunks(&self, conflicts: &mut [Conflict], hunks: Vec<Hunk>) -> Result<()> {
        // Split hunks into smaller snippets and flatten them
        let patch_context_lines = self.context_lines.patch_context_lines as usize;
        let mut splitted_conflicts_hunks: Vec<Hunk> = Vec::with_capacity(hunks.len());
        for h in hunks {
            splitted_conflicts_hunks.extend(h.split(1, patch_context_lines)?);
        }

        for hunk in &splitted_conflicts_hunks {
            self.match_conflicting_hunk_single(conflicts, hunk)?;
        }

        Ok(())
    }

    fn merge_conflict_code(&self, current: &mut Conflict, next: &Conflict) {
        if current.commit_type.is_clean() && next.commit_type.is_clean() {
            let gap = current.local_end..next.local_start;
            for i in gap {
                current.conflict_code.push_str(&self.merged_local_lines[i]);
            }
            current.conflict_code.push_str(&next.conflict_code);
            current.local_end = next.local_end;
        } else if current.commit_type.is_clean() && next.commit_type.is_conflict() {
            current.conflict_code = next.conflict_code.clone();
            current.local_start = next.local_start;
            current.local_end = next.local_end;
        }
    }

    fn merge_conflicts(&self, conflicts: &mut Vec<Conflict>) -> Result<()> {
        conflicts.sort_by_key(|c| c.local_start);

        loop {
            let mut merged: Vec<Conflict> = Vec::with_capacity(conflicts.len());

            // Start with the first conflict
            let mut current = &mut conflicts.remove(0);

            let mut repeat = false;
            for next in &mut *conflicts {
                let same_hunk = || next.hunks.iter().any(|hunk| current.hunks.contains(hunk));

                let within_lines_and_no_intermediate_clean_hunk = || {
                    (next.new_local_start >= current.new_local_end
                        && (next.new_local_start - current.new_local_end)
                            <= Self::EXTEND_IF_WITHIN_LINES)
                        && current.commit_type.is_conflict()
                        && next.commit_type.is_conflict()
                        && current
                            .hunks
                            .iter()
                            .all(|h1| next.hunks.iter().all(|h2| h1.base_start != h2.base_start))
                };

                if current.new_local_end >= next.new_local_start
                    || same_hunk()
                    || within_lines_and_no_intermediate_clean_hunk()
                {
                    repeat = true;
                    log::debug!(
                        "Merging {:?} [{},{}) - {:?} [{},{})",
                        current.commit_type,
                        current.new_local_start,
                        current.new_local_end,
                        next.commit_type,
                        next.new_local_start,
                        next.new_local_end,
                    );

                    if current.local_end > next.local_start {
                        anyhow::bail!(
                            "Invalid conflict merge: current local_end ({}) > next local_start ({}) in {}",
                            current.local_end,
                            next.local_start,
                            current.file_path
                        );
                    }

                    if current.commit_type.is_conflict() && next.commit_type.is_conflict() {
                        current.start_line = current.start_line.min(next.start_line);
                    } else if next.commit_type.is_conflict() {
                        current.start_line = next.start_line;
                    }

                    self.merge_conflict_code(current, next);

                    if current.commit_type.is_clean() && next.commit_type.is_conflict() {
                        current.conflict_raw_patch =
                            Some(next.conflict_raw_patch.as_ref().unwrap().clone());

                        if !Self::EXTEND_CONFLICT_ON_CLEAN_MERGE {
                            current.new_local_start = next.new_local_start;
                            current.new_local_end = next.new_local_end;
                        }

                        current.hunks.clear();
                    } else if current.commit_type.is_conflict() && next.commit_type.is_clean() {
                        if !Self::EXTEND_CONFLICT_ON_CLEAN_MERGE {
                            next.new_local_start = current.new_local_start;
                            next.new_local_end = current.new_local_end;
                        }

                        next.hunks.clear();
                    } else if current.commit_type.is_conflict() && next.commit_type.is_conflict() {
                        current
                            .conflict_raw_patch
                            .as_mut()
                            .unwrap()
                            .push_str(next.conflict_raw_patch.as_ref().unwrap());
                    }

                    // Merge hunks from next into current
                    for hunk in &next.hunks {
                        if !current.hunks.contains(hunk) {
                            current.hunks.push(hunk.clone());
                        }
                    }

                    current.new_local_start = next.new_local_start.min(current.new_local_start);
                    current.new_local_end = next.new_local_end.max(current.new_local_end);
                    current.commit_type = (current.commit_type + next.commit_type)?;

                    log::debug!(
                        "Merged [{},{}) {:?}",
                        current.new_local_start,
                        current.new_local_end,
                        current.commit_type,
                    );
                } else {
                    // No overlap, finalize current and start new
                    merged.push(current.clone());
                    current = next;
                }
            }

            // Push the last conflict
            merged.push(current.clone());

            *conflicts = merged;

            if !repeat {
                break;
            }
        }
        conflicts.retain(|c| c.commit_type != CommitType::CleanDisabled);

        Ok(())
    }

    fn relocate_conflicts(
        &self,
        conflicts: &mut [Conflict],
        prev_conflicts: &[ResolvedConflict],
    ) -> Result<()> {
        let extra_conflict_lines = self.context_lines.extra_conflict_lines as usize;
        let code_context_lines = self.context_lines.code_context_lines as usize;

        conflicts.sort_by_key(|c| c.local_start);
        for conflict in conflicts.iter() {
            log::debug!(
                "pre relocate conflicts: {:?} [{},{})",
                conflict.commit_type,
                conflict.local_start,
                conflict.local_end
            );
        }

        let mut restart = false;
        for i in 0..conflicts.len() {
            let conflicts_tmp = conflicts.to_vec();
            let conflict = &mut conflicts[i];
            // Only Conflicts can be fully relocated
            // Clean commit types can only be extended
            assert!(conflict.commit_type == CommitType::Conflict);
            assert!(conflict.local_end >= conflict.local_start);

            conflict.conflict_relocation = self.conflict_relocation;
            for prev_conflict in prev_conflicts {
                if prev_conflict.conflict.file_path == conflict.file_path
                    && conflict
                        .hunks
                        .iter()
                        .all(|hunk| prev_conflict.conflict.hunks.contains(hunk))
                {
                    conflict.conflict_relocation = prev_conflict.conflict.conflict_relocation;
                }
            }
            let markers_context_lines = if !conflict.conflict_relocation {
                Self::MISPLACED_CONTEXT_LINES
            } else {
                Self::MARKERS_CONTEXT_LINES
            };

            let hunks = self.diff_to_hunks(conflict.conflict_raw_patch.as_ref().unwrap(), false)?;
            assert_eq!(hunks.len(), 1);

            let hunks: Vec<Hunk> = hunks
                .first()
                .unwrap()
                .split(code_context_lines, usize::MAX)?;

            let prev_local_end = if i > 0 {
                conflicts_tmp[i - 1].local_end
            } else {
                0
            };
            let next_local_start = if i + 1 < conflicts_tmp.len() {
                conflicts_tmp[i + 1].local_start
            } else {
                self.merged_local_lines.len()
            };

            let orig_local_start = conflict.local_start;
            let orig_local_end = conflict.local_end;

            let mut min_local_start = usize::MAX;
            let mut max_local_end = 0;

            // Lowest relocation distance found in this pass, it will
            // be enforced only in the next passes
            let mut relocation_distance = conflict.relocation_distance;

            assert!(!hunks.is_empty());

            for hunk in &hunks {
                let mut temp_conflict = conflict.clone();
                temp_conflict.conflict_raw_patch = Some(hunk.to_string());

                let head_context = hunk.get_head_context();
                let tail_context = hunk.get_tail_context();

                let head = head_context.len();
                let tail = tail_context.len();

                let head_margin = (temp_conflict.local_start - prev_local_end).saturating_sub(1);
                let tail_margin = (next_local_start - temp_conflict.local_end).saturating_sub(1);

                let adjusted_prev_local_end = prev_local_end
                    .saturating_sub(head)
                    .max(temp_conflict.local_start.saturating_sub(Self::MAX_SCAN));
                let adjusted_next_local_start = next_local_start
                    .saturating_add(tail)
                    .min(temp_conflict.local_end.saturating_add(Self::MAX_SCAN))
                    .min(self.merged_local_lines.len());

                let head_scan_range = temp_conflict.local_start - adjusted_prev_local_end;
                let tail_scan_range = adjusted_next_local_start - temp_conflict.local_end;

                let mut head_found = head < markers_context_lines;
                let mut tail_found = tail < markers_context_lines;
                //(head_found, tail_found) = (true, true);
                if !head_found || !tail_found {
                    if !head_found && !tail_found {
                        let orig_start = temp_conflict.local_start;
                        let orig_end = temp_conflict.local_end;
                        let (start, end) = if hunks.len() == 1 {
                            (
                                orig_start.saturating_sub(Self::MAX_SCAN),
                                orig_end
                                    .saturating_add(Self::MAX_SCAN)
                                    .min(self.merged_local_lines.len()),
                            )
                        } else {
                            (adjusted_prev_local_end, adjusted_next_local_start)
                        };
                        let found = self.relocate_both(
                            &mut temp_conflict,
                            (start, end),
                            &head_context,
                            &tail_context,
                            hunk.get_conflict_base()?
                                .len()
                                .min(hunk.get_conflict_remote()?.len()),
                            hunk.get_conflict_base()?
                                .len()
                                .max(hunk.get_conflict_remote()?.len()),
                        )?;

                        if let Some(d) = temp_conflict.relocation_distance {
                            relocation_distance =
                                Some(relocation_distance.map_or(d, |r: f64| r.min(d)));
                        }

                        if found {
                            let out_of_bounds = temp_conflict.local_start < prev_local_end
                                || temp_conflict.local_end > next_local_start;

                            if out_of_bounds {
                                let mut overlaps = false;
                                for (j, other_conflict) in conflicts_tmp.iter().enumerate() {
                                    if j == i {
                                        continue;
                                    }
                                    if Self::ranges_overlap(
                                        temp_conflict.local_start,
                                        temp_conflict.local_end,
                                        other_conflict.local_start,
                                        other_conflict.local_end,
                                        true,
                                    ) {
                                        overlaps = true;
                                        break;
                                    }
                                }

                                if overlaps {
                                    temp_conflict.local_start = orig_start;
                                    temp_conflict.local_end = orig_end;
                                } else {
                                    head_found = true;
                                    tail_found = true;
                                    restart = true;
                                }
                            } else {
                                head_found = true;
                                tail_found = true;
                            }
                        }
                    }
                    if !head_found && head_margin > 0 && head_scan_range > head {
                        let local_start = temp_conflict.local_start;
                        head_found = self.relocate_head(
                            &mut temp_conflict,
                            adjusted_prev_local_end,
                            local_start,
                            &head_context,
                        )?;
                    }
                    if !tail_found && tail_margin > 0 && tail_scan_range > tail {
                        let local_end = temp_conflict.local_end;
                        tail_found = self.relocate_tail(
                            &mut temp_conflict,
                            local_end,
                            adjusted_next_local_start,
                            &tail_context,
                        )?;
                    }
                }

                if !restart {
                    if (!head_found && head > 0) || conflict.conflict_relocation {
                        let start = temp_conflict
                            .local_end
                            .saturating_sub(Self::MAX_BASE_SCAN)
                            .max(prev_local_end);
                        let end = temp_conflict.local_end;
                        if !self.relocate_base(&mut temp_conflict, start..end, true)? {
                            self.relocate_remote(&mut temp_conflict, start..end, true)?;
                        }
                    }
                    if (!tail_found && tail > 0) || conflict.conflict_relocation {
                        let start = temp_conflict.local_start;
                        let end = temp_conflict
                            .local_start
                            .saturating_add(Self::MAX_BASE_SCAN)
                            .min(self.merged_local_lines.len())
                            .min(next_local_start);
                        if !self.relocate_base(&mut temp_conflict, start..end, false)? {
                            self.relocate_remote(&mut temp_conflict, start..end, false)?;
                        }
                    }
                }

                min_local_start = min_local_start.min(temp_conflict.local_start);
                max_local_end = max_local_end.max(temp_conflict.local_end);

                if restart {
                    break;
                }
            }

            conflict.relocation_distance = relocation_distance;
            self.update_conflict_code(conflict, min_local_start, max_local_end);

            if restart {
                log::debug!(
                    "Relocated conflict out of order in {}:{}: [{},{}) -> [{},{}) distance {:?}",
                    conflict.file_path,
                    conflict.start_line,
                    orig_local_start,
                    orig_local_end,
                    conflict.local_start,
                    conflict.local_end,
                    conflict.relocation_distance
                );
                log::debug!("Conflicts out of order, sorting and relocating");
                return self.relocate_conflicts(conflicts, prev_conflicts);
            }

            let head_margin = (conflict.local_start - prev_local_end).saturating_sub(1);
            let tail_margin = (next_local_start - conflict.local_end).saturating_sub(1);
            let extra_head = code_context_lines.min(head_margin) + extra_conflict_lines;
            let extra_tail = code_context_lines.min(tail_margin) + extra_conflict_lines;

            if conflict.local_start != orig_local_start || conflict.local_end != orig_local_end {
                log::debug!(
                    "Relocated conflict in {}:{}: [{},{}) -> [{},{})",
                    conflict.file_path,
                    conflict.start_line,
                    orig_local_start,
                    orig_local_end,
                    conflict.local_start,
                    conflict.local_end
                );
            }

            conflict.new_local_start = conflict.local_start.saturating_sub(extra_head);
            conflict.new_local_end = conflict
                .local_end
                .saturating_add(extra_tail)
                .min(self.merged_local_lines.len());

            let is_out_of_order = (i > 0 && conflict.local_start < conflicts_tmp[i - 1].local_end)
                || (i + 1 < conflicts_tmp.len()
                    && conflict.local_end > conflicts_tmp[i + 1].local_start);
            assert!(!is_out_of_order);
        }

        for conflict in conflicts.iter() {
            log::debug!(
                "post relocate conflicts: {:?} [{},{})",
                conflict.commit_type,
                conflict.local_start,
                conflict.local_end
            );
        }
        Ok(())
    }

    fn calc_max_context_distance(&self, conflict: &Conflict, context: &[String]) -> f64 {
        assert!(!context.is_empty());
        if conflict.local_start != conflict.local_end {
            0.75
        } else {
            0.9
        }
    }

    fn calc_distances(
        &self,
        head_context: &[String],
        range: std::ops::Range<usize>,
        (anchored, tail, max_context_distance): (bool, bool, f64),
    ) -> Result<Option<(Vec<f64>, usize)>> {
        let cache = self.lmdb_cache.as_deref();
        let merged_local_lines = if tail {
            &self.merged_local_lines[range.clone()]
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
        } else {
            &self.merged_local_lines[range.clone()]
        };
        let head_context = if tail {
            &head_context.iter().rev().cloned().collect::<Vec<_>>()
        } else {
            head_context
        };
        let head_context_str = &head_context.join("");

        let mut cache_key = String::new();
        if let Some(cache) = cache {
            let merged_local_lines_str = &merged_local_lines.join("");
            cache_key = cache.get_cache_key(&[
                head_context_str.as_bytes(),
                merged_local_lines_str.as_bytes(),
                format!("{:?}", tail).as_bytes(),
                format!("{:?}", anchored).as_bytes(),
                format!("{:?}", max_context_distance).as_bytes(),
            ]);
            if let Some(cached) = cache.get_cached_response(&cache_key) {
                let result: Option<(Vec<f64>, usize)> = serde_json::from_str(&cached)
                    .map_err(|e| anyhow::anyhow!("Failed to parse cached distance: {}", e))?;
                return Ok(result);
            }
        }

        let head_context_len = head_context.len();
        let mut min_distance = f64::MAX;
        let mut offset = usize::MAX;

        let total_iterations = (range.len() + 1).saturating_sub(head_context_len);
        let mut distances = Vec::with_capacity(total_iterations);

        if total_iterations > 0 {
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(total_iterations)
                .max(1);
            log::trace!("scaling levenshtein search over {} threads", num_threads);
            let chunk_size = total_iterations.div_ceil(num_threads);

            std::thread::scope(|s| {
                let mut handles = Vec::with_capacity(num_threads);
                for chunk_idx in 0..num_threads {
                    let start_idx = chunk_idx * chunk_size;
                    if start_idx >= total_iterations {
                        break;
                    }
                    let end_idx = (start_idx + chunk_size).min(total_iterations);

                    let handle = s.spawn(move || {
                        let mut local_distances = Vec::with_capacity(end_idx - start_idx);
                        let mut local_min_distance = f64::MAX;
                        let mut local_offset = usize::MAX;

                        for i in start_idx..end_idx {
                            let end = if !anchored {
                                i + head_context_len
                            } else {
                                merged_local_lines.len()
                            };
                            let candidate = &merged_local_lines[i..end];
                            if !anchored {
                                assert_eq!(candidate.len(), head_context_len);
                            } else {
                                assert!(candidate.len() >= head_context_len);
                            }
                            let candidate = candidate.join("");
                            let distance = nstr::levenshtein(&candidate, head_context_str);
                            if distance > max_context_distance {
                                local_distances.push(f64::MAX);
                                continue;
                            }
                            local_distances.push(distance);
                            if distance < local_min_distance {
                                local_offset = i;
                                local_min_distance = distance;
                            }
                        }
                        (local_distances, local_min_distance, local_offset)
                    });
                    handles.push(handle);
                }

                for handle in handles {
                    let (local_distances, local_min_distance, local_offset) =
                        handle.join().unwrap();
                    distances.extend(local_distances);
                    if local_min_distance < min_distance {
                        min_distance = local_min_distance;
                        offset = local_offset;
                    }
                }
            });
        }

        let result = if offset == usize::MAX {
            None
        } else {
            let distances = if tail {
                distances.iter().rev().cloned().collect::<Vec<_>>()
            } else {
                distances
            };
            assert_eq!(
                distances.len(),
                (range.len() + 1).saturating_sub(head_context_len)
            );
            let offset = offset + head_context_len;
            let offset = if tail { range.len() - offset } else { offset };

            Some((distances, offset))
        };

        if let Some(cache) = cache {
            let serialized = serde_json::to_string(&result)
                .map_err(|e| anyhow::anyhow!("Failed to serialize distance: {}", e))?;
            cache.cache_response(&cache_key, serialized)?;
        }

        Ok(result)
    }

    /// Ignore the calc_distances results that are not strictly lower
    /// than the lowest distance found by the previous relocation
    /// passes, and track the lowest distance found in this pass.
    fn filter_distances(
        result: Option<(Vec<f64>, usize)>,
        prev_distance: f64,
        min_distance: &mut f64,
    ) -> Option<(Vec<f64>, usize)> {
        let (mut distances, offset) = result?;
        let best = distances.iter().copied().fold(f64::MAX, f64::min);
        if best >= prev_distance {
            return None;
        }
        *min_distance = min_distance.min(best);
        for d in distances.iter_mut() {
            if *d >= prev_distance {
                *d = f64::MAX;
            }
        }
        Some((distances, offset))
    }

    fn update_conflict_code(&self, conflict: &mut Conflict, start: usize, end: usize) {
        conflict.local_start = start;
        conflict.local_end = end;
        conflict.conflict_code = self.merged_local_lines[start..end].join("");
    }

    fn relocate_both(
        &self,
        conflict: &mut Conflict,
        (prev_local_end, next_local_start): (usize, usize),
        head_context: &[String],
        tail_context: &[String],
        expected_lines_min: usize,
        expected_lines_max: usize,
    ) -> Result<bool> {
        if expected_lines_max == 0 {
            return Err(anyhow::anyhow!(
                "Relocation failed: expected_lines is zero for conflict in {} at [{},{})",
                conflict.file_path,
                conflict.local_start,
                conflict.local_end
            ));
        }

        // Distances above the lowest distance found in the previous
        // passes are ignored to avoid relocation loops
        let prev_distance = conflict.relocation_distance.unwrap_or(f64::MAX);
        let mut min_distance = f64::MAX;

        let max_context_distance = self.calc_max_context_distance(conflict, head_context);
        let head_result = Self::filter_distances(
            self.calc_distances(
                head_context,
                prev_local_end..next_local_start,
                (false, false, max_context_distance),
            )?,
            prev_distance,
            &mut min_distance,
        );
        let (head_distances, offset) = match head_result {
            Some((d, o)) => (Some(d), o),
            None => {
                log::debug!(
                    "Relocation not found head for both in {} [{},{}): head_context_len={}, scan_range={}",
                    conflict.file_path,
                    conflict.local_start,
                    conflict.local_end,
                    head_context.len(),
                    next_local_start - prev_local_end
                );
                (None, conflict.local_start - prev_local_end)
            }
        };
        let mut head_offset = offset + prev_local_end;
        let max_context_distance = self.calc_max_context_distance(conflict, tail_context);
        let tail_result = Self::filter_distances(
            self.calc_distances(
                tail_context,
                prev_local_end..next_local_start,
                (false, true, max_context_distance),
            )?,
            prev_distance,
            &mut min_distance,
        );
        let (tail_distances, offset) = match tail_result {
            Some((d, o)) => (Some(d), o),
            None => {
                log::debug!(
                    "Relocation not found tail for both in {} [{},{}): tail_context_len={}, scan_range={}",
                    conflict.file_path,
                    conflict.local_start,
                    conflict.local_end,
                    tail_context.len(),
                    next_local_start - prev_local_end
                );
                (None, conflict.local_end - prev_local_end)
            }
        };
        let mut tail_offset = offset + prev_local_end;

        if min_distance < f64::MAX {
            conflict.relocation_distance = Some(prev_distance.min(min_distance));
        }

        let head_context_len = head_context.len();
        if let Some(head_distances) = head_distances
            && let Some(tail_distances) = tail_distances
            && head_offset > tail_offset
        {
            let mut min_sum = f64::MAX;
            let mut best_head = 0;
            let mut best_tail = 0;
            for (i, &hd) in head_distances.iter().enumerate() {
                if hd == f64::MAX {
                    continue;
                }
                for (j, &td) in tail_distances.iter().enumerate() {
                    if td == f64::MAX {
                        continue;
                    }
                    let head_idx = prev_local_end + head_context_len + i;
                    let tail_idx = prev_local_end + j;
                    if head_idx <= tail_idx
                        && tail_idx - head_idx
                            <= expected_lines_max * Self::RELOCATE_BOTH_LENGTH_FACTOR
                        && tail_idx - head_idx
                            >= expected_lines_min / Self::RELOCATE_BOTH_LENGTH_FACTOR
                    {
                        let sum = hd + td;
                        if sum < min_sum {
                            min_sum = sum;
                            best_head = head_idx;
                            best_tail = tail_idx;
                        }
                    }
                }
            }
            if min_sum < f64::MAX {
                head_offset = best_head;
                tail_offset = best_tail;
            }
        }
        if head_offset <= tail_offset {
            self.update_conflict_code(conflict, head_offset, tail_offset);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn relocate_head(
        &self,
        conflict: &mut Conflict,
        prev_local_end: usize,
        next_local_start: usize,
        context: &[String],
    ) -> Result<bool> {
        let max_context_distance = self.calc_max_context_distance(conflict, context);
        let Some((_, offset)) = self.calc_distances(
            context,
            prev_local_end..next_local_start,
            (true, false, max_context_distance),
        )?
        else {
            log::debug!(
                "Relocation not found head in {} [{},{}): context_len={}, scan_range={}",
                conflict.file_path,
                conflict.local_start,
                conflict.local_end,
                context.len(),
                next_local_start - prev_local_end
            );
            return Ok(false);
        };
        let offset = prev_local_end + offset;
        if offset < conflict.local_start {
            self.update_conflict_code(conflict, offset, conflict.local_end);
            return Ok(true);
        }
        Ok(false)
    }

    fn relocate_tail(
        &self,
        conflict: &mut Conflict,
        prev_local_end: usize,
        next_local_start: usize,
        context: &[String],
    ) -> Result<bool> {
        let max_context_distance = self.calc_max_context_distance(conflict, context);
        let Some((_, offset)) = self.calc_distances(
            context,
            prev_local_end..next_local_start,
            (true, true, max_context_distance),
        )?
        else {
            log::debug!(
                "Relocation not found tail in {} [{},{}): context_len={}, scan_range={}",
                conflict.file_path,
                conflict.local_start,
                conflict.local_end,
                context.len(),
                next_local_start - prev_local_end
            );
            return Ok(false);
        };
        let offset = prev_local_end + offset;
        if offset > conflict.local_end {
            self.update_conflict_code(conflict, conflict.local_start, offset);
            return Ok(true);
        }
        Ok(false)
    }

    fn relocate_base(
        &self,
        conflict: &mut Conflict,
        range: std::ops::Range<usize>,
        reverse: bool,
    ) -> Result<bool> {
        let hunks = self.diff_to_hunks(conflict.conflict_raw_patch.as_ref().unwrap(), false)?;
        let mut lines = Vec::new();
        for hunk in &hunks {
            lines.extend(hunk.get_conflict_base()?);
        }
        if lines.is_empty() {
            return Ok(false);
        }
        let max_context_distance = Self::MAX_BASE_DISTANCE;
        let Some((_, offset)) = self.calc_distances(
            &lines,
            range.clone(),
            (false, reverse, max_context_distance),
        )?
        else {
            return Ok(false);
        };
        let offset = range.start + offset;
        if reverse {
            if offset < conflict.local_start {
                self.update_conflict_code(conflict, offset, conflict.local_end);
                return Ok(true);
            }
        } else if offset > conflict.local_end {
            self.update_conflict_code(conflict, conflict.local_start, offset);
            return Ok(true);
        }
        Ok(false)
    }

    fn relocate_remote(
        &self,
        conflict: &mut Conflict,
        range: std::ops::Range<usize>,
        reverse: bool,
    ) -> Result<bool> {
        let hunks = self.diff_to_hunks(conflict.conflict_raw_patch.as_ref().unwrap(), false)?;
        let mut lines = Vec::new();
        for hunk in &hunks {
            lines.extend(hunk.get_conflict_remote()?);
        }
        if lines.is_empty() {
            return Ok(false);
        }
        let max_context_distance = Self::MAX_BASE_DISTANCE / 2.;
        let Some((_, offset)) = self.calc_distances(
            &lines,
            range.clone(),
            (false, reverse, max_context_distance),
        )?
        else {
            return Ok(false);
        };
        let offset = range.start + offset;
        if reverse {
            if offset < conflict.local_start {
                self.update_conflict_code(conflict, offset, conflict.local_end);
                return Ok(true);
            }
        } else if offset > conflict.local_end {
            self.update_conflict_code(conflict, conflict.local_start, offset);
            return Ok(true);
        }
        Ok(false)
    }

    fn regenerate_conflict(
        &self,
        conflict: &mut Conflict,
        prev_new_local_end: usize,
        next_new_local_start: usize,
    ) -> Result<()> {
        let code_context_lines = self.context_lines.code_context_lines as usize;
        let merged_local_lines = &self.merged_local_lines;

        log::debug!(
            "Regenerating conflict: {} {} {} {} {:?}",
            conflict.new_local_start,
            conflict.new_local_end,
            conflict.local_start,
            conflict.local_end,
            conflict.commit_type
        );

        // Recalculate head context
        let head_end = conflict.new_local_start;
        let head_start = head_end
            .saturating_sub(code_context_lines)
            .max(prev_new_local_end);
        conflict.head_context = merged_local_lines[head_start..head_end].join("");
        conflict.nr_head_context_lines = head_end - head_start;

        // Recalculate tail context
        let tail_start = conflict.new_local_end;
        let tail_end = tail_start
            .saturating_add(code_context_lines)
            .min(next_new_local_start);
        conflict.tail_context = merged_local_lines[tail_start..tail_end].join("");
        conflict.nr_tail_context_lines = tail_end - tail_start;

        conflict.conflict_code = format!(
            "{}{}{}",
            merged_local_lines[head_end..conflict.local_start].join(""),
            conflict.conflict_code,
            merged_local_lines[conflict.local_end..tail_start].join("")
        );

        conflict.local_start = conflict.new_local_start;
        conflict.local_end = conflict.new_local_end;

        self.generate_conflict_patch(conflict)?;

        log::debug!(
            "Regenerated conflict: {} {}",
            conflict.local_start,
            conflict.local_end,
        );

        conflict.new_local_start = usize::MAX;
        conflict.new_local_end = usize::MAX - 1;
        conflict.base_start = usize::MAX;
        conflict.base_end = usize::MAX - 1;
        conflict.remote_start = usize::MAX;
        conflict.remote_end = usize::MAX - 1;
        conflict.nr_conflict_lines = usize::MAX;
        conflict.marker_size = usize::MAX;

        Ok(())
    }

    pub fn patch_locator(
        &self,
        conflicts: &mut Vec<Conflict>,
        prev_conflicts: &[ResolvedConflict],
    ) -> Result<()> {
        assert!(!conflicts.is_empty());
        let hunks = self.diff_to_hunks(&self.conflict_diff, false)?;
        if hunks.is_empty() {
            anyhow::bail!("conflict_diff contains no hunks");
        }
        self.match_conflicting_hunks(conflicts, hunks)?;

        conflicts.sort_by_key(|c| c.local_start);
        let pre_relocation_conflicts = conflicts.clone();

        self.relocate_conflicts(conflicts, prev_conflicts)?;

        let hunks = self.diff_to_hunks(&self.clean_diff, true)?;
        if !hunks.is_empty() {
            self.process_clean_hunks(conflicts, &pre_relocation_conflicts, hunks)?;
        }

        let code_snippets = Arc::new(self.create_code_snippets(conflicts));

        self.merge_conflicts(conflicts)?;

        // Merge the hunks associated with each conflict
        for conflict in conflicts.iter_mut() {
            let hunks = std::mem::take(&mut conflict.hunks);
            conflict.hunks = Hunk::merge_overlapping_hunks(hunks)?;
        }

        let mut prev_new_local_end = 0;
        for i in 0..conflicts.len() {
            let next_new_local_start = if i + 1 < conflicts.len() {
                conflicts[i + 1].new_local_start
            } else {
                self.merged_local_lines.len()
            };
            let conflict = &mut conflicts[i];
            if conflict.commit_type == CommitType::CleanDisabled {
                anyhow::bail!("Unexpected CleanDisabled conflicts after merge");
            }
            conflict.code_snippets = code_snippets.clone();
            let next_prev_new_local_end = conflict.new_local_end;
            self.regenerate_conflict(conflict, prev_new_local_end, next_new_local_start)?;
            prev_new_local_end = next_prev_new_local_end;
        }

        self.validate_conflicts(conflicts, self.merged_local_lines.len())?;

        Ok(())
    }

    fn validate_conflicts(&self, conflicts: &[Conflict], total_lines: usize) -> Result<()> {
        for (i, conflict) in conflicts.iter().enumerate() {
            if conflict.local_start > conflict.local_end {
                anyhow::bail!(
                    "Conflict invalid bounds in {}: local_start {} > local_end {}",
                    conflict.file_path,
                    conflict.local_start,
                    conflict.local_end
                );
            }

            // Check for underflow/overflow against total_lines
            if conflict.local_end > total_lines {
                anyhow::bail!(
                    "Conflict underflow/overflow in {}: local_end {} > total_lines {}",
                    conflict.file_path,
                    conflict.local_end,
                    total_lines
                );
            }

            // Check head context bounds
            if conflict.nr_head_context_lines > conflict.local_start {
                anyhow::bail!(
                    "Conflict head context overflow in {}: nr_head_context_lines {} > local_start {}",
                    conflict.file_path,
                    conflict.nr_head_context_lines,
                    conflict.local_start
                );
            }

            // Check tail context bounds
            if conflict.nr_tail_context_lines > (total_lines - conflict.local_end) {
                anyhow::bail!(
                    "Conflict tail context overflow in {}: nr_tail_context_lines {} > (total_lines - local_end) {}",
                    conflict.file_path,
                    conflict.nr_tail_context_lines,
                    total_lines - conflict.local_end
                );
            }

            if conflict.nr_head_context_lines == 0 && conflict.local_start > 0 {
                anyhow::bail!(
                    "Conflict head context underflow in {}: nr_head_context_lines is 0 but local_start {} > 0",
                    conflict.file_path,
                    conflict.local_start
                );
            }
            if conflict.nr_tail_context_lines == 0 && conflict.local_end < total_lines {
                anyhow::bail!(
                    "Conflict tail context underflow in {}: nr_tail_context_lines is 0 but local_end {} < total_lines {}",
                    conflict.file_path,
                    conflict.local_end,
                    total_lines
                );
            }

            // Check head context does not cross previous conflict's local_end
            if i > 0 {
                let prev = &conflicts[i - 1];
                let head_start = conflict.local_start - conflict.nr_head_context_lines;
                if head_start < prev.local_end {
                    anyhow::bail!(
                        "Conflict head context in {} crosses previous conflict at local_start {}, local_end {}, nr_head_context_lines {}",
                        conflict.file_path,
                        prev.local_end,
                        conflict.local_start,
                        conflict.nr_head_context_lines
                    );
                }

                // If local_start == local_end, ensure previous conflict's local_start < local_start
                if conflict.local_start == conflict.local_end
                    && prev.local_start >= conflict.local_start
                {
                    anyhow::bail!(
                        "Conflict local_start == local_end in {} but previous conflict local_start {} >= local_start {}",
                        conflict.file_path,
                        prev.local_start,
                        conflict.local_start
                    );
                }
            }

            // Check tail context does not cross next conflict's local_start
            if i + 1 < conflicts.len() {
                let next = &conflicts[i + 1];
                let tail_end = conflict.local_end + conflict.nr_tail_context_lines;
                if tail_end > next.local_start {
                    anyhow::bail!(
                        "Conflict tail context in {} crosses next conflict at local_end {}, local_start {}, nr_tail_context_lines {}",
                        conflict.file_path,
                        conflict.local_end,
                        next.local_start,
                        conflict.nr_tail_context_lines
                    );
                }

                // If local_start == local_end, ensure next conflict's local_end > local_start
                if conflict.local_start == conflict.local_end
                    && next.local_end <= conflict.local_start
                {
                    anyhow::bail!(
                        "Conflict local_start == local_end in {} but next conflict local_end {} <= local_start {}",
                        conflict.file_path,
                        next.local_end,
                        conflict.local_start
                    );
                }
            }
        }
        Ok(())
    }

    fn generate_conflict_patch(&self, conflict: &mut Conflict) -> Result<()> {
        // Verify each conflict has at least one entry in conflict.hunks and join them
        if conflict.hunks.is_empty() {
            //return Ok(());
            anyhow::bail!(
                "Conflict has no hunks in {} base: [{},{}), remote: [{}-{})",
                conflict.file_path,
                conflict.base_start,
                conflict.base_end,
                conflict.remote_start,
                conflict.remote_end
            );
        }
        conflict.hunks.sort_by_key(|h| h.base_start);
        conflict.conflict_patch = conflict
            .hunks
            .iter()
            .map(|h| h.to_string())
            .collect::<String>();
        Ok(())
    }

    fn create_code_snippets(&self, conflicts: &mut [Conflict]) -> Vec<Snippet> {
        conflicts.sort_by_key(|c| c.local_start);

        let cl = &self.context_lines;
        //let extra_lines = (cl.code_context_lines + cl.extra_conflict_lines) as usize;
        let extra_lines = cl.code_context_lines as usize;
        //let extra_lines = 0;
        let max_context_size = self.max_context_size as usize;
        let mut snippets: Vec<Snippet> = Vec::new();
        let mut last_end = 0;
        for (i, conflict) in conflicts.iter().enumerate() {
            if conflict.commit_type.is_clean() {
                continue;
            }

            let prev_local_end = if i > 0 && conflicts[i - 1].commit_type.is_clean() {
                conflicts[i - 1].local_end
            } else {
                0
            };
            let next_local_start =
                if i + 1 < conflicts.len() && conflicts[i + 1].commit_type.is_clean() {
                    conflicts[i + 1].local_start
                } else {
                    self.merged_local_lines.len()
                };

            let mut snippet = String::new();
            let local_start = conflict.local_start;
            let local_end = conflict.local_end;
            assert!(local_start <= local_end);
            if local_start == local_end {
                continue;
            }
            let mut start = local_start.saturating_sub(extra_lines).max(prev_local_end);
            let end = local_end.saturating_add(extra_lines).min(next_local_start);
            if !snippets.is_empty() {
                if last_end >= end {
                    continue;
                }
                if start <= last_end {
                    assert!(end >= last_end);
                    start = start.max(last_end);
                    snippet = snippets.pop().unwrap().snippet;
                }
            }
            last_end = end;

            for line in &self.merged_local_lines[start..end] {
                snippet.push_str(line);
            }
            snippets.push(Snippet {
                snippet,
                local_start: start,
                local_end: end,
            });
        }

        if snippets
            .iter()
            .map(|s| &s.snippet)
            .cloned()
            .collect::<String>()
            .len()
            > max_context_size
        {
            log::warn!(
                "Code snippets exceed max context size ({} bytes), skipping",
                max_context_size
            );
            snippets.clear();
        }

        snippets
    }

    fn create_minus_lines(&self, conflicts: &Vec<Conflict>, hasher: &mut Sha256) -> Vec<String> {
        let mut minus = Vec::new();
        for conflict in conflicts {
            if conflict.commit_type != CommitType::Conflict {
                continue;
            }
            let conflict_patch = &conflict.conflict_patch;
            for line in conflict_patch.lines() {
                if let Some(stripped) = line.strip_prefix('-') {
                    let stripped = stripped.trim();
                    if stripped.is_empty() {
                        continue;
                    }
                    minus.push(stripped.to_string());
                    hasher.update(stripped.len().to_string());
                    hasher.update(stripped);
                }
            }
        }
        minus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify_bytes_to_lines_and_lines_to_bytes(lines: Vec<&str>) {
        // Create a byte array with various bytes but also adding some b'\n' bytes
        let mut bytes = Vec::new();
        for line in &lines {
            bytes.extend_from_slice(line.as_bytes());
        }

        // Verify bytes_to_lines for every char
        for (i, _) in bytes.iter().enumerate() {
            let line_num = bytes_to_lines(&bytes, i);
            println!("{i} -> {line_num}");
            // The line number should be the count of newlines up to
            // and including position i
            let expected_line = bytes[..i].iter().filter(|&&b| b == b'\n').count();
            assert_eq!(line_num, expected_line, "Failed at byte index {}", i);
        }

        // Verify lines_to_bytes for every line
        for (i, line) in lines.iter().enumerate() {
            let byte_offset = _lines_to_bytes(&bytes, i);
            println!("{i} <- {byte_offset}");
            // The byte offset should be the sum of lengths of all
            // previous lines plus the newline
            let expected_offset = lines[..i].iter().map(|l| l.len()).sum::<usize>();
            assert_eq!(byte_offset, expected_offset, "Failed for line {}", i);

            // Verify that the line at this offset matches
            let line_bytes = line.as_bytes();
            if byte_offset + line_bytes.len() <= bytes.len() {
                assert_eq!(
                    &bytes[byte_offset..byte_offset + line_bytes.len()],
                    line_bytes,
                    "Failed for line {}",
                    i
                );
            }
        }

        // Test with a value beyond the end of the lines
        let beyond_lines = lines.len();
        let offset_beyond_lines = _lines_to_bytes(&bytes, beyond_lines);
        // Should return the last byte offset (end of bytes)
        assert_eq!(
            offset_beyond_lines,
            bytes.len(),
            "Beyond lines should return end of bytes"
        );

        // Test with a value beyond the end of the bytes
        let beyond_bytes = bytes.len() + 100;
        let offset_beyond_bytes = bytes_to_lines(&bytes, beyond_bytes);
        // Should return the last line number (count of newlines in entire bytes)
        let last_line_num = bytes.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(
            offset_beyond_bytes, last_line_num,
            "Beyond bytes should return last line number"
        );
    }

    #[test]
    fn test_bytes_to_lines_and_lines_to_bytes() {
        let all_lines = vec![
            vec!["a"],
            vec!["a\n"],
            vec!["a\n", "bc\n"],
            vec!["a\n", "bc\n", "\n"],
            vec!["a\n", "bc\n", "\n", "\n"],
            vec!["a\n", "bc\n", "de"],
            vec!["a\n", "b"],
            vec!["abcd\n", "abc\n", "abcde\n", "\n", "\n", "ab\n", "a"],
            vec!["abcd\n", "abc\n", "abcde\n", "\n", "\n", "ab\n", "a\n"],
            vec![
                "abcd\n", "abc\n", "abcde\n", "\n", "\n", "ab\n", "a\n", "abc",
            ],
        ];
        for lines in all_lines {
            verify_bytes_to_lines_and_lines_to_bytes(lines);
        }
        //panic!();
    }

    fn test_locator() -> PatchLocator {
        PatchLocator::new(
            Arc::new(String::new()),
            Arc::new(String::new()),
            Arc::new(Vec::new()),
            Arc::new(String::new()),
            Arc::new(String::new()),
            None,
            ContextLines {
                code_context_lines: 3,
                diff_context_lines: 3,
                patch_context_lines: 3,
                extra_conflict_lines: 0,
            },
            200000,
            false,
        )
    }

    fn parse_hunks(diff: &str, clean: bool) -> Vec<Hunk> {
        let locator = test_locator();
        locator.diff_to_hunks(diff, clean).unwrap()
    }

    #[test]
    fn test_merge_overlapping_hunks_preserves_header() {
        let hunks = parse_hunks(
            "@@ -1,3 +1,3 @@ function @@ suffix\n a\n-b\n+B\n c\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n",
            false,
        );
        let merged = Hunk::merge_overlapping_hunks(hunks).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].to_string(),
            "@@ -1,5 +1,5 @@ function @@ suffix\n a\n-b\n+B\n c\n-d\n+D\n e\n"
        );
    }

    #[test]
    fn test_merge_overlapping_hunks_keeps_adjacent() {
        let hunks = parse_hunks("@@ -1 +1 @@\n-a\n+A\n@@ -2 +2 @@\n-b\n+B\n", false);
        assert_eq!(Hunk::merge_overlapping_hunks(hunks.clone()).unwrap(), hunks);
    }

    #[test]
    fn test_merge_overlapping_hunks_rejects_incompatible() {
        let hunks = parse_hunks("@@ -1 +1 @@\n-a\n+A\n@@ -1 +1 @@\n-a\n+X\n", false);
        assert!(Hunk::merge_overlapping_hunks(hunks).is_err());
    }

    #[test]
    fn test_merge_overlapping_hunks_partial_context_overlap() {
        let hunks = parse_hunks(
            "@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n@@ -4,4 +4,4 @@\n d\n e\n-f\n+F\n g\n",
            false,
        );
        let merged = Hunk::merge_overlapping_hunks(hunks).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].to_string(),
            "@@ -1,7 +1,7 @@\n a\n-b\n+B\n c\n d\n e\n-f\n+F\n g\n"
        );
    }

    #[test]
    fn test_merge_overlapping_hunks_multiple_context_lines() {
        let hunks = parse_hunks(
            "@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n@@ -3,4 +3,4 @@\n c\n d\n-e\n+E\n f\n",
            false,
        );
        let merged = Hunk::merge_overlapping_hunks(hunks).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].to_string(),
            "@@ -1,6 +1,6 @@\n a\n-b\n+B\n c\n d\n-e\n+E\n f\n"
        );
    }

    #[test]
    fn test_merge_overlapping_hunks_different_base_and_remote_lengths() {
        let hunks = parse_hunks(
            "@@ -1,3 +1,4 @@\n a\n-b\n+B\n+BB\n c\n@@ -3,3 +4,2 @@\n c\n-d\n e\n",
            false,
        );
        let merged = Hunk::merge_overlapping_hunks(hunks).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].to_string(),
            "@@ -1,5 +1,5 @@\n a\n-b\n+B\n+BB\n c\n-d\n e\n"
        );
    }

    #[test]
    fn test_merge_overlapping_hunks_rejects_incompatible_context() {
        for diff in [
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -3,3 +3,3 @@\n x\n-d\n+D\n e\n",
            "@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\n@@ -3,4 +4,4 @@\n c\n d\n-e\n+E\n f\n",
        ] {
            let hunks = parse_hunks(diff, false);
            assert!(Hunk::merge_overlapping_hunks(hunks).is_err());
        }
    }

    #[test]
    fn test_merge_overlapping_hunks_rejects_overlap_outside_context() {
        for diff in [
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -2,3 +2,3 @@\n b\n c\n-d\n+D\n",
            "@@ -1,3 +1,3 @@\n-a\n+A\n b\n c\n@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n",
        ] {
            let hunks = parse_hunks(diff, false);
            assert!(Hunk::merge_overlapping_hunks(hunks).is_err());
        }
    }

    #[test]
    fn test_merge_overlapping_hunks_keeps_different_clean_flags() {
        let mut hunks = parse_hunks(
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n",
            false,
        );
        hunks[1].clean = true;
        assert_eq!(Hunk::merge_overlapping_hunks(hunks.clone()).unwrap(), hunks);
    }

    #[test]
    fn test_merge_overlapping_hunks_chain_in_reverse_order() {
        let mut hunks = parse_hunks(
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n@@ -5,3 +5,3 @@\n e\n-f\n+F\n g\n",
            false,
        );
        hunks.reverse();
        let merged = Hunk::merge_overlapping_hunks(hunks).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].to_string(),
            "@@ -1,7 +1,7 @@\n a\n-b\n+B\n c\n-d\n+D\n e\n-f\n+F\n g\n"
        );
    }

    #[test]
    fn test_merge_overlapping_hunks_rejects_incompatible_intermediate_hunk() {
        let hunks = parse_hunks(
            "@@ -1,3 +1,3 @@\n a\n b\n c\n@@ -2 +2 @@\n-b\n+X\n@@ -3,2 +3,2 @@\n c\n d\n",
            false,
        );
        assert!(Hunk::merge_overlapping_hunks(hunks).is_err());
    }

    #[test]
    fn test_shared_hunks_survive_until_conflict_merging() {
        let locator = test_locator();
        let hunks = parse_hunks(
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -3,3 +3,3 @@\n c\n-d\n+D\n e\n",
            false,
        );
        let mut conflicts = vec![
            Conflict {
                local_start: 1,
                local_end: 2,
                new_local_start: 1,
                new_local_end: 2,
                conflict_raw_patch: Some(String::new()),
                hunks: hunks.clone(),
                ..Default::default()
            },
            Conflict {
                local_start: 20,
                local_end: 21,
                new_local_start: 20,
                new_local_end: 21,
                conflict_raw_patch: Some(String::new()),
                hunks: vec![hunks[1].clone()],
                ..Default::default()
            },
        ];
        locator.merge_conflicts(&mut conflicts).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].hunks.len(), 2);
        let hunks = std::mem::take(&mut conflicts[0].hunks);
        conflicts[0].hunks = Hunk::merge_overlapping_hunks(hunks).unwrap();
        locator.generate_conflict_patch(&mut conflicts[0]).unwrap();
        assert_eq!(
            conflicts[0].conflict_patch,
            "@@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n-d\n+D\n e\n"
        );
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
