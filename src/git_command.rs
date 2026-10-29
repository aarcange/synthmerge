// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use anyhow::{Context, Result};
use std::process::Command;

// Wrapper around Command to allow inheritance-like behavior
pub struct GitCommand {
    command: Command,
    verbose: bool,
}

impl GitCommand {
    pub fn new(program: &str) -> Self {
        let cmd = Command::new(program);
        GitCommand {
            command: cmd,
            verbose: true,
        }
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command.args(args);
        self
    }

    pub fn verbose(&mut self, verbose: bool) -> &mut Self {
        self.verbose = verbose;
        self
    }

    pub fn output(&mut self) -> Result<std::process::Output> {
        let program = self.command.get_program().to_string_lossy().into_owned();
        let args: Vec<_> = self.command.get_args().collect();
        let args_str = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let output = self.command.output().context("Failed to execute command")?;
        if self.verbose {
            log::debug!("GitCommand: {program} {args_str} {{{}}}", output.status);
            if !output.status.success() {
                log::debug!("stdout: {}", String::from_utf8_lossy(&output.stdout));
                log::debug!("stderr: {}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Ok(output)
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
