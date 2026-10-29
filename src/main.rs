// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use crate::config::Config;
use crate::conflict_resolver::ConflictResolver;
use crate::git_utils::{ContextLines, GitUtils, ResolutionMode};
use anyhow::Result;
use clap::Parser;

mod api_client;
mod config;
mod conflict_resolver;
mod fast_search;
mod gcp_auth;
mod git_command;
mod git_utils;
mod hunk;
mod lmdb_cache;
mod lmdb_cache_main;
mod logger;
mod patch_locator;
mod prob;
#[cfg(feature = "telemetry")]
mod telemetry;

include!("main_args.rs");

impl Args {
    /// Returns the effective cache path.
    /// If `no_cache` is true, returns `None`.
    /// Otherwise, returns the `cache_path` value.
    pub fn get_cache_path(&self) -> Option<String> {
        if self.no_cache {
            None
        } else {
            Some(self.cache_path.clone())
        }
    }
}

fn import_cache(args: &Args) -> Result<bool> {
    if let Some(import_path) = &args.import_cache {
        use crate::lmdb_cache_main;
        let cache_path = args
            .get_cache_path()
            .ok_or_else(|| anyhow::anyhow!("Cache must be enabled to import cache"))?;
        let cache = lmdb_cache_main::create_from_path(&cache_path, false)?;
        cache.import_from_path(import_path)?;
        println!("Cache imported successfully.");
        return Ok(true);
    }
    Ok(false)
}

#[tokio::main]
async fn main() -> Result<()> {
    logger::log_init();
    // clap std::process::exit(2) on error
    let args = Args::parse();

    // If import_cache is provided, import cache and exit
    if import_cache(&args)? {
        return Ok(());
    }

    // Load configuration
    let config_path = shellexpand::full(&args.config_path)?;
    let config = Config::load(std::path::Path::new(config_path.as_ref()))?;

    log::info!("Using config file: {}", args.config_path);

    // Determine resolution mode
    let resolution_mode = if args.vibe {
        if args.with_markers {
            ResolutionMode::VibeWithMarkers
        } else {
            ResolutionMode::VibeWithPatchLocator
        }
    } else {
        ResolutionMode::Interactive
    };

    let context_lines = ContextLines {
        code_context_lines: args.code_context_lines,
        diff_context_lines: args.diff_context_lines,
        patch_context_lines: args.patch_context_lines,
        extra_conflict_lines: args.extra_conflict_lines,
    };
    // Initialize git utilities
    let mut git_utils = GitUtils::new(
        context_lines,
        args.get_cache_path(),
        args.cache_overwrite,
        resolution_mode,
        args.retries as usize,
        args.continue_op,
        args.allow_empty,
    );

    // Try to cherry-pick with diff3 mode
    let result = git_utils.check_diff3();
    if result.is_err() {
        anyhow::bail!(
            "Diff3 check failed. Run 'git config merge.conflictStyle diff3' to fix this."
        );
    }

    let mut prev_conflicts = Vec::new();
    loop {
        let git_diff = if let Some(commit_hash) = git_utils.find_commit_hash()? {
            log::info!("Extracting diff for commit {}", commit_hash);
            git_utils.extract_diff(&commit_hash, args.max_context_size, None)?
        } else {
            None
        };

        // Check if we're in a cherry-pick and extract commit if needed
        // Check if there are conflicts
        let conflicts = git_utils.find_conflicts(args.max_context_size, &prev_conflicts)?;

        if conflicts.is_empty() {
            println!("No conflicts found.");
            if args.vibe && git_utils.continue_operation(&context_lines)? {
                continue;
            }
            break;
        }

        println!("Found {} conflicts to resolve", conflicts.len());

        // Resolve conflicts using AI
        let resolver = ConflictResolver::new(
            &config,
            git_diff.clone(),
            false,
            args.get_cache_path(),
            args.cache_overwrite,
            git_utils.retries == 0,
        );
        let resolved = resolver
            .resolve_conflicts(&conflicts, &prev_conflicts)
            .await?;
        let (resolved_conflicts, resolved_errors) = resolved;

        let mut repeat = false;
        if args.vibe {
            let no_conflicts_left = git_utils.apply_vibe_resolution(
                &conflicts,
                &resolved_conflicts,
                &resolved_errors.retry_files,
            )?;
            if no_conflicts_left {
                repeat = git_utils.continue_operation(&context_lines)?;
            } else {
                repeat = true;
            }
        } else {
            git_utils.apply_resolved_conflicts(&resolved_conflicts)?;
        }

        #[cfg(feature = "telemetry")]
        {
            let telemetry = telemetry::Telemetry::new(&config, &conflicts, &resolved_conflicts);
            telemetry.submit().await?;
        }

        if !repeat {
            break;
        }

        for r in &resolved_conflicts {
            log::debug!(
                "kept resolved_conflicts: endpoint: {}, local_start: {}, local_end: {}",
                r.endpoint_index,
                r.conflict.local_start,
                r.conflict.local_end
            );
        }
        prev_conflicts = resolved_conflicts;
    }

    if !args.vibe {
        println!(
            "Interactive mode restricts the solution within diff3 conflict markers.\n\
             Use --vibe for enhanced resolution, but always review \
             the result with `git diff --cached`."
        );
    }

    Ok(())
}

// Local Variables:
// rust-format-on-save: t
// End:
