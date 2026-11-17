// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const TOKEN_LIFETIME_SECS: u64 = 3600; // 1 hour token requested from GCP
const CACHE_LIFETIME: Duration = Duration::from_secs(3540); // 59 minutes (60s safety buffer)

#[derive(Debug)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Serialize, Debug)]
struct JwtHeader<'a> {
    alg: &'a str,
    typ: &'a str,
}

#[derive(Serialize, Debug)]
struct JwtPayload<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    exp: u64,
    iat: u64,
}

#[derive(Deserialize, Debug)]
struct TokenResponse {
    access_token: String,
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    fetched_at: Instant,
}

#[derive(Debug)]
pub struct GcpTokenProvider {
    key_path: String,
    scope: String,
    client: Client,
    cached_token: Mutex<Option<CachedToken>>,
}

impl GcpTokenProvider {
    /// Create a new provider from a service account JSON file path.
    pub fn from_file(key_path: impl Into<String>) -> Self {
        Self::from_file_with_scope(key_path, DEFAULT_SCOPE)
    }

    /// Create a new provider from a service account JSON file path with a custom OAuth2 scope.
    pub fn from_file_with_scope(key_path: impl Into<String>, scope: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client for GCP token provider");

        Self {
            key_path: key_path.into(),
            scope: scope.into(),
            client,
            cached_token: Mutex::new(None),
        }
    }

    /// Returns a valid OAuth2 access token, refreshing it over network if older than ~1 hour.
    pub async fn get_token(&self) -> Result<String> {
        let mut guard = self.cached_token.lock().await;

        if let Some(ref cached) = *guard
            && cached.fetched_at.elapsed() < CACHE_LIFETIME
        {
            return Ok(cached.token.clone());
        }

        let token = self.fetch_new_token().await?;
        *guard = Some(CachedToken {
            token: token.clone(),
            fetched_at: Instant::now(),
        });

        Ok(token)
    }

    async fn fetch_new_token(&self) -> Result<String> {
        let expanded = shellexpand::full(&self.key_path)?;
        let key_path = Path::new(expanded.as_ref());

        let contents = std::fs::read_to_string(key_path).with_context(|| {
            format!("Failed to read service account key: {}", key_path.display())
        })?;

        let value: Value = serde_json::from_str(&contents).with_context(|| {
            format!(
                "Failed to parse service account JSON: {}",
                key_path.display()
            )
        })?;

        let sa_key = Self::find_service_account_key(&value).with_context(|| {
            format!(
                "Failed to find service account key in JSON: {}",
                key_path.display()
            )
        })?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System time before UNIX epoch")?
            .as_secs();

        // 1. Build and Base64URL encode header
        let header = JwtHeader {
            alg: "RS256",
            typ: "JWT",
        };
        let header_json = serde_json::to_vec(&header)?;
        let header_b64 = URL_SAFE_NO_PAD.encode(&header_json);

        // 2. Build and Base64URL encode payload
        let payload = JwtPayload {
            iss: &sa_key.client_email,
            scope: &self.scope,
            aud: &sa_key.token_uri,
            exp: now + TOKEN_LIFETIME_SECS,
            iat: now,
        };
        let payload_json = serde_json::to_vec(&payload)?;
        let payload_b64 = URL_SAFE_NO_PAD.encode(&payload_json);

        // 3. Create signing input and sign with RS256
        let signing_input = format!("{}.{}", header_b64, payload_b64);
        let signature_b64 = Self::sign_rs256(&sa_key.private_key, signing_input.as_bytes())?;

        let jwt = format!("{}.{}", signing_input, signature_b64);

        // 4. Exchange JWT for access token
        let response = self
            .client
            .post(&sa_key.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &jwt),
            ])
            .send()
            .await
            .context("Failed to send token request to GCP")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<empty>".to_string());
            bail!("GCP token exchange failed with status {}: {}", status, body);
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse GCP token JSON response")?;

        Ok(token_response.access_token)
    }

    fn find_service_account_key(value: &Value) -> Result<ServiceAccountKey> {
        fn is_service_account_key(obj: &Value) -> bool {
            obj.get("client_email").and_then(Value::as_str).is_some()
                && obj.get("private_key").and_then(Value::as_str).is_some()
        }

        fn search(value: &Value) -> Option<&Value> {
            match value {
                Value::Object(map) => {
                    if is_service_account_key(value) {
                        return Some(value);
                    }
                    for (_, v) in map {
                        if let Some(found) = search(v) {
                            return Some(found);
                        }
                    }
                    None
                }
                Value::Array(arr) => {
                    for v in arr {
                        if let Some(found) = search(v) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }

        let found = search(value).context("No object with required keys found in JSON")?;
        let client_email = found
            .get("client_email")
            .and_then(Value::as_str)
            .context("Missing client_email")?
            .to_string();
        let private_key = found
            .get("private_key")
            .and_then(Value::as_str)
            .context("Missing private_key")?
            .to_string();
        let token_uri = found
            .get("token_uri")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or(DEFAULT_TOKEN_URI.to_string());

        Ok(ServiceAccountKey {
            client_email,
            private_key,
            token_uri,
        })
    }

    fn sign_rs256(private_key_pem: &str, data: &[u8]) -> Result<String> {
        let private_key_pem = if private_key_pem.starts_with("-----BEGIN PRIVATE KEY-----\\n") {
            private_key_pem.replace("\\n", "\n")
        } else {
            private_key_pem.to_string()
        };
        let private_key = RsaPrivateKey::from_pkcs8_pem(&private_key_pem)?;

        let signing_key = SigningKey::<Sha256>::new(private_key);
        let signature = signing_key.sign(data);

        Ok(URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
