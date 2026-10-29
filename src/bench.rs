// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use crate::bench_args::BenchArgs;
use crate::config::{Config, EndpointTypeConfig};
use crate::conflict_resolver::{Conflict, ConflictResolver};
use crate::git_utils::{ContextLines, GitUtils};
use crate::prob::logprob_to_prob;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

#[derive(Debug)]
pub struct TestEntry {
    patch: String,
    code: String,
    patch_commit_hash: String,
    code_commit_hash: String,
    patched_code: String,
    filename: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TestResult {
    entry_index: usize,
    model: String,
    correct: bool,
    correct_aligned: bool,
    correct_stripped: bool,
    duration: f64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    logprob: Option<f64>,
    failed_patched_code: Option<String>,
    error: bool,
    patch_commit_hash: String,
    code_commit_hash: String,
}

#[derive(Debug)]
pub enum LogprobType {
    Global,
    Errors,
    Incorrect,
    CorrectStripped,
    CorrectAligned,
    Correct,
    COUNT,
}

#[derive(Debug)]
struct ModelStats {
    total: usize,
    correct: usize,
    correct_aligned: usize,
    correct_stripped: usize,
    error: usize,
    accuracy: f64,
    accuracy_aligned: f64,
    accuracy_stripped: f64,
    error_rate: f64,
    avg_input_tokens: f64,
    avg_output_tokens: f64,
    avg_logprob: [f64; LogprobType::COUNT as usize],
    std_logprob: [f64; LogprobType::COUNT as usize],
    avg_duration: f64,
}

#[derive(Debug)]
pub struct Bench {
    results: Vec<TestResult>,
    model_stats: HashMap<String, ModelStats>,
    git_diffs: HashMap<String, Option<String>>,
    line_number_re: regex::Regex,
}

impl Default for Bench {
    fn default() -> Self {
        Self::new()
    }
}

impl Bench {
    fn line_number_regex() -> regex::Regex {
        regex::Regex::new(r"^@@ -\d+,\d+ \+\d+,\d+ @@").unwrap()
    }
    pub fn new() -> Self {
        Bench {
            results: Vec::new(),
            model_stats: HashMap::new(),
            git_diffs: HashMap::new(),
            line_number_re: Self::line_number_regex(),
        }
    }

    pub fn load_database<P: AsRef<Path>>(path: P) -> Result<Vec<TestEntry>> {
        let file = File::open(path.as_ref())?;
        let mut reader = csv::Reader::from_reader(file);
        let mut entries = Vec::new();

        for result in reader.records() {
            let record = result?;
            if record.len() < 6 {
                continue;
            }
            let description = record
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Failed to get description from CSV record"))?;
            let mut split_desc = description.splitn(2, " / ");
            let code_commit_hash = split_desc.next().unwrap_or("").trim().to_string();
            let code_commit_hash = format!("{}^", code_commit_hash);
            let mut split_desc = split_desc.next().unwrap_or("").split('\n');
            let patch_commit_hash = split_desc.next().unwrap_or("").trim().to_string();
            let patch = record.get(3).unwrap_or("").to_string();
            let code = record.get(4).unwrap_or("").to_string();
            let patched_code = record.get(5).unwrap_or("").to_string();
            let filename = split_desc.next().unwrap_or("").trim().to_string();

            let entry = TestEntry {
                patch,
                code,
                patch_commit_hash,
                code_commit_hash,
                patched_code,
                filename,
            };
            entries.push(entry);
        }

        Ok(entries)
    }

    fn save_checkpoint(&mut self, args: &BenchArgs) -> Result<()> {
        let file = File::create(&args.checkpoint_path)?;
        let mut writer = csv::Writer::from_writer(file);
        for result in &self.results {
            writer.serialize(result)?;
        }
        writer.flush()?;
        self.calculate_stats(args);
        Ok(())
    }

    fn load_checkpoint(&mut self, args: &BenchArgs) -> Result<()> {
        if !Path::new(&args.checkpoint_path).exists() {
            return Ok(());
        }

        let file = File::open(&args.checkpoint_path)?;
        let mut reader = csv::Reader::from_reader(file);

        for result in reader.deserialize() {
            let result: TestResult = result.with_context(|| "Failed to parse test result")?;
            self.results.push(result);
        }

        self.calculate_stats(args);

        Ok(())
    }

