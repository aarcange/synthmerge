// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use anyhow::Result;
use serde_json::{Value, json};

/// Rebuilds a non-streaming response body from SSE events so that
/// the existing response handlers keep working unchanged.
#[derive(Default)]
pub struct StreamAccumulator {
    anthropic: bool,
    error: Option<Value>,
    raw_lines: Vec<String>,
    // openai
    content: String,
    text: Option<String>,
    finish_reason: Option<String>,
    logprobs: Vec<Value>,
    usage: Option<Value>,
    // anthropic
    blocks: Vec<Value>,
    stop_reason: Option<String>,
}

impl StreamAccumulator {
    pub fn new(anthropic: bool) -> Self {
        Self {
            anthropic,
            ..Default::default()
        }
    }

    fn append(v: &mut Value, key: &str, s: &str) {
        let Some(obj) = v.as_object_mut() else {
            log::warn!("Ignoring delta for non-object block");
            return;
        };
        match obj.get_mut(key) {
            Some(Value::String(cur)) => cur.push_str(s),
            _ => {
                obj.insert(key.to_string(), Value::String(s.to_string()));
            }
        }
    }

    /// Accumulate a non-JSON payload (received in an SSE `data:` field
    /// or as a bare text line) as a raw fallback, so that error
    /// messages (e.g. "Usage limit exceeded" or "out of memory")
    /// survive the streaming decode and are still classified by the
    /// response handler. The raw fallback is only used on truncated
    /// streams: the raw lines are surfaced by `finish()` (joined with
    /// '\n') only if the stream did not complete; otherwise they are
    /// discarded, so stray text can't poison the success path. Errors
    /// already recorded win over the raw fallback.
    pub fn push_raw(&mut self, data: &str) {
        self.raw_lines.push(data.to_string());
    }

