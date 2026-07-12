//! The SSE response stream.
//!
//! Wraps `reqwest`'s byte stream with `eventsource-stream` (which reassembles frames split
//! across TCP reads) and maps each frame to a typed [`StreamEvent`]. Tool-call argument
//! deltas are accumulated so callers always receive whole tool calls, whether xAI sends them
//! in one frame (the norm) or in pieces (the hedge).

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use eventsource_stream::{Event as EsEvent, Eventsource};
use futures::Stream;

use crate::error::XaiError;
use crate::event::{Frame, StreamEvent, parse_frame};

const MAX_SSE_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_SSE_TOTAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTIVE_TOOL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_STREAMS: usize = 256;
const MAX_TOOL_IDENTIFIER_BYTES: usize = 1024;
const MAX_TOOL_NAME_BYTES: usize = 256;
// Rollout JSONL rejects lines above 16 MiB. Measure the complete SSE event and leave generous
// room for the persisted ResponseItem envelope and JSON escaping.
const MAX_PROVIDER_OUTPUT_ITEM_BYTES: usize = 15 * 1024 * 1024;
const MAX_ENCRYPTED_REASONING_BYTES: usize = 14 * 1024 * 1024;
const MAX_PROVIDER_OUTPUT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_PROVIDER_OUTPUT_ITEMS: usize = 512;

type EsInner = Pin<
    Box<
        dyn Stream<Item = Result<EsEvent, eventsource_stream::EventStreamError<reqwest::Error>>>
            + Send,
    >,
>;

/// A byte-level guard in front of `eventsource-stream`.
///
/// The SSE decoder necessarily buffers until a line/event terminator. Counting only decoded
/// `data:` events is too late for an attacker that streams an unterminated comment, `id:`, or
/// other non-data field. This adapter stops forwarding raw chunks before the downstream decoder
/// can retain more than the configured line/event limit.
struct BoundedRawSse<S> {
    inner: Pin<Box<S>>,
    limit_hit: Arc<AtomicBool>,
    total_bytes: usize,
    event_bytes: usize,
    line_bytes: usize,
    skip_lf_after_cr: bool,
    max_event_bytes: usize,
    max_total_bytes: usize,
}

impl<S> BoundedRawSse<S> {
    fn new(inner: S, limit_hit: Arc<AtomicBool>) -> Self {
        Self::with_limits(inner, limit_hit, MAX_SSE_FRAME_BYTES, MAX_SSE_TOTAL_BYTES)
    }

    fn with_limits(
        inner: S,
        limit_hit: Arc<AtomicBool>,
        max_event_bytes: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            limit_hit,
            total_bytes: 0,
            event_bytes: 0,
            line_bytes: 0,
            skip_lf_after_cr: false,
            max_event_bytes,
            max_total_bytes,
        }
    }

    fn accept_chunk(&mut self, chunk: &[u8]) -> bool {
        self.total_bytes = match self.total_bytes.checked_add(chunk.len()) {
            Some(total) if total <= self.max_total_bytes => total,
            _ => return false,
        };
        for &byte in chunk {
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.event_bytes = match self.event_bytes.checked_add(1) {
                Some(total) if total <= self.max_event_bytes => total,
                _ => return false,
            };
            match byte {
                b'\r' | b'\n' => {
                    if self.line_bytes == 0 {
                        // A blank line terminates an SSE event. Reset before the downstream
                        // decoder receives the chunk; CRLF counts as one logical terminator.
                        self.event_bytes = 0;
                    }
                    self.line_bytes = 0;
                    self.skip_lf_after_cr = byte == b'\r';
                }
                _ => {
                    self.line_bytes = match self.line_bytes.checked_add(1) {
                        Some(total) if total <= self.max_event_bytes => total,
                        _ => return false,
                    };
                }
            }
        }
        true
    }

    fn mark_limit(&self) {
        self.limit_hit.store(true, Ordering::Release);
    }
}

impl<S, B, E> Stream for BoundedRawSse<S>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    type Item = Result<B, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if this.accept_chunk(chunk.as_ref()) {
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    this.mark_limit();
                    Poll::Ready(None)
                }
            }
            Poll::Ready(other) => Poll::Ready(other),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A stream of [`StreamEvent`]s for one response. Also exposes the exact serialized request
/// size for context-ledger reconciliation.
pub struct ResponseStream {
    inner: EsInner,
    request_bytes: usize,
    request_attempts: u32,
    pending: VecDeque<Result<StreamEvent, XaiError>>,
    /// item_id -> accumulated argument text (for the partial-JSON hedge).
    tool_args: HashMap<String, String>,
    active_tool_argument_bytes: usize,
    total_tool_argument_bytes: usize,
    total_sse_bytes: usize,
    provider_output_bytes: usize,
    provider_output_items: usize,
    raw_limit_hit: Arc<AtomicBool>,
    finished: bool,
}

impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream")
            .field("request_bytes", &self.request_bytes)
            .field("request_attempts", &self.request_attempts)
            .field("pending", &self.pending.len())
            .field("total_sse_bytes", &self.total_sse_bytes)
            .field("provider_output_bytes", &self.provider_output_bytes)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ResponseStream {
    pub(crate) fn from_response(
        resp: reqwest::Response,
        request_bytes: usize,
        request_attempts: u32,
    ) -> Self {
        let raw_limit_hit = Arc::new(AtomicBool::new(false));
        let bounded = BoundedRawSse::new(resp.bytes_stream(), Arc::clone(&raw_limit_hit));
        let inner = Box::pin(bounded.eventsource());
        Self {
            inner,
            request_bytes,
            request_attempts,
            pending: VecDeque::new(),
            tool_args: HashMap::new(),
            active_tool_argument_bytes: 0,
            total_tool_argument_bytes: 0,
            total_sse_bytes: 0,
            provider_output_bytes: 0,
            provider_output_items: 0,
            raw_limit_hit,
            finished: false,
        }
    }

    /// Exact number of bytes serialized for this request body — the ground truth the
    /// context ledger reconciles against.
    #[must_use]
    pub fn request_bytes(&self) -> usize {
        self.request_bytes
    }

    /// Number of POST attempts needed to establish this successful response stream.
    #[must_use]
    pub fn request_attempts(&self) -> u32 {
        self.request_attempts
    }

    /// Translate a parsed frame into zero or more queued [`StreamEvent`]s.
    #[allow(clippy::too_many_lines)] // Keeping the frame-to-event state machine in one match is auditable.
    fn absorb(&mut self, frame: Frame, frame_bytes: usize) {
        match frame {
            Frame::Ignore => {}
            Frame::Done => {
                self.pending.push_back(Err(XaiError::Stream(
                    "received [DONE] before response.completed".to_string(),
                )));
                self.finished = true;
            }
            Frame::Created { response_id } => {
                self.pending
                    .push_back(Ok(StreamEvent::Created { response_id }));
            }
            Frame::TextDelta(t) => self.pending.push_back(Ok(StreamEvent::TextDelta(t))),
            Frame::ReasoningDelta(t) => {
                self.pending.push_back(Ok(StreamEvent::ReasoningDelta(t)));
            }
            Frame::ToolArgsDelta { item_id, delta } => {
                if item_id.len() > MAX_TOOL_IDENTIFIER_BYTES {
                    self.fail_limit("tool item id is too large");
                    return;
                }
                if !self.tool_args.contains_key(&item_id)
                    && self.tool_args.len() >= MAX_TOOL_ARGUMENT_STREAMS
                {
                    self.fail_limit("too many simultaneous tool-argument streams");
                    return;
                }
                let current_len = self.tool_args.get(&item_id).map_or(0, String::len);
                if current_len
                    .checked_add(delta.len())
                    .is_none_or(|length| length > MAX_TOOL_ARGUMENT_BYTES)
                {
                    self.fail_limit("one tool argument exceeds its byte limit");
                    return;
                }
                if self
                    .active_tool_argument_bytes
                    .checked_add(delta.len())
                    .is_none_or(|length| length > MAX_ACTIVE_TOOL_ARGUMENT_BYTES)
                {
                    self.fail_limit("active tool arguments exceed their aggregate byte limit");
                    return;
                }
                self.active_tool_argument_bytes += delta.len();
                self.tool_args.entry(item_id).or_default().push_str(&delta);
            }
            Frame::ToolItemDone {
                output_index,
                mut item,
                item_id,
                call_id,
                name,
                arguments,
            } => {
                if !self.account_provider_output(frame_bytes) {
                    return;
                }
                if output_index >= MAX_PROVIDER_OUTPUT_ITEMS {
                    self.fail_limit("provider output index exceeds the item limit");
                    return;
                }
                if item_id.len() > MAX_TOOL_IDENTIFIER_BYTES
                    || call_id.len() > MAX_TOOL_IDENTIFIER_BYTES
                    || name.len() > MAX_TOOL_NAME_BYTES
                {
                    self.fail_limit("tool call identifier or name is too large");
                    return;
                }
                let accumulated = self.tool_args.remove(&item_id);
                if let Some(accumulated) = &accumulated {
                    self.active_tool_argument_bytes = self
                        .active_tool_argument_bytes
                        .saturating_sub(accumulated.len());
                }
                let arguments = arguments
                    .filter(|s| !s.is_empty())
                    .or(accumulated)
                    .unwrap_or_default();
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    self.fail_limit("one completed tool argument exceeds its byte limit");
                    return;
                }
                let Some(total) = self.total_tool_argument_bytes.checked_add(arguments.len())
                else {
                    self.fail_limit("completed tool-argument byte count overflowed");
                    return;
                };
                if total > MAX_TOOL_ARGUMENT_TOTAL_BYTES {
                    self.fail_limit("completed tool arguments exceed their aggregate byte limit");
                    return;
                }
                self.total_tool_argument_bytes = total;
                if let Some(object) = item.as_object_mut() {
                    object.insert(
                        "arguments".to_string(),
                        serde_json::Value::String(arguments.clone()),
                    );
                }
                self.pending
                    .push_back(Ok(StreamEvent::ProviderOutput { output_index, item }));
                self.pending
                    .push_back(Ok(StreamEvent::ToolCall(crate::event::ToolCall {
                        output_index,
                        call_id,
                        name,
                        arguments,
                    })));
            }
            Frame::EncryptedReasoningDone {
                output_index,
                item,
                reasoning,
            } => {
                if !self.account_provider_output(frame_bytes) {
                    return;
                }
                if output_index >= MAX_PROVIDER_OUTPUT_ITEMS {
                    self.fail_limit("provider output index exceeds the item limit");
                    return;
                }
                if !self.validate_encrypted_reasoning_size(reasoning.encrypted_content.len()) {
                    return;
                }
                self.pending
                    .push_back(Ok(StreamEvent::ProviderOutput { output_index, item }));
                self.pending
                    .push_back(Ok(StreamEvent::EncryptedReasoning(reasoning)));
            }
            Frame::ProviderOutputDone { output_index, item } => {
                if !self.account_provider_output(frame_bytes) {
                    return;
                }
                if output_index >= MAX_PROVIDER_OUTPUT_ITEMS {
                    self.fail_limit("provider output index exceeds the item limit");
                    return;
                }
                self.pending
                    .push_back(Ok(StreamEvent::ProviderOutput { output_index, item }));
            }
            Frame::Completed { usage, stop } => {
                if !self.tool_args.is_empty() {
                    self.fail_limit("response completed with unfinished tool arguments");
                    return;
                }
                if let Some(u) = usage {
                    self.pending.push_back(Ok(StreamEvent::Usage(u)));
                }
                self.pending.push_back(Ok(StreamEvent::Completed { stop }));
                self.finished = true;
            }
            Frame::Error(msg) => {
                self.pending.push_back(Err(XaiError::ApiStreamError(msg)));
                self.finished = true;
            }
            Frame::Malformed(message) => {
                self.pending.push_back(Err(XaiError::Stream(format!(
                    "malformed SSE frame: {message}"
                ))));
                self.finished = true;
            }
        }
    }

    fn account_provider_output(&mut self, frame_bytes: usize) -> bool {
        if frame_bytes > MAX_PROVIDER_OUTPUT_ITEM_BYTES {
            self.fail_limit("one provider output item exceeds the resumable line-size limit");
            return false;
        }
        if self.provider_output_items >= MAX_PROVIDER_OUTPUT_ITEMS {
            self.fail_limit("too many provider output items");
            return false;
        }
        let Some(total) = self.provider_output_bytes.checked_add(frame_bytes) else {
            self.fail_limit("provider output byte count overflowed");
            return false;
        };
        if total > MAX_PROVIDER_OUTPUT_TOTAL_BYTES {
            self.fail_limit("provider outputs exceed their aggregate byte limit");
            return false;
        }
        self.provider_output_items += 1;
        self.provider_output_bytes = total;
        true
    }

    fn account_sse_frame(&mut self, frame_bytes: usize) -> bool {
        if frame_bytes > MAX_SSE_FRAME_BYTES {
            self.fail_limit("one SSE frame exceeds its byte limit");
            return false;
        }
        let Some(total) = self.total_sse_bytes.checked_add(frame_bytes) else {
            self.fail_limit("SSE byte count overflowed");
            return false;
        };
        if total > MAX_SSE_TOTAL_BYTES {
            self.fail_limit("SSE stream exceeds its aggregate byte limit");
            return false;
        }
        self.total_sse_bytes = total;
        true
    }

    fn validate_encrypted_reasoning_size(&mut self, bytes: usize) -> bool {
        if bytes > MAX_ENCRYPTED_REASONING_BYTES {
            self.fail_limit("encrypted reasoning item exceeds its byte limit");
            false
        } else {
            true
        }
    }

    fn fail_limit(&mut self, message: &str) {
        self.pending.push_back(Err(XaiError::Stream(format!(
            "response size limit exceeded: {message}"
        ))));
        self.finished = true;
    }
}

