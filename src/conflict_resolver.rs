// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use crate::api_client::{ApiClient, ApiRequest, ApiResponse};
use crate::config::{Config, EndpointConfig, EndpointTypeConfig};
use crate::lmdb_cache::{ApiCache, LmdbCacheImpl};
use crate::patch_locator::Hunk;
use crate::prob;
use anyhow::Result;
use futures::future::select_all;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CommitType {
    Clean,
    CleanDisabled,
    ConflictAndClean,
    #[default]
    Conflict,
}

impl CommitType {
    pub fn is_clean(&self) -> bool {
        matches!(self, CommitType::Clean | CommitType::CleanDisabled)
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, CommitType::Conflict | CommitType::ConflictAndClean)
    }
}

impl std::ops::Add for CommitType {
    type Output = Result<Self>;

    fn add(self, other: Self) -> Self::Output {
        use CommitType::*;
        match (self, other) {
            (Clean, Clean) => Ok(Clean),
            (Conflict, Conflict) => Ok(Conflict),
            (CleanDisabled, CleanDisabled) => Ok(CleanDisabled),
            (Clean, CleanDisabled) => Ok(Clean),
            (CleanDisabled, Clean) => Ok(Clean),
            (_, _) => Ok(ConflictAndClean),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Conflict {
    pub file_path: String,
    pub conflict_code: String,
    pub head_context: String,
    pub tail_context: String,
    pub conflict_patch: String,
    pub conflict_raw_patch: Option<String>,
    pub start_line: usize,
    pub nr_conflict_lines: usize,
    pub local_start: usize,
    pub local_end: usize,
    pub new_local_start: usize,
    pub new_local_end: usize,
    pub base_start: usize,
    pub base_end: usize,
    pub remote_start: usize,
    pub remote_end: usize,
    pub nr_head_context_lines: usize,
    pub nr_tail_context_lines: usize,
    pub marker_size: usize,
    pub commit_type: CommitType,
    pub merged_local_lines: Arc<Vec<String>>,
    pub code_snippets: Arc<Vec<Snippet>>,
    pub hunks: Vec<Hunk>,
    pub conflict_relocation: bool,
    /// Lowest calc_distances value found by previous relocation
    /// passes: any relocation candidate with a distance greater or
    /// equal to this value is ignored in the next passes, so the
    /// relocation can't ping-pong between two positions forever.
    pub relocation_distance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConflict {
    pub conflict: Conflict,
    pub resolved_version: String,
    pub model: String,
    pub duration: f64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub logprob: Option<f64>,
    pub deduplicated_conflicts: Vec<ResolvedConflict>,
    pub endpoint_index: usize,
    pub multi: Option<usize>,
    pub beam: Option<usize>,
    pub no_change: bool,
}

pub struct ResolverErrors {
    pub errors: HashMap<String, usize>,
    pub retry_files: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snippet {
    pub snippet: String,
    pub local_start: usize,
    pub local_end: usize,
}

pub struct ConflictResolver<'a> {
    config: &'a Config,
    git_diff: Option<String>,
    bench: bool,
    start_regex: Regex,
    end_regex: Regex,
    reject_no_change: bool,
    lmdb_cache: Option<Arc<LmdbCacheImpl>>,
}

impl<'a> ConflictResolver<'a> {
    const BACKTICK: &'static str = "```";
    const DIFF_START: &'static str = "<|diff|>";
    const DIFF_END: &'static str = "<|/diff|>";
    const PATCH_START: &'static str = "<|patch|>";
    const PATCH_END: &'static str = "<|/patch|>";
    const CODE_START: &'static str = "<|code|>";
    const CODE_END: &'static str = "<|/code|>";
    pub const PATCHED_CODE_START: &'static str = "<|patched_code|>";
    pub const PATCHED_CODE_END: &'static str = "<|/patched_code|>";
    const CODE_SNIPPETS_START: &'static str = "<|code_snippets|>";
    const CODE_SNIPPETS_END: &'static str = "<|/code_snippets|>";
    const CODE_SNIPPET_START: &'static str = "<|code_snippet|>";
    const CODE_SNIPPET_END: &'static str = "<|/code_snippet|>";
    const REGEXP_PATCHED_CODE_START: &'static str =
        r"(?ms)^(?:```)?[{<|]{1,3}patched_code[|>}]{1,3}$\n";
    const REGEXP_PATCHED_CODE_END: &'static str =
        r"(?ms)^[{<|/]{1,4}patched_code[|>}]{1,3}(?:```|[<|]{1,2}eot[|>]{1,2})?$";
    pub fn new(
        config: &'a Config,
        git_diff: Option<String>,
        bench: bool,
        cache_path: Option<String>,
        cache_overwrite: bool,
        reject_no_change: bool,
    ) -> Self {
        let mut lmdb_cache: Option<Arc<LmdbCacheImpl>> = None;
        if let Some(ref path) = cache_path {
            lmdb_cache = Some(Arc::new(
                ApiCache::create_from_path(path, cache_overwrite)
                    .expect("Failed to create API cache"),
            ));
        }
        ConflictResolver {
            config,
            git_diff,
            bench,
            start_regex: Regex::new(Self::REGEXP_PATCHED_CODE_START).unwrap(),
            end_regex: Regex::new(Self::REGEXP_PATCHED_CODE_END).unwrap(),
            reject_no_change,
            lmdb_cache,
        }
    }

    /// Resolve all conflicts using AI
    pub async fn resolve_conflicts(
        self,
        conflicts: &[Conflict],
        prev_resolved_conflicts: &[ResolvedConflict],
    ) -> Result<(Vec<ResolvedConflict>, ResolverErrors)> {
        let config = &self.config;
        let endpoints = config.get_all_endpoints();
        let mut resolved_conflicts = Vec::new();
        let mut resolver_errors = ResolverErrors {
            errors: HashMap::new(),
            retry_files: HashSet::new(),
        };

        for (conflict_index, conflict) in conflicts.iter().enumerate() {
            // Check if we have a previous resolved conflict that matches this one
            let mut skip_ai_resolution = false;
            for prev_conflict in prev_resolved_conflicts {
                if prev_conflict.conflict.file_path == conflict.file_path
                    && prev_conflict.conflict.local_start == conflict.local_start
                    && prev_conflict.conflict.local_end == conflict.local_end
                {
                    resolved_conflicts.push(prev_conflict.clone());
                    skip_ai_resolution = true;
                }
            }
            if skip_ai_resolution {
                let conflict_info = format!(
                    "Skipping resolved conflict {} of {} in {}:{}->{}",
                    conflict_index + 1,
                    conflicts.len(),
                    conflict.file_path,
                    conflict.start_line,
                    conflict.local_start
                );
                log::info!("{}", conflict_info);
                continue;
            }
            if !self.bench {
                let conflict_info = format!(
                    "Resolving conflict {} of {} in {}:{}->{}",
                    conflict_index + 1,
                    conflicts.len(),
                    conflict.file_path,
                    conflict.start_line,
                    conflict.local_start
                );
                println!("{}", conflict_info);
                log::info!("{}", conflict_info);
            }

            // Create the prompt for AI resolution
            let prompt = self.create_prompt(conflict);
            let patch = conflict.conflict_patch.clone();
            let code = format!(
                "{}{}{}",
                conflict.head_context, conflict.conflict_code, conflict.tail_context
            );

            // Try to resolve with all endpoints in parallel
            let mut futures = Vec::new();
            for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
                if conflict.commit_type == CommitType::Clean && !endpoint.primary {
                    continue;
                }
                let client = ApiClient::new(endpoint.clone(), self.lmdb_cache.clone());
                let use_backticks = endpoint.use_backticks;
                let message = self.create_message(&patch, &code, use_backticks);
                let git_diff = self.create_git_diff(conflict, use_backticks);
                let training = Self::create_training(use_backticks);
                let api_request = ApiRequest {
                    prompt: prompt.clone(),
                    training,
                    message,
                    patch: patch.clone(),
                    code: code.clone(),
                    git_diff,
                };
                let handle = tokio::spawn(async move {
                    let result = client.query(&api_request).await;
                    (result, endpoint_index)
                });
                futures.push(handle);
            }

            let mut results = Vec::new();
            while !futures.is_empty() {
                let (result, _, remaining) = select_all(futures).await;
                futures = remaining;
                match result {
                    Ok((result, endpoint_index)) => {
                        let (result, fallback) = match result {
                            Ok((result, fallback)) => (Ok(result), fallback),
                            Err(e) => (Err(e), None),
                        };
                        let endpoint = self.resolve_endpoint(endpoints, endpoint_index, fallback);
                        println!(
                            " - {}{}",
                            endpoint.name,
                            self.print_api_response(&result, endpoint)
                        );
                        results.push((result, endpoint_index, fallback))
                    }
                    Err(e) => return Err(anyhow::anyhow!("Task failed: {}", e)),
                }
            }

            self.process_results(
                &mut resolved_conflicts,
                &mut resolver_errors,
                &results,
                conflict,
                endpoints,
            );
        }

        Ok((resolved_conflicts, resolver_errors))
    }

    fn resolve_endpoint<'b>(
        &self,
        endpoints: &'b [EndpointConfig],
        endpoint_index: usize,
        fallback: Option<usize>,
    ) -> &'b EndpointConfig {
        match fallback {
            Some(fallback) => &endpoints[endpoint_index].fallbacks[fallback],
            None => &endpoints[endpoint_index],
        }
    }

    fn print_api_response(
        &self,
        api_response: &Result<ApiResponse>,
        endpoint: &EndpointConfig,
    ) -> String {
        api_response
            .as_ref()
            .map(|r| {
                let mut info = String::new();
                for (variant, variants) in r.iter().enumerate() {
                    self.get_variant_name(endpoint, variant)
                        .map(|x| info.push_str(&format!(" | {x}")));
                    for (beam, entry) in variants.iter().enumerate() {
                        if let Ok(entry) = entry {
                            let beam = if beam > 0 {
                                format!(" ~ #{beam}")
                            } else {
                                String::new()
                            };
                            let duration_info = format!(" {:.1}s", entry.duration);
                            let input_tokens_info = entry
                                .input_tokens
                                .map(|tokens| format!(" {} it", tokens))
                                .unwrap_or_default();
                            let output_tokens_info = entry
                                .output_tokens
                                .map(|tokens| format!(" {} ot", tokens))
                                .unwrap_or_default();
                            let tokens_per_sec_info = if (entry.input_tokens.is_some()
                                || entry.output_tokens.is_some())
                                && entry.duration > 0.0
                            {
                                let total = entry
                                    .input_tokens
                                    .unwrap_or(0)
                                    .saturating_add(entry.output_tokens.unwrap_or(0));
                                format!(" {:.0} t/s", total as f64 / entry.duration)
                            } else {
                                String::new()
                            };
                            let logprob_info = entry
                                .logprob
                                .map(|logprob| format!(" {:.1}%", prob::logprob_to_prob(logprob)))
                                .unwrap_or_default();
                            info.push_str(&format!(
                                "{}{}{}{}{}{}",
                                beam,
                                duration_info,
                                input_tokens_info,
                                output_tokens_info,
                                tokens_per_sec_info,
                                logprob_info,
                            ));
                        }
                    }
                }
                info
            })
            .unwrap_or_default()
    }

    fn get_model_name_multi(
        &self,
        endpoint: &EndpointConfig,
        variant: usize,
        beam: usize,
        multi: usize,
    ) -> String {
        let variant_name = self.get_variant_name(endpoint, variant);
        let mut name = endpoint.name.to_string();
        let mut open = false;
        if let Some(variant_name) = *variant_name {
            open = true;
            name.push_str(" (");
            name.push_str(&variant_name);
        }
        if beam > 0 {
            if !open {
                open = true;
                name.push_str(" (");
            }
            name.push_str(&format!("#{}", beam));
        }
        if multi > 0 {
            if !open {
                open = true;
                name.push_str(" (");
            }
            name.push_str(&format!("${}", multi));
        }
        if open {
            name.push(')');
        }
        name
    }

    fn get_model_name(&self, endpoint: &EndpointConfig, variant: usize, beam: usize) -> String {
        self.get_model_name_multi(endpoint, variant, beam, 0)
    }

    fn get_variant_name(&self, endpoint: &EndpointConfig, variant: usize) -> Box<Option<String>> {
        match &endpoint.config {
            EndpointTypeConfig::OpenAI { variants, .. }
            | EndpointTypeConfig::Anthropic { variants, .. } => {
                if let Some(variants) = variants {
                    if let Some(variant) = variants.get(variant) {
                        return variant.name.clone();
                    } else {
                        assert!(variant == 0);
                    }
                }
                Box::new(None)
            }
            EndpointTypeConfig::Patchpal { .. } => Box::new(None),
        }
    }

    fn validate_resolved_version_not_patch(resolved_version: &str, conflict: &Conflict) -> bool {
        let has_patch_lines = resolved_version
            .lines()
            .any(|line| line.starts_with('+') || line.starts_with('-'));

        if !has_patch_lines {
            return true;
        }

        // Check if conflict_code has lines starting with + or -
        let code_has_patch_lines = conflict
            .conflict_code
            .lines()
            .any(|line| line.starts_with('+') || line.starts_with('-'));

        let patch_has_patch_lines = conflict
            .conflict_patch
            .lines()
            .any(|line| line.chars().nth(1) == Some('+') || line.chars().nth(1) == Some('-'));

        code_has_patch_lines || patch_has_patch_lines
    }

    fn process_results(
        &self,
        resolved_conflicts: &mut Vec<ResolvedConflict>,
        resolver_errors: &mut ResolverErrors,
        results: &Vec<(Result<ApiResponse>, usize, Option<usize>)>,
        conflict: &Conflict,
        endpoints: &[EndpointConfig],
    ) {
        let mut recoverable = [false, false];
        let mut no_solutions = [true, true];
        let mut local_resolved_conflicts = Vec::new();

        // Validate that the content starts with head_context and ends with tail_context
        for result in results {
            let endpoint_index = result.1;
            let fallback = result.2;
            // Resolve the effective endpoint (the fallback if one was used)
            // so variant names and error attribution are consistent
            let endpoint = self.resolve_endpoint(endpoints, endpoint_index, fallback);
            let result = match &result.0 {
                Ok(r) => r,
                Err(e) => {
                    let model = &endpoint.name;
                    log::error!("Skipping {} due to error: {}", model, e);
                    *resolver_errors.errors.entry(model.to_string()).or_insert(0) += 1;
                    continue;
                }
            };

            let primary = if endpoint.primary { 1 } else { 0 };

            // Helper closure for error handling
            let mut record_error = |model: &str, retry: bool| {
                *resolver_errors.errors.entry(model.to_string()).or_insert(0) += 1;
                if retry {
                    recoverable[primary] = true;
                }
            };

            for (variant, api_response_variant) in result.iter().enumerate() {
                for (beam, api_response_entry) in api_response_variant.iter().enumerate() {
                    let api_response_entry = match api_response_entry {
                        Ok(api_response_entry) => api_response_entry,
                        Err(e) => {
                            let model = self.get_model_name(endpoint, variant, beam);
                            log::error!("Skipping {} - {}", model, e);
                            record_error(&model, false);
                            continue;
                        }
                    };

                    let resolved_strings = match self.parse_response(&api_response_entry.response) {
                        Ok(resolved_strings) => resolved_strings,
                        Err(e) => {
                            let model = self.get_model_name(endpoint, variant, beam);
                            log::warn!("Skipping {} - {}", model, e);
                            record_error(&model, beam == 0);
                            continue;
                        }
                    };
                    assert!(!resolved_strings.is_empty());
                    assert!(!api_response_entry.response.is_empty());

                    let mut seen_resolved = std::collections::HashMap::new();
                    for (multi, resolved_string) in resolved_strings.iter().enumerate() {
                        let model = self.get_model_name_multi(endpoint, variant, beam, multi);
                        let mut resolved_version = resolved_string.to_string();

                        let mut found_context = false;
                        for _ in 0..conflict.nr_head_context_lines.saturating_sub(1).max(1) {
                            if resolved_version.starts_with(&conflict.head_context) {
                                found_context = true;
                                break;
                            }
                            // if conflict.head_context.trim().is_empty() {
                            //     break;
                            // }
                            resolved_version = format!("\n{}", resolved_version);
                        }
                        if !found_context {
                            log::warn!("Skipping {} - doesn't start with head context", model);
                            let len = conflict.head_context.len().min(resolved_string.len());
                            let diff = ConflictResolver::create_diff(
                                &conflict.head_context,
                                &resolved_string[..len],
                                1,
                            );
                            log::info!("HeadContextDiff:\n{}", diff);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        }
                        let leading_tail_context = if !conflict.head_context.is_empty() {
                            &format!("\n{}", conflict.tail_context)
                        } else {
                            &conflict.tail_context
                        };
                        let mut found_context = false;
                        for _ in 0..conflict.nr_tail_context_lines.saturating_sub(1).max(1) {
                            if resolved_version.ends_with(leading_tail_context) {
                                found_context = true;
                                break;
                            }
                            // if conflict.head_context.trim().is_empty() {
                            //     break;
                            // }
                            resolved_version = format!("{}\n", resolved_version);
                        }
                        if !found_context {
                            log::warn!("Skipping {} - doesn't end with tail context", model);
                            let diff = ConflictResolver::create_diff(
                                &resolved_string[resolved_string
                                    .len()
                                    .saturating_sub(leading_tail_context.len())..],
                                leading_tail_context,
                                1,
                            );
                            log::info!("TailContextDiff:\n{}", diff);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        }
                        //reduce resolved to the range between head_context and tail_context
                        let context_len = conflict.head_context.len() + conflict.tail_context.len();
                        if resolved_version.len() < context_len {
                            log::warn!(
                                "Skipping {} - resolved content is too short to contain both head and tail context",
                                model
                            );
                            log::trace!("ResolvedContent:\n{}", resolved_string);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        };

                        resolved_version.drain(0..conflict.head_context.len());
                        resolved_version
                            .drain(resolved_version.len() - conflict.tail_context.len()..);

                        if resolved_version.chars().last().is_some_and(|c| c != '\n') {
                            log::warn!(
                                "Skipping {} - resolved content is not newline terminated",
                                model
                            );
                            log::trace!("ResolvedContent:\n{}", resolved_version);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        }

                        if !Self::validate_resolved_version_not_patch(&resolved_version, conflict) {
                            log::warn!("Skipping {} - resolved version looks like a patch", model);
                            log::trace!("ResolvedContent:\n{}", resolved_version);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        }

                        let no_change = resolved_version == conflict.conflict_code;
                        if self.reject_no_change && no_change {
                            log::warn!("Skipping {} - no change detected", model);
                            record_error(&model, beam == 0 && multi == 0);
                            continue;
                        }

                        // Check if this resolved_version is already in the results
                        let key = (endpoint_index, resolved_version.clone());
                        if seen_resolved.contains_key(&key) {
                            log::debug!("Skipping {} - duplicate resolved conflict", model);
                            continue;
                        }
                        seen_resolved.insert(key, model.clone());

                        let input_tokens = api_response_entry.input_tokens;
                        let output_tokens = api_response_entry.output_tokens;
                        let logprob = api_response_entry.logprob;
                        let duration = api_response_entry.duration;
                        local_resolved_conflicts.push(ResolvedConflict {
                            conflict: conflict.clone(),
                            resolved_version,
                            model,
                            duration,
                            input_tokens,
                            output_tokens,
                            logprob,
                            deduplicated_conflicts: Vec::new(),
                            endpoint_index,
                            beam: Some(beam),
                            multi: Some(multi),
                            no_change,
                        });
                        if beam == 0 && multi == 0 {
                            no_solutions[primary] = false;
                        }
                    }
                }
            }
        }

        if (no_solutions[1] && recoverable[1])
            || (no_solutions[1] && no_solutions[0] && recoverable[0])
        {
            resolver_errors
                .retry_files
                .insert(conflict.file_path.clone());
        } else {
            resolved_conflicts.extend(local_resolved_conflicts);
        }
    }

    /// Create a prompt for the AI to resolve the conflict
    fn git_diff(git_diff: Option<String>, use_backticks: bool) -> Option<String> {
        git_diff.map(|s| {
            let mut diff_block = format!(
                r#"{diff_start}
{s}{diff_end}"#,
                diff_start = Self::DIFF_START,
                diff_end = Self::DIFF_END,
            );
            if use_backticks {
                diff_block = format!("{}\n{}\n{}", Self::BACKTICK, diff_block, Self::BACKTICK);
            }
            format!(
                r#"The PATCH originates from the DIFF between {diff_start}{diff_end}.

{diff_block}"#,
                diff_start = Self::DIFF_START,
                diff_end = Self::DIFF_END,
                diff_block = diff_block,
            )
        })
    }

    fn code_snippets(conflict: &Conflict, use_backticks: bool) -> Option<String> {
        let code_snippets = &conflict.code_snippets;
        if code_snippets.is_empty() {
            return None;
        }
        let snippets_with_wrappers = code_snippets
            .iter()
            .filter(|s| conflict.local_start > s.local_start || conflict.local_end < s.local_end)
            .map(|s| {
                format!(
                    "\n{}\n{}{}\n",
                    Self::CODE_SNIPPET_START,
                    s.snippet,
                    Self::CODE_SNIPPET_END
                )
            })
            .collect::<Vec<String>>();
        if snippets_with_wrappers.is_empty() {
            return None;
        }
        let snippets_with_wrappers = snippets_with_wrappers.join("\n");

        let mut code_snippets_block = format!(
            r#"{code_snippets_start}
{snippets_with_wrappers}
{code_snippets_end}"#,
            code_snippets_start = Self::CODE_SNIPPETS_START,
            code_snippets_end = Self::CODE_SNIPPETS_END,
        );
        if use_backticks {
            code_snippets_block = format!(
                "{}\n{}\n{}",
                Self::BACKTICK,
                code_snippets_block,
                Self::BACKTICK
            );
        }

        Some(format!(
            r#"Review the other conflicting CODE SNIPPETS in {file_path} between {code_snippets_start}{code_snippets_end}.

{code_snippets_block}"#,
            code_snippets_start = Self::CODE_SNIPPETS_START,
            code_snippets_end = Self::CODE_SNIPPETS_END,
            file_path = conflict.file_path,
            code_snippets_block = code_snippets_block,
        ))
    }

    fn raw_patch(raw_patch: &str, file_path: &str, use_backticks: bool) -> String {
        let mut diff_block = format!(
            r#"{diff_start}
{raw_patch}{diff_end}"#,
            diff_start = Self::DIFF_START,
            diff_end = Self::DIFF_END,
        );
        if use_backticks {
            diff_block = format!("{}\n{}\n{}", Self::BACKTICK, diff_block, Self::BACKTICK);
        }
        format!(
            r#"The DIFF for {file_path} is between {diff_start}{diff_end}.

{diff_block}"#,
            diff_start = Self::DIFF_START,
            diff_end = Self::DIFF_END,
            diff_block = diff_block,
        )
    }

    fn create_git_diff(&self, conflict: &Conflict, use_backticks: bool) -> Option<String> {
        let mut parts = Vec::new();
        let mut has_diff = false;
        if let Some(diff) = &self.git_diff
            && diff.contains(&conflict.file_path)
            && let Some(formatted_diff) = Self::git_diff(Some(diff.clone()), use_backticks)
        {
            parts.push(formatted_diff);
            has_diff = true;
        }
        if conflict.commit_type == CommitType::ConflictAndClean {
            parts.push(Self::raw_patch(
                conflict.conflict_raw_patch.as_ref().unwrap(),
                &conflict.file_path,
                use_backticks,
            ));
            has_diff = true;
        }
        if let Some(snippets) = Self::code_snippets(conflict, use_backticks) {
            parts.push(snippets);
            if has_diff {
                parts.push("Apply only the adapted PATCH, do not apply other parts of the DIFF to the CODE. The entire DIFF will be applied to all CODE SNIPPETS to produce the final PATCHED CODE SNIPPETS. Adapt the PATCH accordingly so the PATCHED CODE works correctly with the PATCHED CODE SNIPPETS.".to_string());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }

    /// Create a prompt for the AI to resolve the conflict
    fn create_prompt(&self, conflict: &Conflict) -> String {
        let head_lines = conflict.nr_head_context_lines;
        let tail_lines = conflict.nr_tail_context_lines;

        let context_instruction = if head_lines == 0 && tail_lines == 0 {
            String::new()
        } else if head_lines == 0 {
            format!(
                "\n\nRewrite the {nr_tail_context_lines} line{tail_plural} before {code_end} exactly the same, including all empty lines.",
                nr_tail_context_lines = tail_lines,
                tail_plural = if tail_lines != 1 { "s" } else { "" },
                code_end = Self::CODE_END,
            )
        } else if tail_lines == 0 {
            format!(
                "\n\nRewrite the {nr_head_context_lines} line{head_plural} after {code_start} exactly the same, including all empty lines.",
                nr_head_context_lines = head_lines,
                head_plural = if head_lines != 1 { "s" } else { "" },
                code_start = Self::CODE_START,
            )
        } else {
            format!(
                "\n\nRewrite the {nr_head_context_lines} line{head_plural} after {code_start} and the {nr_tail_context_lines} line{tail_plural} before {code_end} exactly the same, including all empty lines.",
                nr_head_context_lines = head_lines,
                nr_tail_context_lines = tail_lines,
                head_plural = if head_lines != 1 { "s" } else { "" },
                tail_plural = if tail_lines != 1 { "s" } else { "" },
                code_start = Self::CODE_START,
                code_end = Self::CODE_END,
            )
        };

        format!(
            r#"Apply the PATCH between {patch_start}{patch_end} to the CODE between {code_start}{code_end}.

FINALLY answer with the final PATCHED CODE between {patched_code_start}{patched_code_end} instead of markdown fences.{context_instruction}"#,
            patch_start = Self::PATCH_START,
            patch_end = Self::PATCH_END,
            code_start = Self::CODE_START,
            code_end = Self::CODE_END,
            patched_code_start = Self::PATCHED_CODE_START,
            patched_code_end = Self::PATCHED_CODE_END,
            context_instruction = context_instruction,
        )
    }

    fn create_training(use_backticks: bool) -> String {
        let mut patch_block = format!(
            r#"{patch_start}
@@ -1,7 +1,7 @@
 
 extern const struct feature default_feat;
 
-static inline const struct feature *get_extra_something(struct object *obj)
+static inline const struct feature *get_special_something(struct device *dev)
 {{
 	return &default_feat;
 }}
{patch_end}"#,
            patch_start = Self::PATCH_START,
            patch_end = Self::PATCH_END,
        );

        let mut code_block = format!(
            r#"{code_start}

extern struct feat feat;

static inline struct feat *get_extra_something(double option, struct device *obj, int param)
 {{	
	return &feat;
}}
{code_end}"#,
            code_start = Self::CODE_START,
            code_end = Self::CODE_END,
        );

        let mut patched_code_block = format!(
            r#"{patched_code_start}

extern struct feat feat;

static inline struct feat *get_special_something(double option, struct device *dev, int param)
 {{	
	return &feat;
}}
{patched_code_end}"#,
            patched_code_start = Self::PATCHED_CODE_START,
            patched_code_end = Self::PATCHED_CODE_END,
        );

        if use_backticks {
            patch_block = format!("{}\n{}\n{}", Self::BACKTICK, patch_block, Self::BACKTICK);
            code_block = format!("{}\n{}\n{}", Self::BACKTICK, code_block, Self::BACKTICK);
            patched_code_block = format!(
                "{}\n{}\n{}",
                Self::BACKTICK,
                patched_code_block,
                Self::BACKTICK
            );
        }

        format!(
            r#"Learn from the following training example:

{}

{}

{}"#,
            patch_block, code_block, patched_code_block,
        )
    }

    fn create_message(&self, patch: &String, code: &String, use_backticks: bool) -> String {
        let mut patch_str = format!(
            r#"{patch_start}
{patch}{patch_end}"#,
            patch_start = Self::PATCH_START,
            patch_end = Self::PATCH_END,
        );
        let mut code_str = format!(
            r#"{code_start}
{code}{code_end}"#,
            code_start = Self::CODE_START,
            code_end = Self::CODE_END,
        );
        if use_backticks {
            patch_str = format!("{}\n{}\n{}", Self::BACKTICK, patch_str, Self::BACKTICK);
            code_str = format!("{}\n{}\n{}", Self::BACKTICK, code_str, Self::BACKTICK);
        }
        format!("{}\n\n{}\n", patch_str, code_str)
    }

    pub fn create_diff(base: &str, remote: &str, patch_context_lines: u32) -> String {
        use similar::{Algorithm, TextDiff};
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Histogram)
            .diff_lines(base, remote);
        diff.unified_diff()
            .context_radius(patch_context_lines as usize)
            .to_string()
    }

    /// Parse the API response into 3 solutions
    fn parse_response(&self, response: &String) -> Result<Vec<String>> {
        let start_regex = &self.start_regex;
        let end_regex = &self.end_regex;

        log::info!("Response:\n{}", response);

        let mut results = Vec::new();
        let mut err: Option<Result<Vec<String>, anyhow::Error>> = None;
        let mut start = 0;

        while let Some(start_match) = start_regex.find_at(response, start) {
            let start_pos = start_match.end();
            let end_match = end_regex.find_at(response, start_pos);
            if end_match.is_none() {
                err = Some(Err(anyhow::anyhow!(
                    "Invalid format: missing {}",
                    Self::PATCHED_CODE_END
                )));
                break;
            }

            let end_match = end_match.unwrap();
            let end_pos = end_match.start();

            let content = &response[start_pos..end_pos];
            results.push(content.to_string());

            start = end_match.end();
        }

        if results.is_empty() {
            match err {
                Some(err) => err,
                None => Err(anyhow::anyhow!("No code blocks found in response")),
            }
        } else {
            Ok(results)
        }
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
