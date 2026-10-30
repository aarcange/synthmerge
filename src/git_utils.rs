// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use crate::conflict_resolver::{CommitType, Conflict, ConflictResolver, ResolvedConflict};
use crate::git_command::GitCommand;
use crate::lmdb_cache::{LmdbCacheImpl, PatchLocatorCache};
use crate::patch_locator::PatchLocator;
use crate::prob;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::fs::{OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub struct ContextLines {
    pub code_context_lines: u32,
    pub diff_context_lines: u32,
    pub patch_context_lines: u32,
    pub extra_conflict_lines: u32,
}

#[derive(Debug, Clone)]
pub struct OperationHead {
    pub file: String,
    pub command: String,
    pub path: PathBuf,
}

/// Remove conflict markers from content
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConflictMarkerMode {
    Local,
    Base,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    Interactive,
    VibeWithPatchLocator,
    VibeWithMarkers,
}

#[derive(Debug, Clone, Copy, Default)]
struct ConflictOffsets {
    local: usize,
    base: usize,
    remote: usize,
}

pub struct GitUtils {
    context_lines: ContextLines,
    in_rebase: bool,
    git_root: String,
    git_dir: String,
    blob_cache: HashMap<String, Arc<String>>,
    lmdb_cache: Option<Arc<LmdbCacheImpl>>,
    resolution_mode: ResolutionMode,
    pub(crate) retries: usize,
    max_retries: usize,
    continue_op: bool,
    allow_empty: bool,
    assisted: bool,
    cherry_pick_x_warned: bool,
    assisted_conflicts: Vec<ResolvedConflict>,
    resolved_files: HashSet<String>,
    unresolved_files: HashSet<String>,
    unresolved_clean_files: HashSet<String>,
    unresolved_added_files: HashSet<String>,
    unresolved_deleted_files: HashSet<String>,
    assisted_by_line: String,
    synthmerge_name: String,
}

impl GitUtils {
    const REBASE_MESSAGE_FILE: &str = "rebase-merge/message";
    const MERGE_MSG_FILE: &str = "MERGE_MSG";

    const DEFAULT_MARKER_SIZE: usize = 7;

    pub fn new(
        context_lines: ContextLines,
        cache_path: Option<String>,
        cache_overwrite: bool,
        resolution_mode: ResolutionMode,
        max_retries: usize,
        continue_op: bool,
        allow_empty: bool,
    ) -> Self {
        let git_root = Self::get_git_root_uncached().expect("Failed to get git root directory");
        let git_dir = Self::get_git_dir_uncached().expect("Failed to get git directory");
        let lmdb_cache = cache_path.map(|path| {
            Arc::new(
                PatchLocatorCache::create_from_path(&path, cache_overwrite)
                    .expect("Failed to create API cache"),
            )
        });
        let synthmerge_name = env!("CARGO_PKG_NAME")
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .take(1)
            .chain(env!("CARGO_PKG_NAME").chars().skip(1))
            .collect::<String>();
        GitUtils {
            context_lines,
            in_rebase: false,
            git_root,
            git_dir,
            blob_cache: HashMap::new(),
            lmdb_cache,
            resolution_mode,
            retries: 0,
            max_retries,
            continue_op,
            allow_empty,
            assisted: false,
            cherry_pick_x_warned: false,
            assisted_conflicts: Vec::new(),
            resolved_files: HashSet::new(),
            unresolved_files: HashSet::new(),
            unresolved_clean_files: HashSet::new(),
            unresolved_added_files: HashSet::new(),
            unresolved_deleted_files: HashSet::new(),
            assisted_by_line: format!("Assisted-by: {}", synthmerge_name),
            synthmerge_name,
        }
    }

    /// Reset context lines to their original values after successful resolution
    pub fn restore_context_lines(&mut self, original: &ContextLines) {
        self.context_lines.code_context_lines = original.code_context_lines;
        self.context_lines.diff_context_lines = original.diff_context_lines;
        self.context_lines.patch_context_lines = original.patch_context_lines;
        self.context_lines.extra_conflict_lines = original.extra_conflict_lines;
    }

    /// Retry logic: decrement retries and adjust context lines based
    /// on resolution mode
    pub fn can_retry(&mut self) -> bool {
        if self.retries >= self.max_retries {
            return false;
        }

        self.retries += 1;
        let enable_relocation = self.retries == 1;

        let extra: u32 = if self.retries > self.max_retries / 2 {
            self.retries.try_into().unwrap()
        } else {
            1
        };

        match self.resolution_mode {
            ResolutionMode::VibeWithMarkers => {
                if self.context_lines.code_context_lines == 0 {
                    return false;
                }
                println!(
                    "Retrying resolution with reduced --code-context-lines ({} -> {})",
                    self.context_lines.code_context_lines,
                    self.context_lines.code_context_lines.saturating_sub(1)
                );
                self.context_lines.code_context_lines =
                    self.context_lines.code_context_lines.saturating_sub(1);

                true
            }
            ResolutionMode::VibeWithPatchLocator => {
                if enable_relocation {
                    println!("Retrying resolution with conflict relocation");
                    return true;
                }
                println!(
                    "Retrying resolution with increased --extra-conflict-lines ({} -> {})",
                    self.context_lines.extra_conflict_lines,
                    self.context_lines
                        .extra_conflict_lines
                        .saturating_add(extra)
                );
                self.context_lines.extra_conflict_lines = self
                    .context_lines
                    .extra_conflict_lines
                    .saturating_add(extra);

                true
            }
            ResolutionMode::Interactive => {
                println!(
                    "You can retry with --vibe or with reduced --code-context-lines ({} -> {})",
                    self.context_lines.code_context_lines,
                    self.context_lines.code_context_lines.saturating_sub(1)
                );

                false
            }
        }
    }

    /// Run git status --porcelain=v2 -z
    fn git_status_porcelain_v2(&self, path: Option<&str>) -> Result<std::process::Output> {
        let git_root = &self.git_root;
        let mut args = vec!["-C", git_root, "status", "--porcelain=v2", "-z"];
        if let Some(p) = path {
            args.push("--");
            args.push(p);
        }
        GitCommand::new("git")
            .args(&args)
            .output()
            .context("Failed to execute git status --porcelain=v2 -z")
    }

    /// Check that git cherry-pick default is diff3 for merge.conflictStyle
    pub fn check_diff3(&self) -> Result<()> {
        let output = GitCommand::new("git")
            .args(["config", "--get", "merge.conflictStyle"])
            .output()
            .context("Failed to get git config")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Failed to get merge.conflictStyle: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let config_value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if config_value != "diff3" {
            return Err(anyhow::anyhow!(
                "merge.conflictStyle is not set to 'diff3', it is set to '{}'",
                config_value
            ));
        }

        Ok(())
    }

    /// Find all conflict markers in the repository
    pub fn find_conflicts(
        &mut self,
        max_context_size: u32,
        prev_conflicts: &[ResolvedConflict],
    ) -> Result<BTreeMap<String, Vec<Conflict>>> {
        // Run git status --porcelain=v2 -z to get blob hashes
        let output = self.git_status_porcelain_v2(None)?;

        // Parse the status output to find the unmerged entry for this file
        let status_output_bytes = &output.stdout;
        let lines = status_output_bytes.split(|&b| b == b'\0').peekable();

        let mut all_conflicts: BTreeMap<String, Vec<Conflict>> = BTreeMap::new();
        for line_bytes in lines {
            let line = String::from_utf8_lossy(line_bytes);
            // Skip header lines
            // if line.starts_with('#') {
            //     continue;
            // }

            // Parse unmerged entries (format: u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>)
            if line.starts_with("u UU") {
                let parts: Vec<&str> = line.splitn(11, ' ').collect();
                if parts.len() >= 11 {
                    // let (base_blob, remote_blob) = (parts[7].to_string(), parts[9].to_string());
                    let (local_blob, file_path) = (parts[8].to_string(), parts[10].to_string());

                    let marker_size = self.get_marker_size_for_file(&file_path)?;

                    let path = Path::new(&self.git_root).join(&file_path);
                    let merged_content = Arc::new(
                        fs::read_to_string(&path)
                            .context(format!("Failed to read file: {}", file_path))?,
                    );

                    let mut conflicts = self.parse_conflicts(&merged_content, marker_size)?;
                    if conflicts.is_empty() {
                        return Err(anyhow::anyhow!(
                            "No conflicts found in unmerged file: {}",
                            file_path
                        ));
                    }
                    for conflict in &mut conflicts {
                        conflict.file_path = file_path.to_string();
                        conflict.marker_size = marker_size;
                    }

                    // Get the blob contents
                    let Some(local_content) = self.get_blob_content_cached(&local_blob)? else {
                        log::warn!(
                            "Skipping conflicts in {}: local blob {} is empty",
                            file_path,
                            local_blob
                        );
                        continue;
                    };

                    // Compute diff between base and remote using git command
                    // let diff = GitCommand::new("git")
                    //     .args([
                    //         "diff",
                    //         "--pretty=",
                    //         "--no-color",
                    //         "--histogram",
                    //         &format!("-U{}", self.context_lines.patch_context_lines),
                    //         &base_blob.to_string(),
                    //         &remote_blob.to_string(),
                    //     ])
                    //     .output()
                    //     .context("Failed to execute git diff for blob")?;

                    // let diff = Arc::new(String::from_utf8_lossy(&diff.stdout).to_string());

                    let merged_content_lines: Vec<String> = merged_content
                        .split_inclusive('\n')
                        .map(|s| s.to_string())
                        .collect();

                    let remove_conflict_markers =
                        |mode: ConflictMarkerMode| -> Result<(Arc<Vec<String>>, Arc<String>)> {
                            let cleaned_lines: Vec<String> = Self::remove_conflict_markers(
                                &merged_content_lines
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>(),
                                marker_size,
                                mode,
                            )?
                            .iter()
                            .map(|s| s.to_string())
                            .collect();
                            let cleaned_content = Arc::new(cleaned_lines.join(""));
                            Ok((Arc::new(cleaned_lines), cleaned_content))
                        };

                    let (merged_local_lines, merged_local_content) =
                        remove_conflict_markers(ConflictMarkerMode::Local)?;
                    let (_, merged_base_content) =
                        remove_conflict_markers(ConflictMarkerMode::Base)?;
                    let (_, merged_remote_content) =
                        remove_conflict_markers(ConflictMarkerMode::Remote)?;

                    let clean_diff = Arc::new(ConflictResolver::create_diff(
                        &local_content,
                        &merged_local_content,
                        self.context_lines.patch_context_lines,
                    ));
                    let conflict_diff = if true {
                        Arc::new(ConflictResolver::create_diff(
                            &merged_base_content,
                            &merged_remote_content,
                            self.context_lines.patch_context_lines,
                        ))
                    } else {
                        let temp_dir = tempfile::Builder::new()
                            .permissions(Permissions::from_mode(0o700))
                            .prefix("synthmerge_")
                            .tempdir_in("/dev/shm")?;

                        let creat = &mut OpenOptions::new();
                        creat.read(true).write(true).create_new(true).mode(0o600);

                        let base_path = temp_dir.path().join("base");
                        let remote_path = temp_dir.path().join("remote");

                        let mut base = creat.open(&base_path)?;
                        let mut remote = creat.open(&remote_path)?;

                        std::io::Write::write_all(&mut base, merged_base_content.as_bytes())?;
                        std::io::Write::write_all(&mut remote, merged_remote_content.as_bytes())?;

                        let output = GitCommand::new("git")
                            .verbose(false)
                            .args([
                                "diff",
                                "--no-index",
                                "--histogram",
                                &format!("-U{}", self.context_lines.patch_context_lines),
                                base_path.to_str().unwrap(),
                                remote_path.to_str().unwrap(),
                            ])
                            .output()
                            .context("Failed to execute git diff for conflicts")?;

                        Arc::new(String::from_utf8_lossy(&output.stdout).to_string())
                    };

                    for conflict in &mut conflicts {
                        conflict.merged_local_lines = merged_local_lines.clone();
                    }

                    if self.resolution_mode == ResolutionMode::VibeWithPatchLocator {
                        let patch_locator = PatchLocator::new(
                            local_content,
                            merged_local_content,
                            merged_local_lines,
                            clean_diff,
                            conflict_diff,
                            self.lmdb_cache.clone(),
                            self.context_lines,
                            max_context_size,
                            self.retries > 0,
                        );
                        patch_locator.patch_locator(&mut conflicts, prev_conflicts)?;
                    }

                    // Replace solved conflicts with previous ones if hunks are identical
                    Self::replace_solved_conflicts(&mut conflicts, prev_conflicts);

                    all_conflicts.insert(file_path, conflicts);
                }
            }
        }

        Ok(all_conflicts)
    }

    /// Replace new conflicts with previous ones if their hunks are identical.
    /// This is used to avoid re-resolving conflicts that haven't changed.
    fn replace_solved_conflicts(conflicts: &mut [Conflict], prev_conflicts: &[ResolvedConflict]) {
        for conflict in conflicts.iter_mut() {
            if let Some(prev_conflict) = prev_conflicts.iter().find(|p| {
                p.conflict.file_path == conflict.file_path
                    && p.conflict.conflict_patch == conflict.conflict_patch
            }) {
                *conflict = prev_conflict.conflict.clone();
            }
        }
    }

    /// Get the content of a git blob, using cache if available.
    /// Returns None if the blob exists but it's empty.
    fn get_blob_content_cached(&mut self, blob_hash: &str) -> Result<Option<Arc<String>>> {
        if let Some(cached) = self.blob_cache.get(blob_hash) {
            return Ok(Some(cached.clone()));
        }

        let Some(content) = self.get_blob_content(blob_hash)? else {
            return Ok(None);
        };
        let content = Arc::new(content);
        self.blob_cache
            .insert(blob_hash.to_string(), content.clone());
        Ok(Some(content))
    }

    /// Get the content of a git blob.
    /// Returns None if the blob exists but it's empty.
    fn get_blob_content(&self, blob_hash: &str) -> Result<Option<String>> {
        let git_root = &self.git_root;
        let output = GitCommand::new("git")
            .args(["-C", git_root, "show", blob_hash])
            .output()
            .context("Failed to execute git show for blob")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git show for blob {} failed: {}",
                blob_hash,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let mut content = String::from_utf8_lossy(&output.stdout).to_string();

        // Check if the blob is empty
        if content.is_empty() {
            log::debug!("Blob {} is empty", blob_hash);
            return Ok(None);
        }

        // Check if the content is newline terminated
        if !content.ends_with('\n') {
            content.push('\n');
        }

        Ok(Some(content))
    }

    /// Create a regex pattern for matching conflict markers
    fn create_conflict_regex(marker_size: usize) -> Result<Regex> {
        Regex::new(&format!(
            r"(?ms)(^{}(?: .*?)?\n.*?^{}(?: .*?)?\n.*?^{}(?: .*?)?\n.*?^{}(?: .*?)?(?:\n|$))",
            Self::create_local_marker(marker_size),
            Self::create_base_marker(marker_size)
                .chars()
                .map(|c| format!(r"\{}", c))
                .collect::<String>(),
            Self::create_remote_marker(marker_size),
            Self::create_end_marker(marker_size),
        ))
        .map_err(|e| anyhow::anyhow!("Failed to create conflict regex: {}", e))
    }

    /// Parse conflicts from file content
    fn parse_conflicts(&self, content: &str, marker_size: usize) -> Result<Vec<Conflict>> {
        let mut conflicts = Vec::new();

        let re = Self::create_conflict_regex(marker_size)?;

        let mut offsets = ConflictOffsets::default();
        for cap in re.captures_iter(content) {
            let this_cap = cap.get(0).unwrap();
            let conflict_text = this_cap.as_str();
            let start_line = content[..this_cap.start()]
                .chars()
                .filter(|&c| c == '\n')
                .count();
            let conflict = self.parse_conflict_text(
                conflict_text,
                content,
                start_line,
                marker_size,
                &mut offsets,
            )?;
            conflicts.push(conflict);
        }

        Ok(conflicts)
    }

    fn gen_context(
        &self,
        conflict_lines: &[&str],
        content_lines: &[&str],
        start_line: usize,
        marker_size: usize,
        mode: ConflictMarkerMode,
    ) -> Result<(usize, usize, String, String)> {
        let head_context_end = start_line;
        let head_content_lines = &content_lines[..head_context_end].to_vec();

        const CONTEXT_BEYOND_MARKER: bool = false;
        let head_content_lines = if CONTEXT_BEYOND_MARKER {
            Self::remove_conflict_markers(head_content_lines, marker_size, mode)
        } else {
            Ok(head_content_lines
                .iter()
                .rev()
                .take_while(|&&x| !x.starts_with(&Self::create_end_marker(marker_size)))
                .cloned()
                .collect::<Vec<_>>()
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>())
        }?;
        let head_context_lines = head_content_lines[head_content_lines
            .len()
            .saturating_sub(self.context_lines.code_context_lines as usize)..]
            .to_vec();
        let nr_head_context_lines = head_context_lines.len();

        let tail_content_lines = &content_lines[start_line + conflict_lines.len()..];
        let tail_content_lines = if CONTEXT_BEYOND_MARKER {
            Self::remove_conflict_markers(tail_content_lines, marker_size, mode)
        } else {
            Ok(tail_content_lines
                .iter()
                .take_while(|&&x| {
                    !x.starts_with(&format!("{} ", Self::create_local_marker(marker_size)))
                })
                .cloned()
                .collect::<Vec<_>>())
        }?;
        let tail_context_lines = tail_content_lines[..tail_content_lines
            .len()
            .min(self.context_lines.code_context_lines as usize)]
            .to_vec();
        let nr_tail_context_lines = tail_context_lines.len();

        Ok((
            nr_head_context_lines,
            nr_tail_context_lines,
            head_context_lines.join(""),
            tail_context_lines.join(""),
        ))
    }

    pub fn create_diff_from_separated(
        head_context: &str,
        tail_context: &str,
        base: &str,
        remote: &str,
        patch_context_lines: u32,
    ) -> String {
        let base_with_context = format!("{}{}{}", head_context, base, tail_context);
        let remote_with_context = format!("{}{}{}", head_context, remote, tail_context);
        ConflictResolver::create_diff(
            &base_with_context,
            &remote_with_context,
            patch_context_lines,
        )
    }

    /// Parse a conflict block into structured data
    fn parse_conflict_text(
        &self,
        conflict_text: &str,
        content: &str,
        start_line: usize,
        marker_size: usize,
        offsets: &mut ConflictOffsets,
    ) -> Result<Conflict> {
        let conflict_lines: Vec<&str> = conflict_text.split_inclusive('\n').collect();

        let local_start = conflict_lines
            .iter()
            .position(|&line| {
                line.starts_with(&format!("{} ", Self::create_local_marker(marker_size)))
            })
            .context("Failed to find head marker")?;

        let base_start = conflict_lines
            .iter()
            .position(|&line| {
                line.starts_with(&format!("{} ", Self::create_base_marker(marker_size)))
            })
            .context("Failed to find base marker")?;

        let remote_start = conflict_lines
            .iter()
            .position(|&line| line == format!("{}\n", Self::create_remote_marker(marker_size)))
            .context("Failed to find conflict marker")?;

        let remote_end = conflict_lines
            .iter()
            .position(|&line| line.starts_with(&Self::create_end_marker(marker_size)))
            .context("Failed to find conflict end marker")?;

        let ai_start = conflict_lines
            .iter()
            .position(|&line| {
                line.starts_with(&format!("{} ", Self::create_ai_marker(marker_size)))
            })
            .unwrap_or(remote_end);

        if remote_end < ai_start
            || remote_end <= remote_start
            || remote_start <= base_start
            || base_start <= local_start
        {
            anyhow::bail!(
                "Invalid conflict markers: ai_start={}, remote_end={}, remote_start={}, base_start={}, local_start={}",
                ai_start,
                remote_end,
                remote_start,
                base_start,
                local_start
            );
        }
        let nr_conflict_lines = conflict_lines.len();
        if remote_end + 1 != nr_conflict_lines {
            anyhow::bail!(
                "Invalid conflict markers: remote_end + 1 ({}) != nr_conflict_lines ({})",
                remote_end + 1,
                nr_conflict_lines
            );
        }

        let local_lines: Vec<&str> = conflict_lines[local_start + 1..base_start].to_vec();
        let base_lines: Vec<&str> = conflict_lines[base_start + 1..remote_start].to_vec();
        let remote_lines: Vec<&str> = conflict_lines[remote_start + 1..ai_start].to_vec();

        let content_lines: Vec<&str> = content.split_inclusive('\n').collect();

        let (nr_head_context_lines, nr_tail_context_lines, head_context, tail_context) = self
            .gen_context(
                &conflict_lines,
                &content_lines,
                start_line,
                marker_size,
                ConflictMarkerMode::Local,
            )?;

        // local_start is the start_line minus the accumulated extra
        // lines from previous conflicts
        let local_start = start_line - offsets.local;
        // local_end is local_start plus the number of local code lines
        let local_end = local_start + local_lines.len();
        // Calculate the number of lines in the conflict that are NOT local code
        // These are the markers and the base/remote sections
        offsets.local += nr_conflict_lines - local_lines.len();

        // Calculate base and remote offsets
        let base_start = start_line - offsets.base;
        let base_end = base_start + base_lines.len();
        offsets.base += nr_conflict_lines - base_lines.len();
        let remote_start = start_line - offsets.remote;
        let remote_end = remote_start + remote_lines.len();
        offsets.remote += nr_conflict_lines - remote_lines.len();

        let conflict_code = local_lines.join("");

        let base = base_lines.join("");
        let remote = remote_lines.join("");
        let conflict_patch = Self::create_diff_from_separated(
            &head_context,
            &tail_context,
            &base,
            &remote,
            self.context_lines.patch_context_lines,
        );
        let conflict_raw_patch = Some(ConflictResolver::create_diff(&base, &remote, u32::MAX));

        Ok(Conflict {
            conflict_code,
            conflict_patch,
            conflict_raw_patch,
            head_context,
            tail_context,
            start_line,
            nr_conflict_lines, // append new AI results at the end
            local_start,
            local_end,
            base_start,
            base_end,
            remote_start,
            remote_end,
            nr_head_context_lines,
            nr_tail_context_lines,
            ..Default::default()
        })
    }

    /// Get the marker size for a specific file from gitattributes
    fn get_marker_size_for_file(&self, file_path: &str) -> Result<usize> {
        // Check if we can find the marker size in gitattributes for this file
        let output = GitCommand::new("git")
            .args([
                "-C",
                &self.git_root,
                "check-attr",
                "conflict-marker-size",
                "--",
                file_path,
            ])
            .output()
            .with_context(|| format!("Failed to execute git check-attr for file: {}", file_path))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Some(size_str) = line
                    .strip_prefix(format!("{}:", file_path).as_str())
                    .and_then(|s| s.trim().strip_prefix("conflict-marker-size: "))
                    && let Ok(size) = size_str.parse::<usize>()
                {
                    return Ok(size);
                }
            }
        }

        // Default to 7 if not found
        Ok(Self::DEFAULT_MARKER_SIZE)
    }

    /// Create a marker with specified size
    fn create_marker(marker_char: char, size: usize) -> String {
        marker_char.to_string().repeat(size)
    }

    /// Create a local marker with specified size
    fn create_local_marker(size: usize) -> String {
        Self::create_marker('<', size)
    }

    /// Create a base marker with specified size
    fn create_base_marker(size: usize) -> String {
        Self::create_marker('|', size)
    }

    /// Create a conflict marker with specified size
    fn create_remote_marker(size: usize) -> String {
        Self::create_marker('=', size)
    }

    /// Create a AI marker with specified size
    fn create_ai_marker(size: usize) -> String {
        Self::create_marker('&', size)
    }

    /// Create an end marker with specified size
    fn create_end_marker(size: usize) -> String {
        Self::create_marker('>', size)
    }

    fn remove_conflict_markers<'a>(
        content_lines: &[&'a str],
        marker_size: usize,
        mode: ConflictMarkerMode,
    ) -> Result<Vec<&'a str>> {
        let content_str = content_lines.join("");
        let re = Self::create_conflict_regex(marker_size)?;

        let mut skip_ranges = Vec::new();
        let mut current_byte = 0;
        let mut current_line = 0;

        for cap in re.captures_iter(&content_str) {
            let m = cap.get(0).unwrap();
            while current_byte < m.start() && current_line < content_lines.len() {
                current_byte += content_lines[current_line].len();
                current_line += 1;
            }
            let start_line = current_line;

            while current_byte < m.end() && current_line < content_lines.len() {
                current_byte += content_lines[current_line].len();
                current_line += 1;
            }
            let end_line = current_line;

            skip_ranges.push(start_line..end_line);
        }

        let mut in_region = false;
        let local_marker = Self::create_local_marker(marker_size);
        let base_marker = Self::create_base_marker(marker_size);
        let remote_marker = Self::create_remote_marker(marker_size);
        let ai_marker = Self::create_ai_marker(marker_size);
        let end_marker = Self::create_end_marker(marker_size);

        let is_marker = |line: &str, marker: &str| -> bool {
            line.starts_with(marker)
                && (line.len() == marker_size
                    || line.as_bytes()[marker_size] == b'\n'
                    || line.as_bytes()[marker_size] == b' ')
        };

        let mut range_idx = 0;

        let result: Vec<&str> = content_lines
            .iter()
            .enumerate()
            .filter(|(i, line)| {
                let i = *i;
                while range_idx < skip_ranges.len() && i >= skip_ranges[range_idx].end {
                    range_idx += 1;
                }

                let skip_lines =
                    range_idx < skip_ranges.len() && skip_ranges[range_idx].contains(&i);

                if skip_lines {
                    if is_marker(line, &local_marker) {
                        in_region = mode == ConflictMarkerMode::Local;
                        return false;
                    } else if is_marker(line, &base_marker) {
                        in_region = mode == ConflictMarkerMode::Base;
                        return false;
                    } else if is_marker(line, &remote_marker) {
                        in_region = mode == ConflictMarkerMode::Remote;
                        return false;
                    } else if is_marker(line, &ai_marker) || is_marker(line, &end_marker) {
                        in_region = false;
                        return false;
                    }
                    in_region
                } else {
                    true
                }
            })
            .map(|(_, line)| *line)
            .collect();

        // Check for nested conflict markers
        let result_str = result.join("");
        let re = Regex::new(&format!(
            r"(?ms)^<{{{},}}.*?^={{{},}}.*?^>{{{},}}",
            Self::DEFAULT_MARKER_SIZE,
            Self::DEFAULT_MARKER_SIZE,
            Self::DEFAULT_MARKER_SIZE,
        ))
        .unwrap();
        let has_nested_markers = re.is_match(&result_str);

        if has_nested_markers {
            log::error!("Nested conflict markers found in file");
        }
        Ok(result)
    }

    /// Apply resolved conflicts back to the repository
    pub fn apply_resolved_conflicts(&mut self, conflicts: &[ResolvedConflict]) -> Result<()> {
        let conflicts = Self::deduplicate_conflicts(conflicts);

        for conflict in conflicts.iter().rev() {
            println!(
                "Applying resolved conflict for: {}:{}->{} - {}",
                conflict.conflict.file_path,
                conflict.conflict.start_line,
                conflict.conflict.local_start,
                conflict.model
            );
            assert!(conflict.conflict.commit_type == CommitType::Conflict);

            // Read the file
            let path = Path::new(&self.git_root).join(&conflict.conflict.file_path);
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read file: {}", conflict.conflict.file_path))?;
            // Split content into lines
            let mut lines: Vec<String> = content
                .split_inclusive('\n')
                .map(|s| s.to_string())
                .collect();

            // Calculate the line where we want to insert the resolved content
            let insert_line =
                conflict.conflict.start_line + conflict.conflict.nr_conflict_lines - 1;

            // Get the marker size for this file from gitattributes
            let marker_size = conflict.conflict.marker_size;

            // Insert the resolved content with markers
            let marker_raw = format!("{} ", Self::create_ai_marker(marker_size));
            let marker = format!(
                "{}{}: {}{}\n",
                marker_raw,
                self.synthmerge_name,
                conflict.model,
                conflict
                    .logprob
                    .map(|p| format!(" {:.1}%", prob::logprob_to_prob(p)))
                    .unwrap_or_default(),
            );
            let current_line = &lines[insert_line];
            if !current_line.starts_with(&format!("{} ", Self::create_end_marker(marker_size)))
                && !current_line.starts_with(&marker_raw)
            {
                log::error!(
                    "Invalid conflict marker found at line {}\n{}",
                    insert_line,
                    current_line
                );
                continue;
            }
            lines.insert(insert_line, marker);
            let resolved_lines: Vec<String> = conflict
                .resolved_version
                .split_inclusive('\n')
                .map(|s| s.to_string())
                .collect();
            for (i, line) in resolved_lines.iter().enumerate() {
                lines.insert(insert_line + 1 + i, line.to_string());
            }

            // Write back to file
            fs::write(&path, lines.join("")).with_context(|| {
                format!("Failed to write file: {}", conflict.conflict.file_path)
            })?;
            self.resolved_files
                .insert(conflict.conflict.file_path.clone());
            self.assisted = true;
        }

        self.update_merge_message(false, false)?;

        Ok(())
    }

    /// Apply vibe resolution - fully resolve conflicts and update git index
    pub fn apply_vibe_resolution(
        &mut self,
        conflicts: &BTreeMap<String, Vec<Conflict>>,
        resolved_conflicts: &[ResolvedConflict],
        retry_files: &HashSet<String>,
    ) -> Result<bool> {
        let resolved_conflicts = Self::deduplicate_conflicts_vibe(resolved_conflicts);

        // if true {
        //     // if self.context_lines.extra_conflict_lines == 0 {
        //     //     resolved_conflicts.pop();
        //     // }
        //     resolved_conflicts.pop();
        // }

        let mut needs_retry = false;
        let mut recoverable = true;

        // Process each file
        for (file_path, file_conflicts) in conflicts {
            if self.retries < self.max_retries && retry_files.contains(file_path) {
                println!("Will retry file: {}", file_path);
                needs_retry = true;
                continue;
            }
            println!("Processing file: {}", file_path);

            // Sort conflicts by start line (ascending)
            let mut sorted_conflicts: Vec<&Conflict> = file_conflicts.iter().collect();
            sorted_conflicts.sort_by_key(|c| c.local_start);

            let updated_content =
                self.apply_vibe_resolution_to_file(&sorted_conflicts, &resolved_conflicts)?;

            if let Some(content) = updated_content {
                // Write back to file
                let path = Path::new(&self.git_root).join(file_path);

                fs::write(&path, content.join(""))
                    .with_context(|| format!("Failed to write file: {}", file_path))?;
                self.git_update_index(Some(file_path))?;
                self.assisted = true;
            } else {
                needs_retry = true;
                if !retry_files.contains(file_path) {
                    recoverable = false;
                }
            }
        }

        if needs_retry {
            if recoverable && self.can_retry() {
                return Ok(false);
            } else {
                self.update_merge_message(false, false)?;
                return Err(anyhow::anyhow!("Incomplete conflict resolution"));
            }
        } else {
            self.update_merge_message(false, true)?;
        }

        Ok(true)
    }

    /// Apply vibe resolution using conflict markers
    fn apply_vibe_resolution_to_file(
        &mut self,
        sorted_conflicts: &[&Conflict],
        resolved_conflicts: &[ResolvedConflict],
    ) -> Result<Option<Vec<String>>> {
        // Split content into lines
        let mut lines: Vec<String> = sorted_conflicts[0].merged_local_lines.as_ref().to_vec();

        // Process conflicts in reverse order to maintain correct line numbers
        for conflict in sorted_conflicts.iter().rev() {
            let file_path = &conflict.file_path;
            let resolved_conflict = resolved_conflicts.iter().find(|r| {
                r.conflict.file_path == *file_path && r.conflict.local_start == conflict.local_start
            });

            if resolved_conflict.is_none() {
                match conflict.commit_type {
                    CommitType::Clean => {
                        log::warn!(
                            "No resolved conflict found for Clean hunk: {}:{}->{}, skipping",
                            file_path,
                            conflict.start_line,
                            conflict.local_start
                        );
                        self.unresolved_clean_files.insert(file_path.to_string());
                        continue;
                    }
                    _ => {
                        log::error!(
                            "No resolved conflict found for: {}:{}->{}",
                            file_path,
                            conflict.start_line,
                            conflict.local_start
                        );
                        return Ok(None);
                    }
                }
            }

            let resolved_conflict = resolved_conflict.unwrap();

            println!(
                "Found vibe resolution for: {}:{}->{}",
                file_path, conflict.start_line, conflict.local_start
            );

            // Replace the entire conflict with the resolved version
            let resolved_lines: Vec<String> = resolved_conflict
                .resolved_version
                .split_inclusive('\n')
                .map(|s| s.to_string())
                .collect();

            // Replace the conflict
            lines.splice(conflict.local_start..conflict.local_end, resolved_lines);

            if resolved_conflict.no_change {
                self.unresolved_files.insert(file_path.to_string());
            } else {
                self.resolved_files.insert(file_path.to_string());
            }

            assert!(!resolved_conflict.deduplicated_conflicts.is_empty());
            for dedup in &resolved_conflict.deduplicated_conflicts {
                self.assisted_conflicts.push(dedup.clone());
            }
        }

        Ok(Some(lines))
    }

    /// Continue the current cherry-pick, rebase, revert, or merge operation
    pub fn continue_operation(&mut self, context_lines: &ContextLines) -> Result<bool> {
        let git_dir = self.git_dir.clone();

        // Check if we're in a cherry-pick, rebase, revert, or merge
        let operation = match self.find_operation_head(&git_dir)? {
            Some(op) => op,
            None => return Ok(false),
        };

        assert_eq!(self.in_rebase, operation.command == "rebase");

        // Always delete unmerged files if any before continuing
        self.git_add_delete_unmerged()?;

        // Check continue_op immediately after git_add_delete_unmerged so
        // unmerged files are handled even when not continuing the operation.
        if !self.continue_op {
            return Ok(false);
        }

        // Restore context lines before continuing
        self.restore_context_lines(context_lines);

        // Reset git_utils
        self.retries = 0;

        // Function to commit and continue operation
        let merge_msg_path = self.get_merge_msg_path(&git_dir);
        if !merge_msg_path.exists() {
            log::warn!(
                "message file not found: {}",
                merge_msg_path.to_string_lossy()
            );
            return Ok(false);
        }
        let content = std::fs::read_to_string(&merge_msg_path)?;
        let cleaned = content
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&merge_msg_path, cleaned)?;

        let before_head = std::fs::read_to_string(&operation.path)
            .with_context(|| format!("Failed to read {}", operation.file))?
            .trim()
            .to_string();

        // Commit the changes
        println!("Committing changes");
        let mut commit_args = vec!["commit", "--no-edit", "-F"];
        let merge_msg_path = merge_msg_path.to_string_lossy();
        commit_args.push(&merge_msg_path);
        if self.allow_empty {
            commit_args.push("--allow-empty");
        }
        let output = GitCommand::new("git")
            .args(&commit_args)
            .output()
            .context("Failed to execute git commit --no-edit")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.contains("nothing to commit") && !stdout.contains("nothing added to commit")
            {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!("Git commit --no-edit failed: {}", stderr));
            } else {
                println!("Nothing to commit");
            }
        }

        let mut subcmd = "--continue";
        loop {
            println!("Executing git {} {}", operation.command, subcmd);
            let output = GitCommand::new("git")
                .args(vec![&operation.command, subcmd])
                .args(if operation.command != "rebase" {
                    vec!["--no-edit"]
                } else {
                    vec![]
                })
                .output()
                .context(format!(
                    "Failed to execute git {} --continue",
                    operation.command
                ))?;

            if !output.status.success() {
                if String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains("--skip\" to skip this patch"))
                {
                    subcmd = "--skip";
                    continue;
                }

                if !operation.path.exists() {
                    return Ok(false);
                }

                if !output.stderr.is_empty() {
                    print!("{}", String::from_utf8_lossy(&output.stderr));
                }

                let after_head = std::fs::read_to_string(&operation.path)
                    .with_context(|| format!("Failed to read {}", operation.file))?
                    .trim()
                    .to_string();

                log::debug!(
                    "commit after_head: {} before_head: {}",
                    after_head,
                    before_head
                );

                // If operation --continue fails, retry synthmerge --vibe --continue
                return Ok(after_head != before_head || subcmd == "--skip");
            }

            break;
        }

        Ok(false)
    }

    pub fn deduplicate_conflicts_vibe(conflicts: &[ResolvedConflict]) -> Vec<ResolvedConflict> {
        let filtered: Vec<_> = conflicts
            .iter()
            .filter(|c| c.multi == Some(0) && c.beam == Some(0))
            .cloned()
            .collect();
        Self::deduplicate_conflicts(&filtered)
    }

    fn deduplicate_conflicts(conflicts: &[ResolvedConflict]) -> Vec<ResolvedConflict> {
        let mut map: HashMap<(String, usize, &str), Vec<&ResolvedConflict>> = HashMap::new();

        // Group conflicts by resolved_version, local_start and file_path
        for conflict in conflicts {
            map.entry((
                conflict.resolved_version.clone(),
                conflict.conflict.local_start,
                &conflict.conflict.file_path,
            ))
            .or_default()
            .push(conflict);
        }

        // For each group, create a new conflict with combined model names
        let mut result = Vec::new();
        for ((resolved_version, _, _), group) in map {
            let model = Self::combine_model_names(group.as_slice());

            // Use the first conflict in the group as the base
            let base_conflict = &group[0].conflict;
            let input_tokens = if group.iter().any(|c| c.input_tokens.is_some()) {
                Some(
                    group.iter().filter_map(|c| c.input_tokens).sum::<u64>()
                        / group.iter().filter_map(|c| c.input_tokens).count() as u64,
                )
            } else {
                None
            };
            let output_tokens = if group.iter().any(|c| c.output_tokens.is_some()) {
                Some(
                    group.iter().filter_map(|c| c.output_tokens).sum::<u64>()
                        / group.iter().filter_map(|c| c.output_tokens).count() as u64,
                )
            } else {
                None
            };
            let logprob = if group.iter().any(|c| c.logprob.is_some()) {
                Some(
                    group.iter().filter_map(|c| c.logprob).sum::<f64>()
                        / group.iter().filter_map(|c| c.logprob).count() as f64,
                )
            } else {
                None
            };
            let no_change = group[0].no_change;
            // for_each or the assert won't run
            group
                .iter()
                .skip(1)
                .for_each(|c| assert_eq!(c.no_change, no_change));
            result.push(ResolvedConflict {
                conflict: base_conflict.clone(),
                resolved_version,
                model,
                duration: group
                    .iter()
                    .map(|c| c.duration)
                    .max_by(|a, b| a.partial_cmp(b).unwrap())
                    .unwrap_or(0.0),
                input_tokens,
                output_tokens,
                logprob,
                endpoint_index: group.iter().map(|c| c.endpoint_index).min().unwrap(),
                deduplicated_conflicts: group
                    .into_iter()
                    .filter(|x| {
                        assert!(&x.conflict == base_conflict);
                        true
                    })
                    .cloned()
                    .collect(),
                beam: None,
                multi: None,
                no_change,
            });
        }

        // Sort by number of models that agree on the resolved conflict (descending)
        // When equal, maintain original order
        let mut seen = std::collections::HashSet::new();

        // First pass: collect all unique resolved conflicts with their original order positions
        let mut unique_conflicts: Vec<(String, &str, usize, usize, usize, bool)> = Vec::new();
        for original in conflicts {
            let key = (
                &original.resolved_version,
                original.conflict.local_start,
                &original.conflict.file_path,
            );
            if seen.insert(key) {
                let pos = result
                    .iter()
                    .position(|r| {
                        (
                            &r.resolved_version,
                            r.conflict.local_start,
                            &r.conflict.file_path,
                        ) == key
                    })
                    .unwrap();
                let num_models = result[pos].deduplicated_conflicts.len();

                // Calculate difference between resolved_version and merged_local_lines for Clean commits
                let adapted = if result[pos].conflict.commit_type.is_clean() {
                    let merged_lines = &result[pos].conflict.merged_local_lines;
                    let local_start = result[pos].conflict.local_start;
                    let local_end = result[pos].conflict.local_end;
                    let expected = merged_lines[local_start..local_end].join("");
                    let resolved = &result[pos].resolved_version;

                    expected != *resolved
                } else {
                    true
                };

                unique_conflicts.push((
                    result[pos].resolved_version.clone(),
                    &result[pos].conflict.file_path,
                    result[pos].conflict.local_start,
                    num_models,
                    result[pos].endpoint_index,
                    adapted,
                ));
            }
        }

        // Sort by file, line, number of models (descending),
        // difference score for Clean (descending), and finally with
        // the original "endpoint" order
        // In the case of a tie between variants of the same endpoint,
        // the stable sort preserves the original order, prioritizing
        // Variant 0 over later variants
        unique_conflicts.sort_by(|a, b| {
            a.1.cmp(b.1)
                .then(a.2.cmp(&b.2))
                .then(b.3.cmp(&a.3))
                .then(b.5.cmp(&a.5))
                .then(a.4.cmp(&b.4))
        });

        // Build the final ordered result
        let mut ordered_result = Vec::new();
        for (resolved_version, file_path, local_start, _, _, _) in unique_conflicts {
            let pos = result
                .iter()
                .position(|r| {
                    (
                        &r.resolved_version,
                        r.conflict.file_path.as_str(),
                        r.conflict.local_start,
                    ) == (&resolved_version, file_path, local_start)
                })
                .unwrap();
            ordered_result.push(result[pos].clone());
        }
        ordered_result
    }

    fn combine_model_names(group: &[&ResolvedConflict]) -> String {
        let mut suffix_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut prefixes = Vec::new();

        // Group models by their prefix (everything before the last '(')
        for conflict in group {
            let model_name = &conflict.model;
            if let Some(pos) = model_name.rfind('(') {
                let prefix = &model_name[..pos];
                let prefix = prefix.trim();
                let suffix_start = pos + 1;
                if let Some(suffix_end) = model_name[suffix_start..].find(')') {
                    let suffix = &model_name[suffix_start..suffix_start + suffix_end];
                    let entry = suffix_map.entry(prefix.to_string()).or_default();
                    assert!(
                        !entry.contains(&suffix.to_string()),
                        "Duplicate suffix found: {} for prefix {}",
                        suffix,
                        prefix
                    );
                    entry.push(suffix.to_string());
                    prefixes.push(prefix);
                } else {
                    // No closing parenthesis, treat as regular name
                    let entry = suffix_map.entry(model_name.clone()).or_default();
                    assert!(
                        entry.is_empty(),
                        "Duplicate empty suffix found for model: {}",
                        model_name
                    );
                    entry.push("".to_string());
                    prefixes.push(prefix);
                }
            } else {
                // No parentheses, treat as regular name
                let entry = suffix_map.entry(model_name.clone()).or_default();
                assert!(
                    entry.is_empty(),
                    "Duplicate empty suffix found for model: {}",
                    model_name
                );
                entry.push("".to_string());
                prefixes.push(model_name);
            }
        }

        let mut combined_names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for prefix in prefixes {
            if !seen.insert(prefix) {
                continue;
            };
            let suffixes = suffix_map.get(prefix).unwrap();
            assert!(!suffixes.is_empty());
            assert!(suffixes.iter().filter(|x| x.is_empty()).count() <= 1);
            if suffixes.len() == 1 && suffixes[0].is_empty() {
                combined_names.push(prefix.to_string());
            } else {
                // Combine suffixes into a single string like "(suffix1|suffix2|suffix3)"
                let suffixes_str = suffixes.to_vec().join("|");
                combined_names.push(format!("{} ({})", prefix, suffixes_str));
            }
        }

        combined_names.join(", ")
    }

    fn git_checkout(&self, commit_hash: &str, path: &str) -> Result<()> {
        let output = GitCommand::new("git")
            .args(["checkout", commit_hash, "--", path])
            .output()
            .context(format!(
                "Failed to execute git checkout {} -- {}",
                commit_hash, path
            ))?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git checkout {} -- {} failed: {}",
                commit_hash,
                path,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    /// Delete unmerged files that have been deleted in one side and updated in the other
    /// Add unmerged files that have been added on one side and updated in theo ther
    fn git_add_delete_unmerged(&mut self) -> Result<()> {
        // Get the current status in v2 format with null-terminated entries
        let status_output = self.git_status_porcelain_v2(None)?;

        // Parse the status output to find files that are unmerged with our state deleted (D)
        // and their state updated (U), or vice versa (U and D)
        // Split by null byte to handle paths with newlines
        let status_output_bytes = &status_output.stdout;
        let lines = status_output_bytes.split(|&b| b == b'\0').peekable();

        for line_bytes in lines {
            let line = String::from_utf8_lossy(line_bytes);
            // Skip header lines
            if line.starts_with('#') {
                continue;
            }

            // Parse unmerged entries (format: u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>)
            if line.starts_with('u') {
                let parts: Vec<&str> = line.splitn(11, ' ').collect();
                if parts.len() >= 11 {
                    let xy = parts[1];
                    // Check if our state is deleted (D) and their state is updated (U)
                    // or our state is updated (U) and their state is deleted (D)
                    // XY format: first char is our state, second is their state
                    if xy.len() >= 2 {
                        let c0 = xy.chars().nth(0);
                        let c1 = xy.chars().nth(1);
                        if (c0 == Some('D') && c1 == Some('U'))
                            || (c0 == Some('U') && c1 == Some('D'))
                            || (c0 == Some('D') && c1 == Some('D'))
                            || (c0 == Some('A') && c1 == Some('U'))
                        {
                            let path = parts[10];
                            // Run git rm on this file
                            let rm_output = GitCommand::new("git")
                                .args(["rm", path])
                                .output()
                                .context(format!("Failed to execute git rm --cached {}", path))?;

                            if !rm_output.status.success() {
                                return Err(anyhow::anyhow!(
                                    "Git rm --cached {} failed: {}",
                                    path,
                                    String::from_utf8_lossy(&rm_output.stderr)
                                ));
                            }
                            self.unresolved_deleted_files.insert(path.to_string());
                            self.assisted = true;
                        } else if c0 == Some('U') && c1 == Some('A') {
                            let path = parts[10];
                            self.git_update_index(Some(path))?;
                            self.unresolved_added_files.insert(path.to_string());
                            self.assisted = true;
                        } else if c0 == Some('A') && c1 == Some('A') {
                            let path = parts[10];

                            // Checkout the file from the current branch if it already exists
                            self.git_checkout("HEAD", path)?;
                            self.unresolved_added_files.insert(path.to_string());
                            self.assisted = true;
                        }
                    }
                }
            }
        }

        self.update_merge_message(true, false)?;
        // Reset assisted
        self.assisted = false;
        self.assisted_conflicts.clear();

        Ok(())
    }

    /// Update the git index
    fn git_update_index(&self, file_path: Option<&str>) -> Result<()> {
        let mut args = vec!["add", "-u"];
        if let Some(fp) = file_path {
            args.push(fp);
        }
        let output = GitCommand::new("git")
            .args(&args)
            .output()
            .context("Failed to execute git add -u")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git add -u failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if let Some(fp) = file_path {
            println!("Updated git index for {}", fp);
        } else {
            println!("Updated git index");
        }
        Ok(())
    }

    /// Get the git root directory
    fn get_git_root_uncached() -> Result<String> {
        let output = GitCommand::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("Failed to execute git rev-parse")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let git_root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(git_root)
    }

    /// Get the git directory
    fn get_git_dir_uncached() -> Result<String> {
        let output = GitCommand::new("git")
            .args(["rev-parse", "--git-dir"])
            .output()
            .context("Failed to execute git rev-parse")?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(git_dir)
    }

    /// Check if cherry-pick was run without -x
    fn check_cherry_pick_x(&mut self, merge_msg_content: &str) -> Result<()> {
        let operation = self.find_operation_head(&self.git_dir)?;
        if let Some(op) = operation {
            if op.command != "cherry-pick" {
                return Ok(());
            }
        } else {
            return Ok(());
        }

        if !self.cherry_pick_x_warned
            && !merge_msg_content.contains("\n(cherry picked from commit ")
        {
            log::warn!("git cherry-pick was run without the -x flag");
            self.cherry_pick_x_warned = true;
        }

        Ok(())
    }

    fn assisted_by_line(&mut self, show_models: bool) -> String {
        if show_models && !self.assisted_conflicts.is_empty() {
            self.assisted_conflicts.sort_by(|a, b| {
                a.endpoint_index
                    .cmp(&b.endpoint_index)
                    .then_with(|| a.model.cmp(&b.model))
            });
            self.assisted_conflicts
                .dedup_by(|a, b| a.endpoint_index == b.endpoint_index && a.model == b.model);
            let models_str = Self::combine_model_names(
                &self
                    .assisted_conflicts
                    .iter()
                    .collect::<Vec<&ResolvedConflict>>(),
            );
            format!("{} [{}]", self.assisted_by_line, models_str)
        } else {
            self.assisted_by_line.clone()
        }
    }

    /// Get the path to the merge message file based on the current operation
    fn get_merge_msg_path(&self, git_dir: &str) -> PathBuf {
        if self.in_rebase {
            Path::new(git_dir).join(Self::REBASE_MESSAGE_FILE)
        } else {
            Path::new(git_dir).join(Self::MERGE_MSG_FILE)
        }
    }

    /// Update the git merge message to include Assisted-by line
    fn update_merge_message(&mut self, files_status: bool, show_models: bool) -> Result<()> {
        if !self.assisted {
            return Ok(());
        }

        let merge_msg_path = self.get_merge_msg_path(&self.git_dir);
        let merge_msg_content = match fs::read_to_string(&merge_msg_path) {
            Ok(content) => content,
            Err(_) => {
                println!(
                    "If you use the AI generated code please add \"{}\"",
                    self.assisted_by_line
                );
                return Ok(());
            }
        };

        let mut lines: Vec<String> = merge_msg_content
            .split_inclusive('\n')
            .map(|s| s.to_string())
            .collect();

        // Find the line before "# Conflicts:" or end of file
        let mut insert_pos = lines.len();
        for (i, line) in lines.iter().enumerate() {
            if line.trim() == "# Conflicts:" {
                insert_pos = i;
                break;
            }
        }

        // Go backwards to find the last non-empty and non comment line
        while insert_pos > 0 {
            insert_pos -= 1;
            let line = &lines[insert_pos];
            if !line.trim().is_empty() && !line.starts_with("#") {
                break;
            }
        }

        let mut added_lines = vec!["\n".to_string()];
        let no_added_lines = added_lines.clone();

        let mut i = insert_pos + 1;
        let regex = regex::Regex::new(r"^[A-Z][^\s]*-by:\s.*\n$").unwrap();
        while i > 0 {
            i -= 1;
            let line = &lines[i];
            if regex.is_match(&lines[i]) {
                added_lines.pop();
                break;
            } else if !line.trim().is_empty() {
                break;
            }
        }

        if !merge_msg_content.contains(&self.assisted_by_line) {
            let assisted_by_line = self.assisted_by_line(show_models);
            added_lines.push(format!("{}\n", assisted_by_line));
            println!("Added \"{}\"", assisted_by_line);
        }

        if files_status {
            let file_lists = [
                ("Resolved", &mut self.resolved_files),
                ("Unresolved", &mut self.unresolved_files),
                ("Unresolved clean", &mut self.unresolved_clean_files),
                ("Unresolved added", &mut self.unresolved_added_files),
                ("Unresolved deleted", &mut self.unresolved_deleted_files),
            ];

            assert!(file_lists.iter().any(|f| !f.1.is_empty()));
            added_lines.push("\n".to_string());
            added_lines.push(format!("{} status:\n", self.synthmerge_name));
            for (title, files) in file_lists {
                added_lines.extend(Self::format_conflict_list(title, files));
                files.drain();
            }
        }

        if !added_lines.is_empty() && added_lines != no_added_lines {
            lines.splice(insert_pos + 1..insert_pos + 1, added_lines);

            let updated_content = lines.join("");
            fs::write(&merge_msg_path, updated_content).with_context(|| {
                format!(
                    "Failed to write updated merge message: {}",
                    merge_msg_path.display()
                )
            })?;
        }

        // Check for cherry-pick without -x flag
        self.check_cherry_pick_x(&merge_msg_content)?;

        Ok(())
    }

    fn format_conflict_list(title: &str, files: &HashSet<String>) -> Vec<String> {
        if files.is_empty() {
            return Vec::new();
        }
        let mut sorted_files: Vec<_> = files.iter().cloned().collect();
        sorted_files.sort();
        let mut lines = vec![format!("\t{}:\n", title)];
        for f in &sorted_files {
            lines.push(format!("\t\t{}\n", f));
        }
        lines
    }

    /// Check if we are currently in a cherry-pick, merge, or rebase state
    pub fn find_commit_hash(&mut self) -> Result<Option<String>> {
        let git_dir = &self.git_dir;

        // Check for cherry-pick, merge, and rebase HEAD files
        let operation = self.find_operation_head(git_dir)?;

        let content = if let Some(operation) = operation {
            let content = std::fs::read_to_string(&operation.path)
                .with_context(|| format!("Failed to read {}", operation.file))?
                .trim()
                .to_string();

            // Check if it's a rebase
            self.in_rebase = operation.command == "rebase";

            Some(content)
        } else {
            None
        };

        Ok(content)
    }

    /// Find the most recent operation HEAD file
    fn find_operation_head(&self, git_dir: &str) -> Result<Option<OperationHead>> {
        // Check each file and return the most recent one
        let mut retval: Option<OperationHead> = None;
        let mut latest_time = std::time::SystemTime::UNIX_EPOCH;

        for (file, command) in [
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REBASE_HEAD", "rebase"),
            ("REVERT_HEAD", "revert"),
            ("MERGE_HEAD", "merge"),
        ] {
            let path = Path::new(git_dir).join(file);
            if path.exists() {
                let metadata = std::fs::metadata(&path)
                    .with_context(|| format!("Failed to get metadata for {}", file))?;
                let file_time = metadata
                    .modified()
                    .with_context(|| format!("Failed to get modification time for {}", file))?;

                if file_time > latest_time {
                    latest_time = file_time;
                    retval = Some(OperationHead {
                        file: file.to_string(),
                        command: command.to_string(),
                        path,
                    });
                }
            }
        }

        if let Some(ref operation) = retval
            && operation.command == "rebase"
        {
            // Also check if the rebase message file exists
            let rebase_msg_path = Path::new(git_dir).join(Self::REBASE_MESSAGE_FILE);
            if !rebase_msg_path.exists() {
                log::warn!(
                    "Rebase message file not found: {}",
                    Self::REBASE_MESSAGE_FILE
                );
                return Ok(None);
            }
        }

        Ok(retval)
    }

    /// Extract the patch from a specific commit hash
    pub fn extract_diff(
        commit_hash: &str,
        max_context_size: u32,
        dir: Option<&str>,
        context_lines: &ContextLines,
    ) -> Result<Option<String>> {
        let diff = Self::git_show_in_dir(commit_hash, dir, None, context_lines)?;
        Ok(diff.and_then(|d| {
            if d.len() <= max_context_size.try_into().unwrap() {
                let lines = d.split_inclusive('\n').collect::<Vec<_>>();
                let commit_end = lines
                    .iter()
                    .position(|line| line.starts_with("diff --git a"))
                    .unwrap_or(lines.len());

                let is_filtered_trailer = |line: &str| {
                    let Some((key, value)) = line.trim_end().split_once(':') else {
                        return false;
                    };
                    let key = key.trim().to_ascii_lowercase();
                    let value = value.trim().to_ascii_lowercase();

                    key.ends_with("-by")
                        || key.contains("fixes")
                        || key.contains("link")
                        || value.contains("https://")
                        || value.contains("http://")
                        || value.contains('@')
                };

                let mut paragraphs = Vec::new();
                let mut line_index = 0;
                while line_index < commit_end {
                    while line_index < commit_end && lines[line_index].trim().is_empty() {
                        line_index += 1;
                    }

                    let paragraph_start = line_index;
                    while line_index < commit_end && !lines[line_index].trim().is_empty() {
                        line_index += 1;
                    }

                    if paragraph_start < line_index {
                        let paragraph = &lines[paragraph_start..line_index];
                        let is_last_paragraph = lines[line_index..commit_end]
                            .iter()
                            .all(|line| line.trim().is_empty());
                        let is_filtered = if is_last_paragraph {
                            paragraph.iter().any(|line| is_filtered_trailer(line))
                        } else {
                            paragraph.iter().all(|line| is_filtered_trailer(line))
                        };
                        if !is_filtered {
                            paragraphs.push(paragraph.concat());
                        }
                    }
                }

                let mut d = paragraphs.join("\n");
                if commit_end < lines.len() {
                    if !d.is_empty() {
                        d.push('\n');
                    }
                    d.extend(lines[commit_end..].iter().copied());
                }

                Some(d)
            } else {
                log::warn!(
                    "Git diff exceeds max context size ({} bytes), skipping",
                    max_context_size
                );
                None
            }
        }))
    }

    /// Extract the patch from a specific commit hash
    fn git_show_in_dir(
        commit_hash: &str,
        dir: Option<&str>,
        filename: Option<&str>,
        context_lines: &ContextLines,
    ) -> Result<Option<String>> {
        let diff_context_lines = &format!("-U{}", context_lines.diff_context_lines);
        let dir = if let Some(directory) = dir {
            shellexpand::tilde(directory).to_string()
        } else {
            ".".to_string()
        };
        let output = if let Some(file) = filename {
            let filearg = &format!("{}:{}", commit_hash, file);
            let args = vec!["-C", &dir, "show", filearg];
            GitCommand::new("git")
                .args(&args)
                .output()
                .context("Failed to execute git show")?
        } else {
            let args = vec![
                "-C",
                &dir,
                "show",
                "--pretty=%B",
                "--no-color",
                "--histogram",
                diff_context_lines,
                commit_hash,
            ];
            GitCommand::new("git")
                .args(&args)
                .output()
                .context("Failed to execute git show")?
        };

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "Git show failed for commit {}: {}",
                commit_hash,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if filename.is_some() {
            Ok(Some(stdout))
        } else {
            let lines: Vec<&str> = stdout.split_inclusive('\n').collect();
            let mut result_lines = Vec::new();
            let mut include_line = true;

            for line in lines {
                if line.starts_with("diff --git") {
                    result_lines.push(line);
                    include_line = false;
                } else if line.starts_with("---") {
                    result_lines.push(line);
                    include_line = true;
                } else if include_line {
                    result_lines.push(line);
                }
            }

            Ok(Some(result_lines.join("")))
        }
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