impl Stream for ResponseStream {
    type Item = Result<StreamEvent, XaiError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(ev));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(event))) => {
                    let frame_bytes = event.data.len();
                    if !this.account_sse_frame(frame_bytes) {
                        continue;
                    }
                    this.absorb(parse_frame(&event.data), frame_bytes);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    if this.raw_limit_hit.load(Ordering::Acquire) {
                        return Poll::Ready(Some(Err(XaiError::Stream(
                            "response size limit exceeded before SSE event decoding".to_string(),
                        ))));
                    }
                    return Poll::Ready(Some(Err(XaiError::Stream(e.to_string()))));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    if this.raw_limit_hit.load(Ordering::Acquire) {
                        return Poll::Ready(Some(Err(XaiError::Stream(
                            "response size limit exceeded before SSE event decoding".to_string(),
                        ))));
                    }
                    return Poll::Ready(Some(Err(XaiError::Stream(
                        "response stream ended before response.completed".to_string(),
                    ))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_stream() -> ResponseStream {
        let inner: EsInner = Box::pin(futures::stream::empty());
        ResponseStream {
            inner,
            request_bytes: 0,
            request_attempts: 1,
            pending: VecDeque::new(),
            tool_args: HashMap::new(),
            active_tool_argument_bytes: 0,
            total_tool_argument_bytes: 0,
            total_sse_bytes: 0,
            provider_output_bytes: 0,
            provider_output_items: 0,
            raw_limit_hit: Arc::new(AtomicBool::new(false)),
            finished: false,
        }
    }

    fn has_limit_error(stream: &ResponseStream) -> bool {
        matches!(
            stream.pending.front(),
            Some(Err(XaiError::Stream(message))) if message.contains("limit exceeded")
        )
    }

    #[tokio::test]
    async fn raw_guard_bounds_unterminated_and_non_data_sse_fields() {
        use futures::StreamExt as _;

        for chunks in [
            vec![b":aaaa".as_slice(), b"aaaa".as_slice()],
            vec![b"e:a\n".as_slice(), b"id:bbbb\n".as_slice()],
        ] {
            let hit = Arc::new(AtomicBool::new(false));
            let source = futures::stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok::<Vec<u8>, std::io::Error>(chunk.to_vec())),
            );
            let mut guarded = BoundedRawSse::with_limits(source, Arc::clone(&hit), 7, 64);
            assert!(guarded.next().await.is_some());
            assert!(guarded.next().await.is_none());
            assert!(hit.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn raw_guard_resets_only_at_blank_event_terminators() {
        use futures::StreamExt as _;

        let hit = Arc::new(AtomicBool::new(false));
        let source = futures::stream::iter([
            Ok::<_, std::io::Error>(b"data:a\n\n".to_vec()),
            Ok(b"data:b\r\n\r\n".to_vec()),
        ]);
        let mut guarded = BoundedRawSse::with_limits(source, Arc::clone(&hit), 10, 32);
        assert!(guarded.next().await.is_some());
        assert!(guarded.next().await.is_some());
        assert!(guarded.next().await.is_none());
        assert!(!hit.load(Ordering::Acquire));
    }

    #[test]
    fn sse_per_frame_and_aggregate_caps_fail_closed() {
        let mut stream = empty_stream();
        assert!(!stream.account_sse_frame(MAX_SSE_FRAME_BYTES + 1));
        assert!(stream.finished);
        assert!(has_limit_error(&stream));

        let mut stream = empty_stream();
        stream.total_sse_bytes = MAX_SSE_TOTAL_BYTES;
        assert!(!stream.account_sse_frame(1));
        assert!(has_limit_error(&stream));
    }

    #[test]
    fn provider_output_aggregate_cap_fails_without_allocating_payload() {
        let mut stream = empty_stream();
        assert!(!stream.account_provider_output(MAX_PROVIDER_OUTPUT_TOTAL_BYTES.saturating_add(1)));
        assert!(has_limit_error(&stream));
    }

    #[test]
    fn single_provider_output_stays_below_rollout_line_limit() {
        let mut stream = empty_stream();
        assert!(!stream.account_provider_output(MAX_PROVIDER_OUTPUT_ITEM_BYTES.saturating_add(1)));
        assert!(has_limit_error(&stream));
    }

    #[test]
    fn encrypted_reasoning_field_cap_fails_without_allocating_payload() {
        let mut stream = empty_stream();
        assert!(
            !stream
                .validate_encrypted_reasoning_size(MAX_ENCRYPTED_REASONING_BYTES.saturating_add(1))
        );
        assert!(has_limit_error(&stream));
    }

    #[test]
    fn tool_argument_field_cap_rejects_oversized_delta() {
        let mut stream = empty_stream();
        stream.absorb(
            Frame::ToolArgsDelta {
                item_id: "fc_1".to_string(),
                delta: "x".repeat(MAX_TOOL_ARGUMENT_BYTES + 1),
            },
            0,
        );
        assert!(has_limit_error(&stream));
        assert!(stream.tool_args.is_empty());
    }
}
