// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2025-2026  Red Hat, Inc.

use crate::config::{
    EndpointConfig, EndpointContextElement, EndpointContextLayout, EndpointGrammar, EndpointJson,
    EndpointTypeConfig, EndpointVariants,
};
use crate::conflict_resolver::ConflictResolver;
use crate::lmdb_cache::{ApiCache, LmdbCacheImpl};
use crate::prob;
use anyhow::{Context, Result, bail};
use reqwest::Certificate;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct ApiRequest {
    pub prompt: String,
    pub message: String,
    pub patch: String,
    pub code: String,
    pub git_diff: Option<String>,
    pub training: String,
}

#[derive(Clone, Debug)]
pub struct ApiResponseEntry {
    pub response: String,
    pub logprob: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration: f64,
}

#[derive(Debug)]
pub enum ApiRequestError {
    ExceedContextSize,
    UsageLimitExceeded,
    ContentFilterRecitation,
    IncompleteGeneration,
}

impl fmt::Display for ApiRequestError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ApiRequestError::ExceedContextSize => write!(f, "Exceed context size"),
            ApiRequestError::UsageLimitExceeded => write!(f, "Usage limit exceeded"),
            ApiRequestError::ContentFilterRecitation => write!(f, "Content filter: recitation"),
            ApiRequestError::IncompleteGeneration => write!(f, "Incomplete generation"),
        }
    }
}

macro_rules! get_context_field {
    ($endpoint_context:expr, $variant_context:expr, $field:ident, $default:expr) => {{
        let endpoint_value = $endpoint_context
            .as_ref()
            .and_then(|context| context.$field.clone());
        let variant_value = $variant_context.as_ref().and_then(|ctx| ctx.$field.clone());
        endpoint_value.or(variant_value).unwrap_or($default)
    }};
}

pub type ApiResponse = Vec<Vec<Result<ApiResponseEntry>>>;

pub struct ApiClient {
    endpoint: EndpointConfig,
    client: reqwest::Client,
    lmdb_cache: Option<Arc<LmdbCacheImpl>>,
}

impl ApiClient {
    pub fn new(endpoint: EndpointConfig, lmdb_cache: Option<Arc<LmdbCacheImpl>>) -> Self {
        let client = Self::create_client(&endpoint);

        ApiClient {
            endpoint,
            client: client.expect("Failed to create client"),
            lmdb_cache,
        }
    }

    pub fn create_client(endpoint: &EndpointConfig) -> Result<reqwest::Client> {
        let mut builder =
            reqwest::Client::builder().timeout(Duration::from_millis(endpoint.timeout));

        // Add root certificate if specified
        if let Some(cert_path) = &endpoint.root_certificate_pem {
            let cert_path = shellexpand::full(cert_path)?;
            let mut buf = Vec::new();
            File::open(cert_path.as_ref())
                .and_then(|mut file| file.read_to_end(&mut buf))
                .map_err(|e| {
                    anyhow::anyhow!("Failed to read certificate file {}: {}", cert_path, e)
                })?;
            let cert = Certificate::from_pem(&buf).map_err(|e| {
                anyhow::anyhow!("Failed to parse certificate from {}: {}", cert_path, e)
            })?;
            builder = builder.add_root_certificate(cert);
            log::trace!("Root certificate loaded successfully from {}", cert_path);
        }

        builder
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build client: {}", e))
    }

    /// Query the AI endpoint with the given prompt
    pub async fn query(&self, api_request: &ApiRequest) -> Result<(ApiResponse, Option<usize>)> {
        let result = self.query_endpoint(api_request).await;

        let mut last_error = match result {
            Ok(response) => {
                if self.endpoint.fallbacks.is_empty()
                    || !response.iter().flatten().all(Result::is_err)
                {
                    return Ok((response, None));
                };
                anyhow::anyhow!("All variants failed, trying fallbacks")
            }
            Err(e) => e,
        };
        for (fallback_index, fallback) in self.endpoint.fallbacks.iter().enumerate() {
            let client = ApiClient::new(fallback.clone(), self.lmdb_cache.clone());
            match client.query_endpoint(api_request).await {
                Ok(response) => return Ok((response, Some(fallback_index))),
                Err(e) => last_error = e,
            }
        }

        Err(last_error)
    }

