// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use crate::stream_accumulator::StreamAccumulator;

#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: String,
    /// Set when at least one `data` field was received for the
    /// current event, even if its value was empty: per the SSE spec
    /// a bare `data` line (no colon) is an empty data field and still
    /// contributes a '\n' separator when joined with other data lines.
    has_data: bool,
    scanned: usize,
    /// Set once the leading BOM (if any) has been handled.
    bom_checked: bool,
}

impl SseParser {
    const BOM: &'static [u8] = b"\xEF\xBB\xBF";

    pub fn feed(&mut self, acc: &mut StreamAccumulator, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if !self.bom_checked {
            // Per the SSE spec a single leading UTF-8 BOM is stripped
            // from the stream. Wait for more bytes if the buffer is
            // still a strict prefix of the BOM.
            if self.buf.len() < Self::BOM.len() && Self::BOM.starts_with(&self.buf) {
                return;
            }
            if self.buf.starts_with(Self::BOM) {
                self.buf.drain(..Self::BOM.len());
            }
            self.bom_checked = true;
        }
        let mut start = 0;
        let mut from = self.scanned;
        // Per the SSE spec lines are terminated by "\r\n", "\n" or a
        // lone "\r".
        while let Some(off) = memchr::memchr2(b'\r', b'\n', &self.buf[from..]) {
            let end = from + off;
            let next = if self.buf[end] == b'\r' {
                match self.buf.get(end + 1) {
                    Some(b'\n') => end + 2,
                    Some(_) => end + 1,
                    // A trailing '\r' may be followed by '\n' in the
                    // next chunk: wait for more data before deciding.
                    None => break,
                }
            } else {
                end + 1
            };
            let line = String::from_utf8_lossy(&self.buf[start..end]);
            Self::process_line(&mut self.data, &mut self.has_data, acc, &line);
            start = next;
            from = start;
        }
        self.buf.drain(..start);
        // A trailing '\r' left in the buffer must be scanned again on
        // the next call to find out if it is part of a "\r\n".
        self.scanned = if self.buf.last() == Some(&b'\r') {
            self.buf.len() - 1
        } else {
            self.buf.len()
        };
    }