    fn calculate_stats(&mut self, args: &BenchArgs) {
        // Initialize stats for all models
        let mut model_totals = HashMap::new();
        let mut model_correct = HashMap::new();
        let mut model_correct_aligned = HashMap::new();
        let mut model_correct_stripped = HashMap::new();
        let mut model_tokens = HashMap::new();
        let mut model_input_tokens = HashMap::new();
        let mut model_logprob = Vec::with_capacity(LogprobType::COUNT as usize);
        for _ in 0..LogprobType::COUNT as usize {
            model_logprob.push(HashMap::new());
        }
        let mut model_durations = HashMap::new();
        let mut model_errors = HashMap::new();

        // Collect all results by model
        for result in self
            .results
            .iter()
            .filter(|x| args.max_entries.is_none() || x.entry_index < args.max_entries.unwrap())
        {
            let model = &result.model;
            *model_totals.entry(model.clone()).or_insert(0) += 1;
            if result.correct {
                *model_correct.entry(model.clone()).or_insert(0) += 1;
            }
            if result.correct_aligned {
                *model_correct_aligned.entry(model.clone()).or_insert(0) += 1;
            }
            if result.correct_stripped {
                *model_correct_stripped.entry(model.clone()).or_insert(0) += 1;
            }
            if let Some(input_tokens) = result.input_tokens {
                model_input_tokens
                    .entry(model.clone())
                    .or_insert_with(Vec::new)
                    .push(input_tokens);
            }
            if let Some(output_tokens) = result.output_tokens {
                model_tokens
                    .entry(model.clone())
                    .or_insert_with(Vec::new)
                    .push(output_tokens);
            }
            if let Some(logprob) = result.logprob {
                model_logprob[LogprobType::Global as usize]
                    .entry(model.clone())
                    .or_insert_with(Vec::new)
                    .push(logprob);
                if result.error {
                    assert!(!result.correct);
                    assert!(!result.correct_aligned);
                    assert!(!result.correct_stripped);
                    model_logprob[LogprobType::Errors as usize]
                        .entry(model.clone())
                        .or_insert_with(Vec::new)
                        .push(logprob);
                }
                if result.correct {
                    assert!(!result.error);
                    assert!(result.correct_aligned);
                    assert!(result.correct_stripped);
                    model_logprob[LogprobType::Correct as usize]
                        .entry(model.clone())
                        .or_insert_with(Vec::new)
                        .push(logprob);
                }
                if result.correct_aligned {
                    assert!(!result.error);
                    assert!(result.correct_aligned);
                    model_logprob[LogprobType::CorrectAligned as usize]
                        .entry(model.clone())
                        .or_insert_with(Vec::new)
                        .push(logprob);
                }
                if result.correct_stripped {
                    assert!(!result.error);
                    model_logprob[LogprobType::CorrectStripped as usize]
                        .entry(model.clone())
                        .or_insert_with(Vec::new)
                        .push(logprob);
                } else {
                    assert!(!result.error);
                    model_logprob[LogprobType::Incorrect as usize]
                        .entry(model.clone())
                        .or_insert_with(Vec::new)
                        .push(logprob);
                }
            }
            if result.duration.is_normal() {
                model_durations
                    .entry(model.clone())
                    .or_insert_with(Vec::new)
                    .push(result.duration);
            }
            if result.error {
                *model_errors.entry(model.clone()).or_insert(0) += 1;
            }
        }

        // Calculate final stats
        self.model_stats.clear();
        for (model, total) in model_totals {
            let correct = model_correct.get(&model).copied().unwrap_or(0);
            let accuracy = correct as f64 / total as f64;
            let correct_aligned = model_correct_aligned.get(&model).copied().unwrap_or(0);
            let accuracy_aligned = correct_aligned as f64 / total as f64;
            let correct_stripped = model_correct_stripped.get(&model).copied().unwrap_or(0);
            let accuracy_stripped = correct_stripped as f64 / total as f64;
            let error = model_errors.get(&model).copied().unwrap_or(0);
            let error_rate = error as f64 / total as f64;

            let avg_input_tokens = model_input_tokens
                .get(&model)
                .map(|tokens| tokens.iter().sum::<u64>() as f64 / tokens.len() as f64)
                .unwrap_or(f64::INFINITY);

            let avg_output_tokens = model_tokens
                .get(&model)
                .map(|tokens| tokens.iter().sum::<u64>() as f64 / tokens.len() as f64)
                .unwrap_or(f64::INFINITY);

            let avg_logprob = [
                model_logprob[LogprobType::Global as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Errors as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Incorrect as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::CorrectStripped as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::CorrectAligned as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Correct as usize]
                    .get(&model)
                    .map(|logprob| logprob.iter().sum::<f64>() / logprob.len() as f64)
                    .unwrap_or(f64::INFINITY),
            ];

            let std_logprob = [
                model_logprob[LogprobType::Global as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Errors as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Incorrect as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::CorrectStripped as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::CorrectAligned as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
                model_logprob[LogprobType::Correct as usize]
                    .get(&model)
                    .map(|logprob| {
                        let avg = logprob.iter().map(|x| logprob_to_prob(*x)).sum::<f64>()
                            / logprob.len() as f64;
                        let variance = logprob
                            .iter()
                            .map(|x| (logprob_to_prob(*x) - avg).powi(2))
                            .sum::<f64>()
                            / logprob.len() as f64;
                        variance.sqrt()
                    })
                    .unwrap_or(f64::INFINITY),
            ];

            let avg_duration = model_durations
                .get(&model)
                .map(|durations| durations.iter().sum::<f64>() / durations.len() as f64)
                .unwrap_or(f64::INFINITY);

            self.model_stats.insert(
                model,
                ModelStats {
                    total,
                    correct,
                    correct_aligned,
                    correct_stripped,
                    error,
                    accuracy,
                    accuracy_aligned,
                    accuracy_stripped,
                    error_rate,
                    avg_input_tokens,
                    avg_output_tokens,
                    avg_logprob,
                    std_logprob,
                    avg_duration,
                },
            );
        }
        self.print_results();
    }

    fn print_results(&self) {
        println!("\n=== MODEL ACCURACY RESULTS ===");
        if self.model_stats.is_empty() {
            println!("No results available");
            return;
        }

        let mut sorted_stats: Vec<_> = self.model_stats.iter().collect();
        sorted_stats.sort_by(|a, b| b.1.accuracy.partial_cmp(&a.1.accuracy).unwrap());

        for (model, stats) in sorted_stats {
            println!("\nModel: {}", model);
            println!(
                "  Accuracy: {:.2}% ({}/{})",
                stats.accuracy * 100.0,
                stats.correct,
                stats.total
            );
            if stats.accuracy_aligned.is_finite() {
                println!(
                    "  Accuracy (aligned): {:.2}% ({}/{})",
                    stats.accuracy_aligned * 100.0,
                    stats.correct_aligned,
                    stats.total
                );
            }
            if stats.accuracy_stripped.is_finite() {
                println!(
                    "  Accuracy (stripped): {:.2}% ({}/{})",
                    stats.accuracy_stripped * 100.0,
                    stats.correct_stripped,
                    stats.total
                );
            }
            if stats.error_rate.is_finite() {
                println!(
                    "  Error Rate: {:.2}% ({}/{})",
                    stats.error_rate * 100.0,
                    stats.error,
                    stats.total
                );
            }
            if stats.avg_input_tokens.is_finite() {
                println!("  Average input tokens: {:.2}", stats.avg_input_tokens);
            }
            if stats.avg_output_tokens.is_finite() {
                println!("  Average output tokens: {:.2}", stats.avg_output_tokens);
            }
            if stats.avg_duration.is_normal() {
                println!("  Average duration: {:.2} s", stats.avg_duration);
            }
            let logprob_type_names = [
                "Average prob",
                "Average prob (errors)",
                "Average prob (incorrect)",
                "Average prob (stripped)",
                "Average prob (aligned)",
                "Average prob (correct)",
            ];
            for (i, &value) in stats.avg_logprob.iter().enumerate() {
                if value.is_finite() {
                    println!(
                        "  {}: {:.1}% (+- {:.1})",
                        logprob_type_names[i],
                        logprob_to_prob(value),
                        stats.std_logprob[i]
                    );
                }
            }
        }
    }

    fn generate_failed_patched_code(resolved_version: &str, patched_code: &str) -> Option<String> {
        if resolved_version == patched_code {
            None
        } else {
            Some(ConflictResolver::create_diff(
                resolved_version,
                patched_code,
                1,
            ))
        }
    }

    pub async fn run_test(
        &mut self,
        config: &Config,
        entries: &[TestEntry],
        args: BenchArgs,
    ) -> Result<()> {
        println!("Running statistics test on {} entries", entries.len());

        let context_lines = ContextLines {
            code_context_lines: args.code_context_lines,
            diff_context_lines: args.diff_context_lines,
            patch_context_lines: args.patch_context_lines,
            extra_conflict_lines: 0,
        };

        // Load existing checkpoint
        self.load_checkpoint(&args)?;
        println!(
            "Loaded {} existing results from checkpoint",
            self.results.len()
        );

        let model_names = self.get_all_model_names(config);

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let exit_code;
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .unwrap();
                tokio::select! {
                   _ = tokio::signal::ctrl_c() => {
                       exit_code = 130;
                   }
                   _ = sigterm.recv() => {
                       exit_code = 143;
                   }
                }
                tx.try_send(exit_code)
                    .unwrap_or_else(|_| std::process::exit(exit_code));
                println!(" Saving checkpoint and exiting...");
            }
        });