    async fn query_endpoint(&self, api_request: &ApiRequest) -> Result<ApiResponse> {
        match &self.endpoint.config {
            EndpointTypeConfig::OpenAI { .. } => self.query_openai(api_request).await,
            EndpointTypeConfig::Patchpal { .. } => self.query_patchpal(api_request).await,
            EndpointTypeConfig::Anthropic { .. } => self.query_anthropic(api_request).await,
        }
    }

    async fn read_api_key(&self, api_key_file: &String) -> Result<String> {
        Ok(
            std::fs::read_to_string(shellexpand::full(api_key_file)?.as_ref())
                .context("Failed to read API key file")?
                .trim()
                .to_string(),
        )
    }

    async fn create_headers(&self) -> Result<reqwest::header::HeaderMap> {
        let mut headers = reqwest::header::HeaderMap::new();
        if !self.endpoint.no_user_agent {
            let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            let user_agent = if let Some(suffix) = &*self.endpoint.user_agent_suffix {
                format!("{} ({})", user_agent, suffix)
            } else {
                user_agent
            };
            headers.insert(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_str(&user_agent)
                    .context("Invalid User-Agent")?,
            );
        }
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        if let Some(gcp_token_provider) = &self.endpoint.gcp_token_provider {
            let token = gcp_token_provider.get_token().await?;
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                    .context("Invalid GCP token")?,
            );
        } else if let Some(api_key_file) = &*self.endpoint.api_key_file {
            // Only add the Authorization header if an API key file is specified
            let api_key = self.read_api_key(api_key_file).await?;
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .context("Invalid API key")?,
            );
        }
        if let Some(x_api_key_file) = &*self.endpoint.x_api_key_file {
            // Only add the X-API-Key header if an API key file is specified
            let api_key = self.read_api_key(x_api_key_file).await?;
            headers.insert(
                reqwest::header::HeaderName::from_static("x-api-key"),
                reqwest::header::HeaderValue::from_str(&api_key).context("Invalid X-API-Key")?,
            );
        }
        if let Some(endpoint_headers) = &self.endpoint.headers {
            for (key, value) in &endpoint_headers.headers {
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(key.as_bytes())
                        .with_context(|| format!("Invalid header name: {}", key))?,
                    reqwest::header::HeaderValue::from_str(value.as_str().ok_or_else(|| {
                        anyhow::anyhow!("Header '{}' value must be a string", key)
                    })?)
                    .with_context(|| format!("Invalid header value for: {}", key))?,
                );
            }
        }
        log::trace!("Headers: {:?}", headers);
        Ok(headers)
    }

    fn clear_signature_fields(json: &mut serde_json::Value) {
        match json {
            serde_json::Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    if key.to_lowercase().contains("signature") {
                        *value = serde_json::Value::Null;
                    } else {
                        Self::clear_signature_fields(value);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    Self::clear_signature_fields(item);
                }
            }
            _ => {}
        }
    }

    fn log_response_json(&self, response_text: &str, json_response: &mut serde_json::Value) {
        Self::clear_signature_fields(json_response);
        log::trace!(
            "Response JSON ({}):\n{}",
            self.endpoint.name,
            serde_json::to_string_pretty(json_response).unwrap_or(response_text.to_string())
        );
    }

    async fn query_openai_variant(
        &self,
        request: &ApiRequest,
        variant: &EndpointVariants,
        no_chat: &bool,
        grammar: &Option<EndpointGrammar>,
        perplexity: &mut Vec<String>,
    ) -> Result<ApiResponseEntry> {
        let mut chat = self.create_chat(request, variant);
        let mut perplexity_search = None;
        assert!(chat.len().is_multiple_of(2), "{}", chat.len());
        if !perplexity.is_empty() {
            perplexity_search = Some(perplexity.remove(0));
            chat.push(perplexity_search.clone());
        }
        let mut payload = if !*no_chat {
            let mut payload = serde_json::json!({ "messages": [], });
            let messages = payload["messages"].as_array_mut().unwrap();
            for (i, msg) in chat.iter().enumerate().filter(|(_, s)| s.is_some()) {
                let role = if i == 0 {
                    "system"
                } else if i % 2 == 1 {
                    "user"
                } else {
                    "assistant"
                };
                messages.push(serde_json::json!({
                    "role": role,
                    "content": msg
                }));
            }
            payload
        } else {
            let prompt = chat
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_some())
                //.filter(|(i, s)| s.is_some() && (*i == 0 || i % 2 == 1))
                .map(|(_, s)| s.as_ref().unwrap().clone())
                .collect::<Vec<_>>()
                .join("\n\n")
                + "\n\n";
            serde_json::json!({ "prompt": prompt })
        };

        self.apply_parameters(&mut payload, &self.endpoint.json)?;
        self.apply_parameters(&mut payload, &variant.json)?;
        if perplexity_search.is_some() {
            payload.as_object_mut().unwrap().remove("n_probs");
        } else if let Some(grammar) = grammar {
            let (key, value) = match grammar {
                EndpointGrammar::Gbnf => (
                    "grammar",
                    serde_json::json!(format!(
                        r#"root ::= "{}\n" .*"#,
                        ConflictResolver::PATCHED_CODE_START
                    )),
                ),
                EndpointGrammar::Ebnf => (
                    "structured_outputs",
                    serde_json::json!({
                        "grammar": format!(
                            r#"root ::= "{}\n" [^]*"#,
                            ConflictResolver::PATCHED_CODE_START
                        ),
                    }),
                ),
            };
            self.apply_parameters(
                &mut payload,
                &Some(EndpointJson {
                    json: std::collections::HashMap::from([(key.to_string(), value)]),
                }),
            )?;
        }

        self.retry_request_perplexity_search(
            &self.endpoint.url,
            &payload,
            perplexity,
            |response_text: &str,
            perplexity: &mut Vec<String>,
            duration: f64| -> Result<ApiResponseEntry> {
                // Parse JSON response to extract the content
                let mut json_response: serde_json::Value = serde_json::from_str(response_text)
                    .map_err(|e| {
                        if response_text.contains("Usage limit exceeded") {
                            anyhow::anyhow!(ApiRequestError::UsageLimitExceeded)
                        } else {
                            log::warn!("Failed to parse JSON response:\n{}", response_text);
                            anyhow::anyhow!("Failed to parse JSON response: {}", e)
                        }
                    })?;
                self.log_response_json(response_text, &mut json_response);

                // Check for context size error in OpenAI responses
                if let Some(error) = json_response.get("error")
                    && let Some(error_type) = error.get("type").and_then(|v| v.as_str())
                    && error_type == "exceed_context_size_error"
                {
                    log::warn!(
                        "Context size error for endpoint {}. Not retrying.",
                        self.endpoint.name
                    );
                    bail!(ApiRequestError::ExceedContextSize);
                }

                // Check for context size error in Gemini responses (array format)
                if let Some(errors) = json_response.as_array()
                    && let Some(first_error) = errors.first()
                    && let Some(error_obj) = first_error.get("error")
                    && let Some(message) = error_obj.get("message").and_then(|v| v.as_str())
                    && message.contains("exceeds the maximum number of tokens allowed")
                {
                    log::warn!(
                        "Context size error for endpoint {}. Not retrying.",
                        self.endpoint.name
                    );
                    bail!(ApiRequestError::ExceedContextSize);
                }

                // Check for content filter recitation error in OpenAI responses
                if let Some(choices) = json_response.get("choices").and_then(|c| c.as_array())
                    && let Some(choice) = choices.first()
                    && let Some(finish_reason) =
                    choice.get("finish_reason").and_then(|v| v.as_str())
                    && finish_reason == "content_filter: RECITATION"
                {
                    log::warn!(
                        "Content filter recitation error for endpoint {}. Not retrying.",
                        self.endpoint.name
                    );
                    bail!(ApiRequestError::ContentFilterRecitation);
                }

                // Check for incomplete generation in OpenAI responses
                if let Some(choices) = json_response.get("choices").and_then(|c| c.as_array())
                    && let Some(choice) = choices.first()
                    && let Some(finish_reason) =
                    choice.get("finish_reason").and_then(|v| v.as_str())
                    && finish_reason != "stop"
                {
                    log::warn!(
                        "Incomplete generation error for endpoint {}. Not retrying. Finish reason: {}",
                        self.endpoint.name,
                        finish_reason
                    );
                    log::warn!(
                        "Response JSON ({}):\n{}",
                        self.endpoint.name,
                        serde_json::to_string_pretty(&json_response).unwrap()
                    );
                    bail!(ApiRequestError::IncompleteGeneration);
                }

                let content = json_response
                    .get("choices")
                    .and_then(|choices| choices.get(0))
                    .and_then(|choice| choice.get("message"))
                    .and_then(|message| message.get("content"))
                    .or_else(|| {
                        json_response
                            .get("choices")
                            .and_then(|choices| choices.get(0))
                            .and_then(|choice| choice.get("text"))
                    })
                    .and_then(|content| content.as_str())
                    .with_context(|| {
                        log::warn!(
                            "Failed to extract content from response:\n{}",
                            serde_json::to_string_pretty(&json_response).unwrap()
                        );
                        "Failed to extract content from response"
                    })?;

                let logprob = if perplexity_search.is_some() {
                    None
                } else {
                    prob::logprob(&json_response, perplexity)
                };

                let input_tokens = json_response
                    .get("usage")
                    .and_then(|usage| usage.get("prompt_tokens"))
                    .and_then(|tokens| tokens.as_u64());

                let output_tokens = json_response
                    .get("usage")
                    .and_then(|usage| usage.get("total_tokens"))
                    .and_then(|tokens|
                              tokens.as_u64()
                              .map(|total| total.saturating_sub(input_tokens.unwrap_or(0))));

                let completion_tokens = json_response
                    .get("usage")
                    .and_then(|usage| usage.get("completion_tokens"))
                    .and_then(|tokens| tokens.as_u64());
                if completion_tokens != output_tokens {
                    log::info!(
                        "Completion tokens ({:?}) don't match output_tokens ({:?})",
                        completion_tokens,
                        output_tokens
                    );
                }

                let mut response_entry = ApiResponseEntry {
                    response: content.to_string(),
                    logprob,
                    input_tokens,
                    output_tokens,
                    duration,
                };
                if let Some(prefix) = &perplexity_search {
                    response_entry.response = format!("{}{}", prefix, response_entry.response);
                }
                Ok(response_entry)
            },
        )
            .await
    }

    async fn query_openai(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let (variants, no_chat, grammar) = match &self.endpoint.config {
            EndpointTypeConfig::OpenAI {
                variants,
                no_chat,
                grammar,
                ..
            } => (variants, no_chat, grammar),
            _ => panic!("cannot happen"),
        };

        // Handle EndpointVariants - if None, create a single entry with no parameters
        let variants_list = match variants {
            Some(variants) => variants,
            None => &vec![EndpointVariants::default()],
        };

        let mut responses = Vec::new();

        for variant in variants_list {
            let mut perplexity = Vec::<String>::new();
            let mut variant_responses = Vec::new();
            loop {
                variant_responses.push(
                    self.query_openai_variant(request, variant, no_chat, grammar, &mut perplexity)
                        .await,
                );
                if perplexity.is_empty() {
                    break;
                }
            }
            responses.push(variant_responses)
        }

        Ok(responses)
    }

    async fn query_anthropic_variant(
        &self,
        request: &ApiRequest,
        variant: &EndpointVariants,
    ) -> Result<ApiResponseEntry> {
        let chat = self.create_chat(request, variant);

        let mut payload = serde_json::json!({
            "system": chat[0],
            "messages": [],
        });
        let messages = payload["messages"].as_array_mut().unwrap();
        for (i, msg) in chat[1..].iter().enumerate().filter(|(_, s)| s.is_some()) {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            messages.push(serde_json::json!({
                "role": role,
                "content": [{"type": "text", "text": msg}]
            }));
        }

        self.apply_parameters(&mut payload, &self.endpoint.json)?;
        self.apply_parameters(&mut payload, &variant.json)?;

        self.retry_request(
            &self.endpoint.url,
            &payload,
            |response_text: &str, _: &mut Vec<String>, duration: f64| -> Result<ApiResponseEntry> {
                // Parse JSON response to extract the content
                let mut json_response: serde_json::Value = serde_json::from_str(response_text)
                    .map_err(|e| {
                        if response_text.contains("Usage limit exceeded") {
                            anyhow::anyhow!(ApiRequestError::UsageLimitExceeded)
                        } else {
                            log::warn!("Failed to parse JSON response:\n{}", response_text);
                            anyhow::anyhow!("Failed to parse JSON response: {}", e)
                        }
                    })?;
                self.log_response_json(response_text, &mut json_response);

                // Check for context size error in Anthropic responses
                if let Some(error) = json_response.get("error")
                    && let Some(error_type) = error.get("type").and_then(|v| v.as_str())
                    && error_type == "invalid_request_error"
                    && let Some(message) = error.get("message").and_then(|v| v.as_str())
                    && (message.contains("Request size exceeds model context window") ||
                        message.contains("prompt is too long"))
                {
                    log::warn!(
                        "Context size error for endpoint {}. Not retrying.",
                        self.endpoint.name
                    );
                    bail!(ApiRequestError::ExceedContextSize);
                }

                // Check for incomplete generation in Anthropic responses
                if let Some(stop_reason) = json_response.get("stop_reason").and_then(|v| v.as_str())
                    && stop_reason != "end_turn"
                {
                    log::warn!(
                        "Incomplete generation error for endpoint {}. Not retrying. Stop reason: {}",
                        self.endpoint.name,
                        stop_reason
                    );
                    log::warn!(
                        "Response JSON ({}):\n{}",
                        self.endpoint.name,
                        serde_json::to_string_pretty(&json_response).unwrap()
                    );
                    bail!(ApiRequestError::IncompleteGeneration);
                }

                let content = json_response
                    .get("content")
                    .and_then(|choices| choices.as_array())
                    .and_then(|choices| {
                        choices.iter().find(|choice| {
                            choice.get("type").and_then(|v| v.as_str()) == Some("text")
                        })
                        .and_then(|choice| {
                            choice.get("text").and_then(|text| text.as_str())
                        })
                    })
                    .with_context(|| {
                        log::warn!(
                            "Failed to extract content from response:\n{}",
                            serde_json::to_string_pretty(&json_response).unwrap()
                        );
                        "Failed to extract content from response"
                    })?;

                let mut perplexity = Vec::<String>::new();
                let logprob = prob::logprob(&json_response, &mut perplexity);

                let input_tokens = json_response
                    .get("usage")
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(|tokens| tokens.as_u64());
                let output_tokens = json_response
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(|tokens| tokens.as_u64());

                Ok(ApiResponseEntry {
                    response: content.to_string(),
                    logprob,
                    input_tokens,
                    output_tokens,
                    duration,
                })
            },
        ).await
    }

    async fn query_anthropic(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let variants = match &self.endpoint.config {
            EndpointTypeConfig::Anthropic { variants, .. } => variants,
            _ => panic!("cannot happen"),
        };

        // Handle EndpointVariants - if None, create a single entry with no parameters
        let variants_list = match variants {
            Some(variants) => variants,
            None => &vec![EndpointVariants::default()],
        };

        let mut responses = Vec::new();

        for variant in variants_list {
            responses.push(vec![self.query_anthropic_variant(request, variant).await]);
        }

        Ok(responses)
    }

    fn create_chat(&self, request: &ApiRequest, variant: &EndpointVariants) -> Vec<Option<String>> {
        let mut chat = Vec::new();

        let layout = get_context_field!(
            &self.endpoint.context,
            &variant.context,
            layout,
            EndpointContextLayout::default()
        );
        let no_diff = get_context_field!(&self.endpoint.context, &variant.context, no_diff, false);
        let no_training =
            get_context_field!(&self.endpoint.context, &variant.context, no_training, false);
        let mut system_message = String::new();
        let mut user_message = String::new();

        let push = |s: &mut String, text: &str, need_newline: &mut bool| {
            if *need_newline {
                s.push_str("\n\n");
            } else {
                *need_newline = true;
            }
            s.push_str(text);
        };

        for (message, context) in [
            (&mut system_message, layout.system_message),
            (&mut user_message, layout.user_message),
        ] {
            let mut need_newline = false;
            for element in context.into_iter() {
                match element {
                    EndpointContextElement::Prompt => {
                        push(message, &request.prompt, &mut need_newline)
                    }
                    EndpointContextElement::Training => {
                        if !no_training {
                            push(message, &request.training, &mut need_newline)
                        }
                    }
                    EndpointContextElement::Diff => {
                        if !no_diff && let Some(git_diff) = &request.git_diff {
                            push(message, git_diff, &mut need_newline)
                        }
                    }
                }
            }
            assert!(!message.chars().next().is_some_and(char::is_whitespace));
            assert!(!message.chars().last().is_some_and(char::is_whitespace));
        }

        if !user_message.is_empty() {
            user_message.push_str("\n\n");
        }
        user_message.push_str(&request.message);

        chat.push(Some(system_message));
        chat.push(Some(user_message));

        log::debug!("Chat[0]:\n{}", chat[0].as_ref().unwrap_or(&String::new()));
        log::info!(
            "{}",
            chat.iter()
                .enumerate()
                .skip(1)
                .filter(|(_, s)| s.is_some())
                .map(|(i, s)| format!("Chat[{}]:\n{}", i, s.as_ref().unwrap()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        chat
    }

    fn apply_parameters(
        &self,
        payload: &mut serde_json::Value,
        json: &Option<EndpointJson>,
    ) -> Result<()> {
        // Apply parameters from EndpointVariants JSON
        if let Some(json) = json {
            for (key, value) in &json.json {
                if payload.get(key).is_some() {
                    return Err(anyhow::anyhow!("Key {} already present in payload", key));
                }
                payload[key] = value.clone();
            }
        }
        Ok(())
    }

    async fn query_patchpal(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        let payload = serde_json::json!({"jsonrpc": "2.0",
                                         "method": "inference",
                                         "params" : {"patch" : request.patch,
                                                     "code" : request.code}});

        let response_handler =
            |response_text: &str, _: &mut Vec<String>, duration: f64| -> Result<ApiResponse> {
                // Try to parse as JSON and extract content
                let mut json_response: serde_json::Value = serde_json::from_str(response_text)
                    .map_err(|e| {
                        if response_text.contains("Usage limit exceeded") {
                            anyhow::anyhow!(ApiRequestError::UsageLimitExceeded)
                        } else {
                            log::warn!("Failed to parse JSON response:\n{}", response_text);
                            anyhow::anyhow!("Failed to parse JSON response: {}", e)
                        }
                    })?;
                self.log_response_json(response_text, &mut json_response);

                if json_response.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
                    log::warn!(
                        "Invalid patchpal jsonrpc version:\n{}",
                        serde_json::to_string_pretty(&json_response).unwrap()
                    );
                    return Err(anyhow::anyhow!("Invalid patchpal jsonrpc version"));
                }

                // Check for RPC errors in the response
                if let Some(error) = json_response.get("error") {
                    let error_code = error.get("code").and_then(|v| v.as_i64());
                    let error_message = error.get("message").and_then(|v| v.as_str());
                    if error_message.is_some_and(|msg| msg.contains("out of memory")) {
                        log::error!(
                            "Patchpal RPC error: code={}, message={}",
                            error_code.map_or("".to_string(), |c| c.to_string()),
                            error_message.unwrap_or("unknown error")
                        );
                        return Err(anyhow::anyhow!(ApiRequestError::ExceedContextSize));
                    }
                    log::error!(
                        "Patchpal RPC error: code={}, message={}",
                        error_code.map_or("-1".to_string(), |c| c.to_string()),
                        error_message.unwrap_or("unknown error")
                    );
                    return Err(anyhow::anyhow!(
                        "Patchpal RPC error: {}",
                        error_message.unwrap_or("unknown error")
                    ));
                }

                let n_beams = match &self.endpoint.config {
                    EndpointTypeConfig::Patchpal { n_beams, .. } => *n_beams,
                    _ => panic!("cannot happen"),
                };
                let responses: Vec<_> = json_response
                    .get("result")
                    .and_then(|v| v.as_array())
                    .context("Failed to extract content from patchpal response")?
                    .iter()
                    .take(n_beams as usize)
                    .map(|v| {
                        (
                            v.get(0)
                                .and_then(|v| v.as_str())
                                .context("Failed to extract patched code from patchpal response"),
                            v.get(1)
                                .and_then(|v| v.as_f64())
                                .context("Failed to extract logprobs from patchpal response"),
                        )
                    })
                    .map(|s| -> Result<ApiResponseEntry> {
                        Ok(ApiResponseEntry {
                            response: format!(
                                "{}\n{}{}",
                                ConflictResolver::PATCHED_CODE_START,
                                s.0?,
                                ConflictResolver::PATCHED_CODE_END
                            ),
                            logprob: s.1.ok(),
                            input_tokens: None,
                            output_tokens: None,
                            duration,
                        })
                    })
                    .collect();
                if responses.iter().any(Result::is_err) {
                    log::warn!(
                        "Failed to extract content from patchpal response:\n{}",
                        serde_json::to_string_pretty(&json_response).unwrap()
                    );
                }

                Ok(vec![responses])
            };

        self.retry_request(&self.endpoint.url, &payload, response_handler)
            .await
    }

    async fn retry_request_perplexity_search<F, R>(
        &self,
        url: &str,
        payload: &serde_json::Value,
        perplexity: &mut Vec<String>,
        response_handler: F,
    ) -> Result<R>
    where
        F: Fn(&str, &mut Vec<String>, f64) -> Result<R>,
    {
        let start = std::time::Instant::now();
        let mut last_error = None;
        let mut delay = Duration::from_millis(self.endpoint.delay);
        let max_delay = Duration::from_millis(self.endpoint.max_delay);

        log::trace!(
            "Request JSON ({}):\n{}",
            self.endpoint.name,
            serde_json::to_string_pretty(payload).unwrap()
        );

        let cache_key = match &self.lmdb_cache {
            Some(cache) => {
                let key = cache.get_cache_key(&[url, &serde_json::to_string(payload).unwrap()]);
                if let Some(response_text) = cache.get_cached_response(&key) {
                    let api_response = response_handler(&response_text, perplexity, 0.);
                    match api_response {
                        Ok(r) => return Ok(r),
                        Err(e) => {
                            if let Some(
                                ApiRequestError::ExceedContextSize
                                | ApiRequestError::ContentFilterRecitation
                                | ApiRequestError::IncompleteGeneration,
                            ) = e.downcast_ref::<ApiRequestError>()
                            {
                                return Err(e);
                            } else {
                                bail!(
                                    "Cached response failed with recoverable error for {}: {}",
                                    self.endpoint.name,
                                    e
                                );
                            }
                        }
                    }
                }
                Some(key)
            }
            None => None,
        };

        for _ in 0..self.endpoint.retries {
            let headers = match self.create_headers().await {
                Ok(headers) => headers,
                Err(e) => {
                    log::warn!(
                        "Failed to create headers for endpoint {}: {}",
                        self.endpoint.name,
                        e
                    );
                    self.apply_delay(&mut delay, max_delay, &e).await;
                    last_error = Some(e);
                    continue;
                }
            };
            let response = self
                .client
                .post(url.to_string())
                .headers(headers)
                .json(payload)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let response_text = match response.text().await {
                        Ok(text) => text,
                        Err(e) => {
                            self.apply_delay(&mut delay, max_delay, &e).await;
                            last_error = Some(e.into());
                            continue;
                        }
                    };
                    let duration = start.elapsed().as_secs_f64();
                    match response_handler(&response_text, perplexity, duration) {
                        Ok(api_response) => {
                            if let (Some(cache), Some(key)) = (&self.lmdb_cache, &cache_key) {
                                cache.cache_response(key, response_text)?;
                            }
                            self.apply_wait().await;
                            return Ok(api_response);
                        }
                        Err(e) => {
                            if let Some(api_error) = e.downcast_ref::<ApiRequestError>() {
                                match api_error {
                                    ApiRequestError::ExceedContextSize
                                    | ApiRequestError::ContentFilterRecitation
                                    | ApiRequestError::IncompleteGeneration => {
                                        // If it's a context size error, don't retry
                                        if let (Some(cache), Some(key)) =
                                            (&self.lmdb_cache, &cache_key)
                                        {
                                            cache.cache_response(key, response_text)?;
                                        }
                                        self.apply_wait().await;
                                        return Err(e);
                                    }
                                    ApiRequestError::UsageLimitExceeded => {}
                                }
                            }
                            self.apply_delay(&mut delay, max_delay, &e).await;
                            last_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    let mut source = std::error::Error::source(&e);
                    let mut is_hyper_error = false;
                    while let Some(err) = source {
                        if err
                            .downcast_ref::<hyper_util::client::legacy::Error>()
                            .is_some()
                        {
                            is_hyper_error = true;
                            break;
                        }
                        source = err.source();
                    }
                    if e.is_timeout() && !is_hyper_error {
                        // Don't retry on timeout errors or it may waste energy
                        log::warn!(
                            "Timeout error for endpoint {} after {:.1}s (configured timeout: {}ms): {e:?}",
                            self.endpoint.name,
                            start.elapsed().as_secs_f64(),
                            self.endpoint.timeout
                        );
                        self.apply_wait().await;
                        return Err(e.into());
                    }
                    self.apply_delay(&mut delay, max_delay, &e).await;
                    last_error = Some(e.into());
                }
            }
        }
        Err(last_error.context("Failed to send request after retries")?)
    }

    async fn retry_request<F, R>(
        &self,
        url: &str,
        payload: &serde_json::Value,
        response_handler: F,
    ) -> Result<R>
    where
        F: Fn(&str, &mut Vec<String>, f64) -> Result<R>,
    {
        let mut perplexity = Vec::<String>::new();
        self.retry_request_perplexity_search(url, payload, &mut perplexity, response_handler)
            .await
    }

    async fn apply_wait(&self) {
        if self.endpoint.wait != 0 {
            log::trace!("Waiting {}ms before next request", self.endpoint.wait);
            tokio::time::sleep(Duration::from_millis(self.endpoint.wait)).await;
        }
    }

    async fn apply_delay<E>(&self, delay: &mut Duration, max_delay: Duration, error: &E)
    where
        E: std::fmt::Display + 'static + std::fmt::Debug,
    {
        log::warn!(
            "Retrying endpoint {} in {:?} after error: {:?}{}",
            self.endpoint.name,
            delay,
            error,
            Self::error_chain(error)
        );
        tokio::time::sleep(*delay).await;
        *delay = std::cmp::min(*delay * 2, max_delay);
    }

    /// Format the full error chain, the Display of the outermost error
    /// is usually too generic (i.e. "error sending request for url (...)")
    /// and the interesting details (hyper, io, tls, dns) are only
    /// reachable through the sources.
    fn error_chain<E>(error: &E) -> String
    where
        E: std::fmt::Display + 'static,
    {
        let any = error as &dyn std::any::Any;

        // anyhow::Error doesn't implement std::error::Error, it
        // exposes its own chain() instead
        if let Some(error) = any.downcast_ref::<anyhow::Error>() {
            return error
                .chain()
                .skip(1)
                .map(|cause| format!("\n  caused by: {}", cause))
                .collect();
        }

        let mut chain = String::new();
        let mut source = any
            .downcast_ref::<reqwest::Error>()
            .and_then(|e| std::error::Error::source(e));
        while let Some(cause) = source {
            chain.push_str(&format!("\n  caused by: {}", cause));
            source = cause.source();
        }
        chain
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
