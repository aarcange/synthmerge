// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025  Red Hat, Inc.

use crate::api_client::ApiClient;
use crate::config::{Config, EndpointConfig, EndpointTypeConfig};
use crate::conflict_resolver::{Conflict, ResolvedConflict};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TelemetryData {
    id: Uuid,
    endpoints: Vec<String>,
    version: String,
    nr_conflicts: usize,
    nr_resolved_conflicts: usize,
    nr_deduplicated_resolved_conflicts: usize,
    duration: f64,
}

pub struct Telemetry {
    data: Option<TelemetryData>,
    endpoint: Option<EndpointConfig>,
}

impl Telemetry {
    pub fn new(
        config: &Config,
        conflicts: &BTreeMap<String, Vec<Conflict>>,
        resolved_conflicts: &[ResolvedConflict],
    ) -> Self {
        let endpoints = config.get_all_endpoints();
        let mut patchpal_endpoint = endpoints
            .iter()
            .find(|e| matches!(&e.config, EndpointTypeConfig::Patchpal { .. }));
        if let Some(e) = patchpal_endpoint
            && let EndpointTypeConfig::Patchpal {
                telemetry,
                n_beams: _,
            } = &e.config
            && !telemetry
        {
            patchpal_endpoint = None;
        }

        let mut telemetry = Self {
            data: None,
            endpoint: patchpal_endpoint.cloned(),
        };
        if patchpal_endpoint.is_some() {
            let data = TelemetryData {
                endpoints: endpoints
                    .iter()
                    .map(|e| match &e.config {
                        EndpointTypeConfig::OpenAI { .. } => "openai".to_string(),
                        EndpointTypeConfig::Anthropic { .. } => "anthropic".to_string(),
                        EndpointTypeConfig::Patchpal { .. } => "patchpal".to_string(),
                    })
                    .collect(),
                version: concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"))
                    .to_string(),
                nr_conflicts: conflicts.values().map(|c| c.len()).sum(),
                nr_resolved_conflicts: resolved_conflicts
                    .iter()
                    .map(|c| c.deduplicated_conflicts.len().max(1))
                    .sum(),
                nr_deduplicated_resolved_conflicts: resolved_conflicts.len(),
                duration: resolved_conflicts.iter().map(|c| c.duration).sum(),
                id: Self::create_environment_uuid(),
            };
            telemetry.data = Some(data);
        }
        telemetry
    }

    pub async fn submit(&self) -> Result<()> {
        if let (Some(endpoint), Some(data)) = (self.endpoint.clone(), self.data.clone()) {
            println!("Sending telemetry data to patchpal endpoint");
            log::trace!("Telemetry: {:?}", data);
            self.send_telemetry_patchpal(&endpoint, &data).await?;
        }
        Ok(())
    }

    async fn send_telemetry_patchpal(
        &self,
        endpoint: &EndpointConfig,
        telemetry_data: &TelemetryData,
    ) -> Result<()> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "telemetry",
            "id": 10000,
            "params": { "json": serde_json::to_string(telemetry_data)? }
        });

        let client = ApiClient::create_client(endpoint)?;
        let response = client
            .post(&endpoint.url)
            .headers(headers)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send telemetry to patchpal: {}", e))?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to send telemetry to patchpal: {}",
                response.status()
            ));
        }

        Ok(())
    }

    // Function to create a stable UUID based on environment characteristics
    fn create_environment_uuid() -> Uuid {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();

        // Use hostname if available
        if let Ok(hostname) = hostname::get()
            && let Ok(hostname_str) = hostname.into_string()
        {
            hasher.update(hostname_str);
        }

        // Use machine ID if available (Linux)
        if let Ok(machine_id) = std::fs::read_to_string("/etc/machine-id") {
            hasher.update(machine_id);
        }

        // Use machine ID if available (macOS)
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = std::process::Command::new("ioreg")
                .args(&["-rd1", "-c", "IOPlatformExpertDevice"])
                .output()
            {
                if let Ok(stdout) = std::str::from_utf8(&output.stdout) {
                    if let Some(line) = stdout.lines().find(|line| line.contains("IOPlatformUUID"))
                    {
                        if let Some(uuid) = line.split('"').nth(3) {
                            hasher.update(uuid);
                        }
                    }
                }
            }
        }

        // Use current user
        if let Ok(user) = std::env::var("USER") {
            hasher.update(user);
        }
        // Use current home directory
        if let Ok(home) = std::env::var("HOME") {
            hasher.update(home);
        }

        // Use current uid
        if let Ok(uid) = std::env::var("UID") {
            hasher.update(uid);
        }

        // Use a fixed identifier for the application
        hasher.update(env!("CARGO_PKG_NAME"));

        // Create UUID from the hash
        let hash = hasher.finalize();

        // Hash the bytes again to create a more uniform distribution
        let mut second_hasher = sha2::Sha256::new();
        second_hasher.update(hash);
        let final_hash = second_hasher.finalize();
        let mut final_bytes = [0u8; 16];
        final_bytes.copy_from_slice(&final_hash[..16]);

        // Create UUID from the final hash bytes
        Uuid::from_bytes(final_bytes)
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