        let mut ai_consensus_model_names = std::collections::HashSet::new();
        let mut modified = false;
        for (i, entry) in entries
            .iter()
            .enumerate()
            .take(args.max_entries.unwrap_or(usize::MAX))
        {
            // Skip entries that are already processed
            if self
                .results
                .iter()
                .filter(|r| model_names.contains(&r.model) && r.entry_index == i)
                .count()
                >= model_names.len()
            {
                continue;
            }

            let processing_msg = format!("Processing entry {} of {}...", i + 1, entries.len());
            log::info!("{}", processing_msg);
            println!("{}", processing_msg);

            // Create conflict from test entry
            let git_diff = match self.git_diffs.get(&entry.patch_commit_hash).cloned() {
                Some(git_diff) => git_diff,
                None => {
                    self.git_diffs.clear();
                    let diff = self.git_show_dirs(
                        &args.git_directories,
                        &entry.patch_commit_hash,
                        args.max_context_size,
                        &context_lines,
                    )?;
                    self.git_diffs
                        .insert(entry.patch_commit_hash.clone(), diff.clone());
                    diff
                }
            };

            let conflict = self.create_conflict_from_entry(
                entry,
                args.create_patch,
                args.patch_context_lines,
            )?;

            let resolver = ConflictResolver::new(
                config,
                git_diff,
                true,
                args.get_cache_path(),
                args.cache_overwrite,
                false,
            );

            let resolved_conflicts = resolver.resolve_conflicts(&[conflict], &Vec::new()).await;
            let resolved_conflicts = match resolved_conflicts {
                Ok((resolved_conflicts, resolved_errors)) => {
                    assert!(!(resolved_conflicts.is_empty() && resolved_errors.errors.is_empty()));
                    for (model_name, error_count) in resolved_errors.errors.iter() {
                        let test_result = TestResult {
                            entry_index: i,
                            model: model_name.clone(),
                            correct: false,
                            correct_aligned: false,
                            correct_stripped: false,
                            duration: 0.0,
                            input_tokens: None,
                            output_tokens: None,
                            logprob: None,
                            failed_patched_code: None,
                            error: true,
                            patch_commit_hash: entry.patch_commit_hash.clone(),
                            code_commit_hash: entry.code_commit_hash.clone(),
                        };
                        for _ in 0..*error_count {
                            self.results.push(test_result.clone());
                        }
                    }
                    for resolved_conflict in resolved_conflicts.iter() {
                        let test_result = TestResult {
                            entry_index: i,
                            model: resolved_conflict.model.clone(),
                            correct: resolved_conflict.resolved_version == entry.patched_code,
                            correct_aligned: self
                                .aligned(&resolved_conflict.resolved_version, &entry.patched_code),
                            correct_stripped: self
                                .stripped(&resolved_conflict.resolved_version, &entry.patched_code),
                            duration: resolved_conflict.duration,
                            input_tokens: resolved_conflict.input_tokens,
                            output_tokens: resolved_conflict.output_tokens,
                            logprob: resolved_conflict.logprob,
                            failed_patched_code: Self::generate_failed_patched_code(
                                &resolved_conflict.resolved_version,
                                &entry.patched_code,
                            ),
                            error: false,
                            patch_commit_hash: entry.patch_commit_hash.clone(),
                            code_commit_hash: entry.code_commit_hash.clone(),
                        };
                        self.results.push(test_result);
                    }
                    resolved_conflicts
                }
                Err(e) => anyhow::bail!("Failed to resolve conflicts: {}", e),
            };

            let deduplicated_conflicts = GitUtils::deduplicate_conflicts_vibe(&resolved_conflicts);

            let ai_consensus_model = "AI consensus".to_string();
            let ai_consensus_result = if deduplicated_conflicts.is_empty() {
                TestResult {
                    entry_index: i,
                    model: ai_consensus_model,
                    correct: false,
                    correct_aligned: false,
                    correct_stripped: false,
                    duration: 0.0,
                    input_tokens: None,
                    output_tokens: None,
                    logprob: None,
                    failed_patched_code: None,
                    error: true,
                    patch_commit_hash: entry.patch_commit_hash.clone(),
                    code_commit_hash: entry.code_commit_hash.clone(),
                }
            } else {
                let first_conflict = &deduplicated_conflicts[0];
                for dedup_conflict in &first_conflict.deduplicated_conflicts {
                    ai_consensus_model_names
                        .insert((dedup_conflict.endpoint_index, dedup_conflict.model.clone()));
                }
                TestResult {
                    entry_index: i,
                    model: ai_consensus_model,
                    correct: first_conflict.resolved_version == entry.patched_code,
                    correct_aligned: self
                        .aligned(&first_conflict.resolved_version, &entry.patched_code),
                    correct_stripped: self
                        .stripped(&first_conflict.resolved_version, &entry.patched_code),
                    duration: first_conflict.duration,
                    input_tokens: first_conflict.input_tokens,
                    output_tokens: first_conflict.output_tokens,
                    logprob: first_conflict.logprob,
                    failed_patched_code: Self::generate_failed_patched_code(
                        &first_conflict.resolved_version,
                        &entry.patched_code,
                    ),
                    error: false,
                    patch_commit_hash: entry.patch_commit_hash.clone(),
                    code_commit_hash: entry.code_commit_hash.clone(),
                }
            };
            self.results.push(ai_consensus_result);

            modified = true;
            if let Ok(exit_code) = rx.try_recv() {
                self.save_checkpoint(&args)?;
                std::process::exit(exit_code);
            }
            // Save checkpoint periodically
            if (i + 1) % args.checkpoint_interval == 0 {
                self.save_checkpoint(&args)?;
                modified = false;
            }
        }