    pub fn push(&mut self, data: &str) {
        if data == "[DONE]" {
            return;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("Failed to parse SSE event ({}): {}", e, data);
                self.push_raw(data);
                return;
            }
        };
        if v.get("error").is_some_and(|e| !e.is_null()) {
            // Keep the first error: later ones are usually derived
            // from the same failure.
            self.error.get_or_insert(v);
            return;
        }
        // Older vLLM and some proxies report errors as
        // {"object":"error","message":...} rather than {"error": ...}.
        if v.get("object").and_then(Value::as_str) == Some("error") {
            self.error.get_or_insert(v);
            return;
        }
        // Gemini reports errors as [{"error": {...}}] (a JSON array),
        // which has no top-level "error" key.
        if let Some(arr) = v.as_array() {
            let is_error = arr
                .first()
                .is_some_and(|e| e.get("error").is_some_and(|err| !err.is_null()));
            if is_error {
                self.error.get_or_insert(v);
                return;
            }
        }
        if self.anthropic {
            self.push_anthropic(v)
        } else {
            self.push_openai(v)
        }
    }

    fn push_openai(&mut self, v: Value) {
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(u.clone());
        }
        let Some(c) = v.get("choices").and_then(Value::as_array).and_then(|a| {
            a.iter()
                .find(|c| c.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
        }) else {
            return;
        };
        if let Some(t) = c.pointer("/delta/content").and_then(Value::as_str) {
            self.content.push_str(t);
        }
        if let Some(t) = c.get("text").and_then(Value::as_str) {
            self.text.get_or_insert_with(String::new).push_str(t);
        }
        if let Some(fr) = c.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        if let Some(lp) = c.pointer("/logprobs/content").and_then(Value::as_array) {
            self.logprobs.extend(lp.iter().cloned());
        }
    }

    fn push_anthropic(&mut self, v: Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.usage = v.pointer("/message/usage").cloned();
            }
            Some("content_block_start") => {
                if let Some(b) = v.get("content_block") {
                    let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if idx < self.blocks.len() {
                        self.blocks[idx] = b.clone();
                    } else if idx == self.blocks.len() {
                        self.blocks.push(b.clone());
                    } else {
                        // content_block_start events arrive in ascending
                        // index order in the Anthropic SSE protocol, so an
                        // index beyond the current length is malformed
                        // input; padding up to it would allocate
                        // unboundedly. Ignore it instead.
                        log::warn!("Ignoring out-of-order content_block_start index {}", idx);
                    }
                }
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let (Some(block), Some(delta)) = (self.blocks.get_mut(idx), v.get("delta")) else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            Self::append(block, "text", t);
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                            Self::append(block, "thinking", t);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(Value::Object(u)) = v.get("usage") {
                    let usage = self.usage.get_or_insert_with(|| json!({}));
                    if !usage.is_object() {
                        *usage = json!({});
                    }
                    let usage = usage.as_object_mut().unwrap();
                    for (k, val) in u {
                        usage.insert(k.clone(), val.clone());
                    }
                }
            }
            _ => {}
        }
    }

    pub fn finish(self) -> Result<String> {
        if let Some(e) = self.error {
            return Ok(e.to_string());
        }
        let complete = if self.anthropic {
            self.stop_reason.is_some()
        } else {
            self.finish_reason.is_some()
        };
        if !complete {
            if !self.raw_lines.is_empty() {
                return Ok(self.raw_lines.join("\n"));
            }
            anyhow::bail!("SSE stream ended before completion");
        }
        if self.anthropic {
            return Ok(json!({
                "type": "message",
                "role": "assistant",
                "content": self.blocks,
                "stop_reason": self.stop_reason,
                "usage": self.usage,
            })
            .to_string());
        }
        let mut choice = json!({ "index": 0, "finish_reason": self.finish_reason });
        match self.text {
            Some(text) => choice["text"] = json!(text),
            None => choice["message"] = json!({ "role": "assistant", "content": self.content }),
        }
        if !self.logprobs.is_empty() {
            choice["logprobs"] = json!({ "content": self.logprobs });
        }
        Ok(json!({ "choices": [choice], "usage": self.usage }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conflict_resolver::ConflictResolver;
    use crate::prob;

    fn acc_with(events: &[&str], anthropic: bool) -> StreamAccumulator {
        let mut acc = StreamAccumulator::new(anthropic);
        for e in events {
            acc.push(e);
        }
        acc
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn test_openai_chat_stream_with_usage() {
        let acc = acc_with(
            &[
                r#"{"id":"x","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"id":"x","choices":[{"index":0,"delta":{"content":"<|patched_code|>\n"},"finish_reason":null}]}"#,
                r#"{"id":"x","choices":[{"index":0,"delta":{"content":"hello\n"},"finish_reason":null}]}"#,
                r#"{"id":"x","choices":[{"index":0,"delta":{"content":"<|/patched_code|>"},"finish_reason":null}]}"#,
                r#"{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                r#"{"id":"x","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
                "[DONE]",
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        let choice = &value["choices"][0];
        assert_eq!(choice["finish_reason"], "stop");
        assert_eq!(
            choice["message"]["content"],
            format!(
                "{}\nhello\n{}",
                ConflictResolver::PATCHED_CODE_START,
                ConflictResolver::PATCHED_CODE_END
            )
        );
        assert_eq!(choice["message"]["role"], "assistant");
        assert_eq!(value["usage"]["prompt_tokens"], 10);
        assert_eq!(value["usage"]["completion_tokens"], 5);
        assert_eq!(value["usage"]["total_tokens"], 15);
    }

    #[test]
    fn test_openai_completions_stream() {
        let acc = acc_with(
            &[
                r#"{"choices":[{"index":0,"text":"foo","finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"text":"bar","finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"text":"","finish_reason":"length"}]}"#,
                "[DONE]",
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        let choice = &value["choices"][0];
        assert_eq!(choice["text"], "foobar");
        assert_eq!(choice["finish_reason"], "length");
        assert!(choice.get("message").is_none());
    }

    #[test]
    fn test_openai_finish_reason_with_last_content() {
        // vLLM and llama.cpp send finish_reason in the same chunk as
        // the last content delta.
        let acc = acc_with(
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"foo"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"bar"},"finish_reason":"stop"}]}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        let choice = &value["choices"][0];
        assert_eq!(choice["message"]["content"], "foobar");
        assert_eq!(choice["finish_reason"], "stop");
    }

    #[test]
    fn test_openai_choice_index_filtering() {
        // Multiple choices in the stream; only index 0 must be accumulated.
        let acc = acc_with(
            &[
                r#"{"choices":[{"index":1,"delta":{"content":"wrong"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"right"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "right");
    }

    #[test]
    fn test_openai_logprobs_accumulation() {
        let acc = acc_with(
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null,"logprobs":{"content":[{"token":"a","logprob":-0.1}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"b"},"finish_reason":null,"logprobs":{"content":[{"token":"b","logprob":-0.2}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        let logprobs = value["choices"][0]["logprobs"]["content"]
            .as_array()
            .unwrap();
        assert_eq!(logprobs.len(), 2);
        assert_eq!(logprobs[0]["token"], "a");
        assert_eq!(logprobs[1]["token"], "b");
    }

    #[test]
    fn test_openai_logprobs_usable_by_prob() {
        // The rebuilt JSON must still be understood by prob::logprob,
        // which relies on the patched code markers among the tokens.
        let acc = acc_with(
            &[
                &format!(
                    r#"{{"choices":[{{"index":0,"delta":{{"content":"{}\n"}},"finish_reason":null,"logprobs":{{"content":[{{"token":"{}\n","logprob":0.0}}]}}}}]}}"#,
                    ConflictResolver::PATCHED_CODE_START,
                    ConflictResolver::PATCHED_CODE_START
                ),
                r#"{"choices":[{"index":0,"delta":{"content":"hello\n"},"finish_reason":null,"logprobs":{"content":[{"token":"hello\n","logprob":-0.5}]}}]}"#,
                &format!(
                    r#"{{"choices":[{{"index":0,"delta":{{"content":"{}"}},"finish_reason":null,"logprobs":{{"content":[{{"token":"{}","logprob":-1.0}}]}}}}]}}"#,
                    ConflictResolver::PATCHED_CODE_END,
                    ConflictResolver::PATCHED_CODE_END
                ),
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        let mut perplexity = Vec::<String>::new();
        let logprob = prob::logprob(&value, &mut perplexity);
        assert_eq!(logprob, Some(-0.5));
    }

    #[test]
    fn test_openai_reasoning_content_does_not_leak() {
        // vLLM / llama.cpp / DeepSeek send reasoning_content deltas.
        // They must not end up in the content. The reasoning is also
        // dropped from the assembled body (it resets the idle timer
        // simply because bytes keep arriving), so the final
        // reconstructed response contains no trace of it.
        let acc = acc_with(
            &[
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"thinking"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "answer");
        assert!(
            !value.to_string().contains("thinking"),
            "reasoning content must not leak into the reconstructed body"
        );
    }

    #[test]
    fn test_anthropic_stream_with_thinking_and_text() {
        // The thinking block starts with the realistic empty
        // signature and ends with a signature_delta carrying a
        // signature: the delta must be ignored without disturbing the
        // thinking contents or the text block that follows.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc123"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                &format!(
                    r#"{{"type":"content_block_delta","index":1,"delta":{{"type":"text_delta","text":"{}\n"}}}}"#,
                    ConflictResolver::PATCHED_CODE_START
                ),
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello\n"}}"#,
                &format!(
                    r#"{{"type":"content_block_delta","index":1,"delta":{{"type":"text_delta","text":"{}"}}}}"#,
                    ConflictResolver::PATCHED_CODE_END
                ),
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["role"], "assistant");
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "let me think");
        assert_eq!(blocks[1]["type"], "text");
        let expected = format!(
            "{}\nhello\n{}",
            ConflictResolver::PATCHED_CODE_START,
            ConflictResolver::PATCHED_CODE_END
        );
        assert_eq!(blocks[1]["text"], expected);
        // The handler extracts the text from the first block with
        // type == "text"; the thinking block at index 0 must not
        // disturb it.
        let text = blocks.iter().find(|b| b["type"] == "text").unwrap();
        assert_eq!(text["text"], expected);
        assert_eq!(value["usage"]["input_tokens"], 7);
        assert_eq!(value["usage"]["output_tokens"], 9);
    }

    #[test]
    fn test_anthropic_redacted_thinking_passthrough() {
        // A redacted_thinking block arrives complete in
        // content_block_start, carries a data field and has no
        // deltas. It must pass through untouched and must not break
        // the text extraction of the following block.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"id":"msg_2","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"EmwKAhgBEgy3bW9kZWxzL2NsYXVkZQ"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                r#"{"type":"message_stop"}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["stop_reason"], "end_turn");
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        // Full equality: the block must be cloned verbatim, without
        // injected keys (e.g. a stray "text" from a future fallback).
        assert_eq!(
            blocks[0],
            json!({"type":"redacted_thinking","data":"EmwKAhgBEgy3bW9kZWxzL2NsYXVkZQ"})
        );
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "answer");
        let text = blocks.iter().find(|b| b["type"] == "text").unwrap();
        assert_eq!(text["text"], "answer");
    }

    #[test]
    fn test_anthropic_max_tokens_is_complete() {
        // A non-end_turn stop_reason must still be a successful
        // reconstruction: the handler raises IncompleteGeneration.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["stop_reason"], "max_tokens");
        assert_eq!(value["content"][0]["text"], "partial");
    }

    #[test]
    fn test_anthropic_delta_without_block_start() {
        // A delta with no matching content_block_start must be ignored
        // without panicking.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_delta","index":5,"delta":{"type":"text_delta","text":"ignored"}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"kept"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "kept");
    }

    #[test]
    fn test_anthropic_content_block_gap() {
        // content_block_start at index 1 with no start at index 0 must
        // not panic; the out-of-order block is ignored and the gap is
        // not padded.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 0);
    }

    #[test]
    fn test_anthropic_huge_block_index_is_ignored() {
        // A bogus index must not cause a huge allocation.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":4000000000,"content_block":{"type":"text","text":"bogus"}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        let blocks = value["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "ok");
    }

    #[test]
    fn test_anthropic_error_after_partial_content() {
        // An error event arriving after content must win over the
        // partially accumulated response.
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["error"]["type"], "overloaded_error");
        assert_eq!(value["error"]["message"], "Overloaded");
    }

    #[test]
    fn test_truncated_stream_is_error() {
        let acc = acc_with(
            &[r#"{"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}"#],
            false,
        );
        let err = acc.finish().unwrap_err();
        assert!(
            err.to_string().contains("ended before completion"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_truncated_anthropic_stream_is_error() {
        let acc = acc_with(
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
            ],
            true,
        );
        let err = acc.finish().unwrap_err();
        assert!(
            err.to_string().contains("ended before completion"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_error_event_preserves_top_level_error() {
        // vLLM / OpenAI-compatible style error event
        let acc = acc_with(
            &[r#"{"error":{"type":"exceed_context_size_error","message":"too long"}}"#],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(
            value["error"]["type"], "exceed_context_size_error",
            "the top-level error object must be preserved"
        );

        // Anthropic style error event
        let acc = acc_with(
            &[
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
            ],
            true,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(value["error"]["message"], "prompt is too long");
    }

    #[test]
    fn test_object_error_event_is_preserved() {
        // Older vLLM reports errors as {"object":"error",...}.
        let acc = acc_with(
            &[
                r#"{"object":"error","message":"This model's maximum context length is 8192 tokens","code":400}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["object"], "error");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("maximum context length")
        );
    }

    #[test]
    fn test_first_error_wins() {
        // A later error event must not overwrite the first one.
        let acc = acc_with(
            &[
                r#"{"error":{"type":"exceed_context_size_error","message":"too long"}}"#,
                r#"{"error":{"type":"server_error","message":"internal"}}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["error"]["type"], "exceed_context_size_error");
    }

    #[test]
    fn test_text_payload_is_preserved() {
        // Plain-text payloads (e.g. proxies emitting the error as text)
        // must be preserved as the body so the handler can classify them.
        let mut acc = StreamAccumulator::new(false);
        acc.push("Usage limit exceeded");
        let value = acc.finish().unwrap();
        assert_eq!(value, "Usage limit exceeded");
    }

    #[test]
    fn test_null_error_is_ignored() {
        // Some OpenAI-compatible servers emit "error": null on every chunk.
        let acc = acc_with(
            &[
                r#"{"error":null,"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
                r#"{"error":null,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert!(value.get("error").is_none());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
    }

    #[test]
    fn test_done_marker_ignored() {
        let mut acc = StreamAccumulator::new(false);
        acc.push("[DONE]");
        // No finish_reason was seen, so this must still be an error.
        assert!(acc.finish().is_err());
    }

    #[test]
    fn test_malformed_event_ignored() {
        // A stray non-JSON event must not replace the reconstructed
        // body of a complete stream: the response is still a success.
        let acc = acc_with(
            &[
                "not json",
                r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_gemini_array_error_is_preserved() {
        let acc = acc_with(
            &[
                r#"[{"error":{"code":400,"message":"The input token count exceeds the maximum number of tokens allowed","status":"INVALID_ARGUMENT"}}]"#,
            ],
            false,
        );
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value[0]["error"]["code"], 400);
        assert!(
            value[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("exceeds the maximum number of tokens allowed")
        );

        // An array whose first element has a null error is not an error.
        let acc = acc_with(&[r#"[{"error":null}]"#], false);
        assert!(acc.finish().is_err()); // truncated, not classified as error
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
