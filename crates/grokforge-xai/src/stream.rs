//! The SSE response stream.
//!
//! Wraps `reqwest`'s byte stream with `eventsource-stream` (which reassembles frames split
//! across TCP reads) and maps each frame to a typed [`StreamEvent`]. Tool-call argument
//! deltas are accumulated so callers always receive whole tool calls, whether xAI sends them
//! in one frame (the norm) or in pieces (the hedge).

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use eventsource_stream::{Event as EsEvent, Eventsource};
use futures::Stream;

use crate::error::XaiError;
use crate::event::{Frame, StreamEvent, parse_frame};

type EsInner = Pin<
    Box<
        dyn Stream<Item = Result<EsEvent, eventsource_stream::EventStreamError<reqwest::Error>>>
            + Send,
    >,
>;

/// A stream of [`StreamEvent`]s for one response. Also exposes the exact serialized request
/// size for context-ledger reconciliation.
pub struct ResponseStream {
    inner: EsInner,
    request_bytes: usize,
    pending: VecDeque<Result<StreamEvent, XaiError>>,
    /// item_id -> accumulated argument text (for the partial-JSON hedge).
    tool_args: HashMap<String, String>,
    finished: bool,
}

impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream")
            .field("request_bytes", &self.request_bytes)
            .field("pending", &self.pending.len())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ResponseStream {
    pub(crate) fn from_response(resp: reqwest::Response, request_bytes: usize) -> Self {
        let inner = Box::pin(resp.bytes_stream().eventsource());
        Self {
            inner,
            request_bytes,
            pending: VecDeque::new(),
            tool_args: HashMap::new(),
            finished: false,
        }
    }

    /// Exact number of bytes serialized for this request body — the ground truth the
    /// context ledger reconciles against.
    #[must_use]
    pub fn request_bytes(&self) -> usize {
        self.request_bytes
    }

    /// Translate a parsed frame into zero or more queued [`StreamEvent`]s.
    fn absorb(&mut self, frame: Frame) {
        match frame {
            Frame::Ignore => {}
            Frame::Done => self.finished = true,
            Frame::Created { response_id } => {
                self.pending
                    .push_back(Ok(StreamEvent::Created { response_id }));
            }
            Frame::TextDelta(t) => self.pending.push_back(Ok(StreamEvent::TextDelta(t))),
            Frame::ReasoningDelta(t) => {
                self.pending.push_back(Ok(StreamEvent::ReasoningDelta(t)));
            }
            Frame::ToolArgsDelta { item_id, delta } => {
                self.tool_args.entry(item_id).or_default().push_str(&delta);
            }
            Frame::ToolItemDone {
                item_id,
                call_id,
                name,
                arguments,
            } => {
                let accumulated = self.tool_args.remove(&item_id);
                let arguments = arguments
                    .filter(|s| !s.is_empty())
                    .or(accumulated)
                    .unwrap_or_default();
                self.pending
                    .push_back(Ok(StreamEvent::ToolCall(crate::event::ToolCall {
                        call_id,
                        name,
                        arguments,
                    })));
            }
            Frame::Completed { usage, stop } => {
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
        }
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
                Poll::Ready(Some(Ok(event))) => this.absorb(parse_frame(&event.data)),
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(XaiError::Stream(e.to_string()))));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
