//! Bridges structured question requests from the core to the interactive TUI.

use async_trait::async_trait;
use grokforge_core::Questioner;
use grokforge_protocol::{QuestionRequest, QuestionResponse};
use tokio::sync::{mpsc, oneshot};

/// A question batch the UI must resolve, with the channel used to return the answer.
#[derive(Debug)]
pub struct PendingQuestion {
    pub request: QuestionRequest,
    pub respond: oneshot::Sender<QuestionResponse>,
}

/// A [`Questioner`] that queues every request in the TUI event loop.
#[derive(Debug, Clone)]
pub struct ChannelQuestioner {
    tx: mpsc::UnboundedSender<PendingQuestion>,
}

impl ChannelQuestioner {
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PendingQuestion>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl Questioner for ChannelQuestioner {
    async fn ask(&self, request: QuestionRequest) -> QuestionResponse {
        let (respond, wait) = oneshot::channel();
        if self.tx.send(PendingQuestion { request, respond }).is_err() {
            return QuestionResponse::Unavailable {
                reason: "interactive frontend closed before the question could be shown"
                    .to_string(),
            };
        }
        wait.await.unwrap_or(QuestionResponse::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use grokforge_core::Questioner as _;
    use grokforge_protocol::{
        QuestionAnswer, QuestionId, QuestionOption, ToolCallId, UserQuestion,
    };

    use super::*;

    fn request() -> QuestionRequest {
        QuestionRequest {
            id: QuestionId::new(),
            call_id: ToolCallId::new(),
            questions: vec![UserQuestion {
                header: "Scope".into(),
                prompt: "Which scope?".into(),
                options: vec![
                    QuestionOption {
                        label: "Small".into(),
                        description: String::new(),
                    },
                    QuestionOption {
                        label: "Large".into(),
                        description: String::new(),
                    },
                ],
                allow_custom: false,
            }],
        }
    }

    #[tokio::test]
    async fn forwards_and_returns_the_exact_structured_response() {
        let (questioner, mut pending) = ChannelQuestioner::new();
        let task = tokio::spawn(async move { questioner.ask(request()).await });
        let pending = pending.recv().await.expect("pending question");
        let response = QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                question: 0,
                selected: Some(1),
                custom: None,
            }],
        };
        pending
            .respond
            .send(response.clone())
            .expect("question response accepted");
        assert_eq!(task.await.expect("question task"), response);
    }

    #[tokio::test]
    async fn dropped_ui_cancels_instead_of_deadlocking() {
        let (questioner, mut pending) = ChannelQuestioner::new();
        let task = tokio::spawn(async move { questioner.ask(request()).await });
        drop(pending.recv().await.expect("pending question"));
        assert_eq!(
            task.await.expect("question task"),
            QuestionResponse::Cancelled
        );
    }
}
