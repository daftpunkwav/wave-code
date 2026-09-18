/*!
 * @file BtwJob
 * @description Side-question (`/btw`) session pump.
 *
 * Responsibilities:
 * - Own the private side-session link assembled by the harness factory
 *   (read-only mode, seeded with the main dialogue).
 * - Pump session events into a bounded channel the UI drains
 *   non-blockingly, like [`crate::controllers::shell::ShellJob`].
 * - Forward follow-up questions and cancellation through a control
 *   channel so the UI never awaits the side session directly.
 *
 * This module must not depend on: runtime, capability, or bootstrap
 * crates; the link arrives already assembled.
 */

//! `/btw` side-session pump: answers stream into the panel without
//! touching the main conversation.

use tokio::sync::mpsc;
use uuid::Uuid;
use wavecode_wire::{EventMsg, Op, Submission};

use crate::ui::SessionLink;

/// One event from the side session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtwEvent {
    /// Incremental answer text.
    Delta(String),
    /// One answer settled: full text, `true` when cancelled or failed.
    Done {
        /// The settled answer text (possibly empty on cancel).
        text: String,
        /// True when the turn was interrupted or errored.
        interrupted: bool,
    },
    /// The pump task exited (link shut down or channel closed).
    Ended,
}

/// UI-to-pump control messages.
enum BtwControl {
    /// Submit a follow-up question to the side session.
    Ask(String),
    /// Stop the current answer (if any) and shut the side session down.
    Cancel,
}

/// Channel capacity; deltas apply backpressure to the pump instead of
/// buffering an unbounded transcript in memory.
const CHANNEL_CAP: usize = 256;

/// A running side-question session.
pub struct BtwJob {
    rx: mpsc::Receiver<BtwEvent>,
    control: mpsc::UnboundedSender<BtwControl>,
    /// Set once the pump reported `Ended` so `try_recv` stops
    /// reporting (mirrors the shell job's drain-once contract).
    ended: bool,
}

impl BtwJob {
    /// Spawn the pump: submits `question` to the side session and
    /// streams its answer. The link is owned exclusively from here on.
    pub fn spawn(mut link: Box<dyn SessionLink>, question: &str) -> Self {
        let question = question.to_string();
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        let (control, mut ctl_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _ = link
                .submit(Submission {
                    id: Uuid::new_v4().to_string(),
                    op: Op::UserInput { text: question },
                })
                .await;
            let mut running = true;
            let mut closing = false;
            let mut buffer = String::new();
            loop {
                tokio::select! {
                    biased;
                    control = ctl_rx.recv() => match control {
                        Some(BtwControl::Ask(text)) => {
                            // Busy side sessions queue the input; idle
                            // ones start a fresh turn immediately.
                            let _ = link
                                .submit(Submission {
                                    id: Uuid::new_v4().to_string(),
                                    op: Op::UserInput { text },
                                })
                                .await;
                        }
                        Some(BtwControl::Cancel) => {
                            if running {
                                closing = true;
                                let _ = link.submit(Submission {
                                    id: Uuid::new_v4().to_string(),
                                    op: Op::Interrupt,
                                }).await;
                            } else {
                                break;
                            }
                        }
                        // The UI dropped the job: shut the session down.
                        None => break,
                    },
                    event = link.next_event() => {
                        let Some(event) = event else { break };
                        match event.msg {
                            EventMsg::AgentMessageDelta { text } => {
                                buffer.push_str(&text);
                                if tx.send(BtwEvent::Delta(text)).await.is_err() {
                                    break;
                                }
                            }
                            EventMsg::AgentMessageComplete { text }
                                if buffer.is_empty() && !text.is_empty() =>
                            {
                                // Senders that skip deltas still answer.
                                buffer.push_str(&text);
                            }
                            EventMsg::Error { message, .. } => {
                                running = false;
                                buffer.clear();
                                if tx.send(BtwEvent::Done {
                                    text: message,
                                    interrupted: true,
                                })
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            EventMsg::TurnCompleted { interrupted } => {
                                running = false;
                                let text = std::mem::take(&mut buffer);
                                if tx.send(BtwEvent::Done { text, interrupted }).await.is_err() {
                                    break;
                                }
                                if closing {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            let _ = link
                .submit(Submission {
                    id: Uuid::new_v4().to_string(),
                    op: Op::Shutdown,
                })
                .await;
            let _ = tx.send(BtwEvent::Ended).await;
        });
        Self {
            rx,
            control,
            ended: false,
        }
    }

    /// Poll one pending event; `None` when nothing is ready or the job
    /// already ended. A closed channel reports `Ended` exactly once.
    pub fn try_recv(&mut self) -> Option<BtwEvent> {
        if self.ended {
            return None;
        }
        match self.rx.try_recv() {
            Ok(event) => {
                if matches!(event, BtwEvent::Ended) {
                    self.ended = true;
                }
                Some(event)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                self.ended = true;
                Some(BtwEvent::Ended)
            }
            Err(mpsc::error::TryRecvError::Empty) => None,
        }
    }

    /// Submit a follow-up question (continues the same side session).
    pub fn ask(&self, question: &str) {
        let _ = self.control.send(BtwControl::Ask(question.to_string()));
    }

    /// Stop the current answer and shut the side session down; the
    /// remaining events still drain and the job ends with `Ended`.
    pub fn cancel(&self) {
        let _ = self.control.send(BtwControl::Cancel);
    }
}