        // Save final checkpoint
        if modified {
            self.save_checkpoint(&args)?;
        }

        let mut sorted_model_names: Vec<_> = ai_consensus_model_names.iter().collect();

        // Verify no duplicate model names with different endpoints
        let mut seen_models = std::collections::HashMap::new();
        for (endpoint, model_name) in &sorted_model_names {
            if let Some(existing_endpoint) = seen_models.get(model_name) {
                if existing_endpoint != endpoint {
                    panic!(
                        "Duplicate model name '{}' found with different endpoints: {} and {}",
                        model_name, existing_endpoint, endpoint
                    );
                }
            } else {
                seen_models.insert(model_name.clone(), *endpoint);
            }
        }

        sorted_model_names.sort_by_key(|a| a.0);
        let ai_consensus_model = sorted_model_names
            .iter()
            .map(|(_, model)| model.as_str())
            .collect::<Vec<_>>()
            .join(" + ");
        println!("AI Consensus: {}", ai_consensus_model);

        Ok(())
    }

    fn git_show_dirs(
        &self,
        dirs: &[String],
        commit_hash: &str,
        max_context_size: u32,
        context_lines: &ContextLines,
    ) -> Result<Option<String>> {
        let mut last_error = None;
        for dir in dirs {
            match GitUtils::extract_diff(commit_hash, max_context_size, Some(dir), context_lines) {
                Ok(diff) => return Ok(diff),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("git_diff not found"))
    }

    fn get_all_model_names(&mut self, config: &Config) -> std::collections::HashSet<String> {
        let mut model_names = std::collections::HashSet::new();
        // Collect all model names from endpoints configuration
        for endpoint in config.get_all_endpoints() {
            match &endpoint.config {
                EndpointTypeConfig::OpenAI { variants, .. }
                | EndpointTypeConfig::Anthropic { variants, .. } => {
                    if let Some(variants) = variants {
                        for variant in variants.iter() {
                            let variant_name = if let Some(variant) = &*variant.name {
                                format!("{} ({})", endpoint.name, variant)
                            } else {
                                endpoint.name.clone()
                            };
                            model_names.insert(variant_name);
                        }
                    } else {
                        // No variants, just the endpoint name
                        model_names.insert(endpoint.name.clone());
                    }
                }
                EndpointTypeConfig::Patchpal { n_beams, .. } => {
                    // For patchpal, we have n_beams variants
                    model_names.insert(endpoint.name.to_string());
                    for y in 1..*n_beams {
                        model_names.insert(format!("{} (#{})", endpoint.name, y));
                    }
                }
            }
        }

        assert!(!model_names.is_empty());

        model_names
    }

    fn stripped(&self, resolved: &str, expected: &str) -> bool {
        self.__stripped(resolved) == self.__stripped(expected)
    }

    fn __stripped(&self, s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join("")
    }

    fn aligned(&self, resolved: &str, expected: &str) -> bool {
        self.__aligned(resolved) == self.__aligned(expected)
    }

    fn __aligned(&self, s: &str) -> String {
        s.lines()
            .filter(|line| line.chars().any(|c| !c.is_whitespace()))
            .map(|line| {
                let mut result = String::new();
                let mut seen_non_whitespace = false;
                let mut last_was_whitespace = false;
                for c in line.chars() {
                    if c.is_whitespace() {
                        if !seen_non_whitespace {
                            result.push(c);
                        } else {
                            last_was_whitespace = true;
                        }
                    } else {
                        if last_was_whitespace {
                            result.push(' ');
                        }
                        result.push(c);
                        seen_non_whitespace = true;
                    }
                }
                result
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn create_conflict_from_entry(
        &self,
        entry: &TestEntry,
        create_patch: bool,
        patch_context_lines: u32,
    ) -> Result<Conflict> {
        let mut base_lines = Vec::new();
        let mut remote_lines = Vec::new();
        let mut nr_head_context_lines = 0;
        let mut found_first_change = false;
        let mut line_count = 0;
        for line in entry.patch.split_inclusive('\n') {
            if self.line_number_re.is_match(line) {
                continue;
            }
            if let Some(line) = line.strip_prefix('+') {
                remote_lines.push(line.to_string());
                if !found_first_change {
                    nr_head_context_lines = line_count;
                    found_first_change = true;
                }
                line_count = 0;
                continue;
            } else if let Some(line) = line.strip_prefix('-') {
                base_lines.push(line.to_string());
                if !found_first_change {
                    nr_head_context_lines = line_count;
                    found_first_change = true;
                }
                line_count = 0;
                continue;
            } else if let Some(line) = line.strip_prefix(' ') {
                base_lines.push(line.to_string());
                remote_lines.push(line.to_string());
                line_count += 1;
                continue;
            }
            panic!("malformed patch: {:?}", entry.patch);
        }
        let nr_tail_context_lines = line_count;
        let patch = if create_patch {
            ConflictResolver::create_diff(
                &base_lines.join(""),
                &remote_lines.join(""),
                patch_context_lines,
            )
        } else {
            entry.patch.clone()
        };
        Ok(Conflict {
            file_path: entry.filename.clone(),
            conflict_code: entry.code.clone(),
            conflict_patch: patch,
            nr_head_context_lines,
            nr_tail_context_lines,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_result_serialization() {
        let result = TestResult {
            entry_index: 0,
            model: "test_model".to_string(),
            correct: true,
            correct_aligned: true,
            correct_stripped: true,
            duration: 0.0,
            input_tokens: None,
            output_tokens: None,
            logprob: None,
            failed_patched_code: None,
            error: false,
            patch_commit_hash: "abc123".to_string(),
            code_commit_hash: "def456".to_string(),
        };

        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: TestResult = serde_json::from_str(&serialized).unwrap();

        assert_eq!(result.entry_index, deserialized.entry_index);
        assert_eq!(result.model, deserialized.model);
        assert_eq!(result.correct, deserialized.correct);
        assert_eq!(result.correct_aligned, deserialized.correct_aligned);
        assert_eq!(result.correct_stripped, deserialized.correct_stripped);
        assert_eq!(result.duration, deserialized.duration);
        assert_eq!(result.input_tokens, deserialized.input_tokens);
        assert_eq!(result.output_tokens, deserialized.output_tokens);
        assert_eq!(result.logprob, deserialized.logprob);
        assert_eq!(result.failed_patched_code, deserialized.failed_patched_code);
        assert_eq!(result.error, deserialized.error);
        assert_eq!(result.patch_commit_hash, deserialized.patch_commit_hash);
        assert_eq!(result.code_commit_hash, deserialized.code_commit_hash);
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