    fn process_line(
        data: &mut String,
        has_data: &mut bool,
        acc: &mut StreamAccumulator,
        line: &str,
    ) {
        if line.is_empty() {
            Self::dispatch(data, has_data, acc);
            return;
        }
        // Per the SSE spec the field name is everything before
        // the first colon (or the whole line if there is no
        // colon), and a single leading space is stripped from
        // the value.
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            // ":" keepalive comments are ignored
            "" => {}
            "data" => {
                if *has_data {
                    data.push('\n');
                }
                data.push_str(value);
                *has_data = true;
            }
            // "event", "id" and "retry" fields are ignored
            "event" | "id" | "retry" => {}
            _ => {
                // Unknown field or bare text line: a proxy may
                // send a plain text body (e.g. "Usage limit
                // exceeded") as a text/event-stream payload,
                // without the `data:` prefix. The SSE spec would
                // ignore such a line; preserve it as a raw
                // fallback instead so that the response handler
                // can still classify it. The raw fallback is only
                // used if the stream does not complete.
                acc.push_raw(line);
            }
        }
    }

    fn dispatch(data: &mut String, has_data: &mut bool, acc: &mut StreamAccumulator) {
        if *has_data {
            // An event whose data is the empty string carries no
            // payload for the accumulator: skip it.
            if !data.is_empty() {
                acc.push(data);
            }
            data.clear();
            *has_data = false;
        }
    }

    /// Lenient end-of-stream flush: an unterminated trailing line in
    /// `buf` is processed as if it were newline terminated. A server
    /// may close the connection right after the last event without
    /// the final newline, and dropping that line would turn a complete
    /// stream into a spurious truncation error and a retry. Truncation
    /// is still detected by `StreamAccumulator::finish()` via the
    /// missing finish_reason/stop_reason. Any event data accumulated so
    /// far is then dispatched even without the terminating blank line.
    /// A buffered trailing lone '\r' is also completed by the injected
    /// '\n', so it becomes a "\r\n" terminator rather than a line of its own.
    pub fn finish(mut self, acc: &mut StreamAccumulator) {
        if !self.buf.is_empty() {
            log::debug!(
                "SSE stream ended with an unterminated line ({} bytes), processing it",
                self.buf.len()
            );
            self.feed(acc, b"\n");
        }
        Self::dispatch(&mut self.data, &mut self.has_data, acc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Feed `chunks` to a fresh parser, one `feed` call per chunk,
    /// and return the body rebuilt by the accumulator.
    fn run_chunks(chunks: &[&[u8]]) -> String {
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        for c in chunks {
            p.feed(&mut acc, c);
        }
        p.finish(&mut acc);
        acc.finish().unwrap()
    }

    #[test]
    fn test_sse_framing_arbitrary_chunking() {
        // Covers CRLF, `data:` without space, keepalive comments,
        // event:/id: lines, multi-byte UTF-8 and chunk boundaries that
        // split a line. Feeding the body byte by byte and in arbitrary
        // chunk sizes must produce identical results.
        let body = format!(
            ": keepalive\r\n\
             event: message\r\n\
             id: 1\r\n\
             retry: 5000\r\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"h\u{e9}llo \u{1f980}\"}},\"finish_reason\":null}}]}}\r\n\r\n\
             data:{{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        );
        let run = |chunk: usize| {
            let mut acc = StreamAccumulator::new(false);
            let mut parser = SseParser::default();
            if chunk == 0 {
                parser.feed(&mut acc, body.as_bytes());
            } else {
                for c in body.as_bytes().chunks(chunk) {
                    parser.feed(&mut acc, c);
                }
            }
            assert!(parser.buf.is_empty(), "trailing bytes left in buffer");
            parser.finish(&mut acc);
            acc.finish().unwrap()
        };
        let whole = run(0);
        let value = parse(&whole);
        assert_eq!(
            value["choices"][0]["message"]["content"],
            "h\u{e9}llo \u{1f980}"
        );
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        for n in 1..8 {
            assert_eq!(run(n), whole, "chunk size {n}");
        }
    }

    #[test]
    fn test_sse_leading_partial_line_is_not_pushed() {
        // A line split across two `feed` calls must not be pushed to
        // the accumulator as if it were a complete leading line. With a
        // single raw byte in the buffer, `from` must not skip it.
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        parser.feed(&mut acc, b"d");
        // The partial line stays buffered and nothing is dispatched yet;
        // `scanned` may advance since 'd' contains no '\n'.
        assert_eq!(parser.buf, b"d");
        assert_eq!(parser.scanned, 1);
        parser.feed(
            &mut acc,
            b"ata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
        );
        parser.feed(
            &mut acc,
            b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        parser.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
    }

    #[test]
    fn test_sse_final_event_without_blank_line() {
        // The last event is followed by a newline but no blank line.
        // Our end-of-stream flush dispatches it.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n";
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        parser.feed(&mut acc, body.as_bytes());
        assert!(parser.buf.is_empty(), "trailing bytes left in buffer");
        parser.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_sse_multiline_data_is_joined() {
        // A single JSON payload split across two `data:` lines must be
        // rejoined into one event.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":\n\
                    data: {\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        parser.feed(&mut acc, body.as_bytes());
        parser.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
    }

    #[test]
    fn test_sse_multiline_data_is_joined_across_chunks() {
        // Same as above, but the body is delivered one byte at a time so
        // that events and multiline `data:` fields are split across
        // calls. This only works if the partially accumulated event is
        // preserved across `feed` calls like a streaming caller does.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":\n\
                    data: {\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        for c in body.as_bytes().chunks(1) {
            parser.feed(&mut acc, c);
        }
        parser.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
    }

    #[test]
    fn test_sse_unterminated_final_line_is_processed() {
        // A final complete event without '\n' (the server closed the
        // connection right after it) must be processed at
        // end-of-stream, so the stream is not reported as truncated.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        parser.feed(&mut acc, body.as_bytes());
        assert!(!parser.buf.is_empty(), "unterminated line must stay in buf");
        parser.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_sse_unterminated_final_line_without_stop_is_error() {
        // An unterminated final line that does not complete the stream
        // must still be reported as truncated by the accumulator.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}";
        let mut acc = StreamAccumulator::new(false);
        let mut parser = SseParser::default();
        parser.feed(&mut acc, body.as_bytes());
        assert!(!parser.buf.is_empty(), "unterminated line must stay in buf");
        parser.finish(&mut acc);
        assert!(acc.finish().is_err());
    }

    #[test]
    fn test_sse_bare_text_line_preserved() {
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"Usage limit exceeded\n\n");
        p.finish(&mut acc);
        assert_eq!(acc.finish().unwrap(), "Usage limit exceeded");
    }

    #[test]
    fn test_sse_bare_text_lines_are_joined_in_order() {
        // A bare-text keepalive followed by a real error message must
        // yield the combined raw fallback, joined with '\n' in the
        // order the lines arrived.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"ping\n\nUsage limit exceeded\n\n");
        p.finish(&mut acc);
        assert_eq!(acc.finish().unwrap(), "ping\nUsage limit exceeded");
    }

    #[test]
    fn test_sse_bare_text_line_without_newline_preserved() {
        // A plain-text error body that is not newline terminated (the
        // server closed the connection right after it) must be
        // preserved: the unterminated trailing line is processed at
        // end-of-stream, so the response handler can classify it.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"Usage limit exceeded");
        assert!(!p.buf.is_empty(), "unterminated line must stay in buf");
        p.finish(&mut acc);
        assert_eq!(acc.finish().unwrap(), "Usage limit exceeded");
    }

    #[test]
    fn test_sse_lone_cr_terminators() {
        // Lone '\r' is a valid line terminator per the SSE spec.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\r\r\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\r";
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, body.as_bytes());
        // The first '\r' of the trailing "\r\r" terminates the stop
        // line; the second one may be followed by '\n' in the next
        // chunk, so it stays pending as exactly one byte.
        assert_eq!(p.buf, b"\r", "trailing bytes left in buffer");
        p.finish(&mut acc);
        let whole = acc.finish().unwrap();
        let value = parse(&whole);
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");

        // A lone '\r' at a chunk boundary followed by a non-'\n' byte
        // is the trickiest path.
        for n in 1..8 {
            assert_eq!(
                run_chunks(&body.as_bytes().chunks(n).collect::<Vec<_>>()),
                whole,
                "chunk size {n}"
            );
        }
    }

    #[test]
    fn test_sse_cr_and_lf_in_separate_feeds() {
        // A '\r' at the end of one chunk followed by a '\n' at the
        // start of the next must be treated as a single "\r\n"
        // terminator, not as two terminators. One event is split
        // across two `data:` lines so that a spurious blank line
        // would dispatch half of the JSON and lose the content.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":\r\n\
                    data: {\"content\":\"ok\"},\"finish_reason\":null}]}\r\n\
                    \r\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\
                    \r\n";
        let run = |split_at: usize| {
            let bytes = body.as_bytes();
            run_chunks(&[&bytes[..split_at], &bytes[split_at..]])
        };
        let whole = run(body.len());
        let value = parse(&whole);
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        for (i, b) in body.as_bytes().iter().enumerate() {
            if *b == b'\r' {
                assert_eq!(run(i + 1), whole, "split after CR at {}", i);
            }
        }
    }

    #[test]
    fn test_sse_leading_bom_is_stripped() {
        let body = "\u{feff}data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let whole = run_chunks(&[body.as_bytes()]);
        let value = parse(&whole);
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");

        // BOM split across chunks.
        let bytes = body.as_bytes();
        assert_eq!(run_chunks(&[&bytes[..1], &bytes[1..2], &bytes[2..]]), whole);

        // A leading partial BOM followed by a non-BOM byte must
        // neither stall nor be stripped, and the stream must still be
        // parsed normally. U+FE0F is EF B8 8F (1-byte BOM prefix),
        // U+FEFE is EF BB BE (2-byte BOM prefix). The input is split
        // so that for some splits the parser has to wait for more
        // bytes before deciding. The prefix becomes part of the first
        // line, whose `data:` field name is therefore not recognized;
        // that event degrades to a bare-text fallback and is discarded
        // once the stream completes, so the content of the first event
        // is lost while the response is still a success.
        for c in ['\u{fe0f}', '\u{fefe}'] {
            let prefixed = format!("{c}{body}");
            let bytes = prefixed.as_bytes();
            for split in 1..=3 {
                let value = parse(&run_chunks(&[&bytes[..split], &bytes[split..]]));
                assert!(value.get("error").is_none());
                assert_eq!(value["choices"][0]["message"]["content"], "");
                assert_eq!(value["choices"][0]["finish_reason"], "stop");
            }
        }
    }

    #[test]
    fn test_sse_second_bom_is_not_stripped() {
        // Only a single leading BOM is stripped. A second BOM after the
        // first one is part of the stream contents; here it precedes a
        // `data:` field name, so the field is not recognized and the
        // line becomes a bare-text fallback. If the stream completes,
        // the fallback is discarded and the response is a success, but
        // the content of that event is lost.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(
            &mut acc,
            b"\xEF\xBB\xBF\xEF\xBB\xBFdata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        p.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert!(value.get("error").is_none());
        // The "ok" event became a raw fallback (discarded on completion).
        assert_eq!(value["choices"][0]["message"]["content"], "");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_sse_finish_with_pending_lone_cr() {
        // A trailing lone '\r' left pending in the buffer must be
        // processed at end-of-stream.
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\r\r\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r";
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, body.as_bytes());
        // The stop line is unterminated: its '\r' may be the start of
        // "\r\n", so the whole line plus the '\r' stays buffered and
        // the '\r' itself is rescanned.
        assert!(p.buf.starts_with(b"data: ") && p.buf.ends_with(b"}\r"));
        assert_eq!(p.scanned, p.buf.len() - 1);
        p.finish(&mut acc);
        let value = parse(&acc.finish().unwrap());
        assert_eq!(value["choices"][0]["message"]["content"], "ok");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn test_sse_bare_data_line_is_empty_field() {
        // Per the SSE spec a bare `data` line (no colon) is an empty
        // data field and still contributes a '\n' separator.
        let mut acc = StreamAccumulator::new(false);
        let (mut data, mut has_data) = (String::new(), false);
        for line in ["data: a", "data", "data: b"] {
            SseParser::process_line(&mut data, &mut has_data, &mut acc, line);
        }
        assert_eq!(data, "a\n\nb");

        // A `data:` line (colon with an empty value) goes through the
        // colon branch of the field split. A leading empty data field
        // still contributes a '\n' separator to the following field.
        let (mut data, mut has_data) = (String::new(), false);
        for line in ["data:", "data: x"] {
            SseParser::process_line(&mut data, &mut has_data, &mut acc, line);
        }
        assert_eq!(data, "\nx");

        // An event made only of empty data fields is "\n", not skipped.
        // The "\n" is not valid JSON, so `StreamAccumulator::push`
        // stores it through the `push_raw` fallback, and since the
        // stream never completes `finish()` returns the raw fallback.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"data\ndata\n\n");
        p.finish(&mut acc);
        assert_eq!(acc.finish().unwrap(), "\n");
    }

    #[test]
    fn test_sse_single_empty_data_event_is_not_pushed() {
        // An event whose data is exactly the empty string carries no
        // payload: `dispatch` skips it rather than pushing an empty
        // string to the accumulator.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"data\n\n");
        p.finish(&mut acc);
        // Nothing was pushed and no raw fallback was recorded, so the
        // stream is reported as truncated.
        let err = acc.finish().unwrap_err();
        assert!(err.to_string().contains("ended before completion"), "{err}");
    }

    #[test]
    fn test_sse_partial_bom_only_stream() {
        // A stream that ends inside a BOM prefix must not stall: the
        // pending bytes are flushed by `finish()` as a bare-text line.
        let mut acc = StreamAccumulator::new(false);
        let mut p = SseParser::default();
        p.feed(&mut acc, b"\xEF\xBB");
        assert_eq!(p.buf, b"\xEF\xBB");
        p.finish(&mut acc);
        assert_eq!(acc.finish().unwrap(), "\u{fffd}");
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
