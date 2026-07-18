//! Multi-agent free discussion (spec §42): several agents with distinct
//! perspectives take turns debating a topic over multiple rounds, then a
//! synthesizer produces the agreed conclusion. Unlike a review panel (which
//! examines a diff), a discussion is open-ended design/reasoning.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_model::{Message, ModelRef, ModelRequest, ModelRuntime, Role};

use crate::error::OrchestratorError;

/// One participant with a persona.
#[derive(Debug, Clone)]
pub struct Participant {
    pub name: String,
    pub persona: String,
}

/// The default panel: complementary, deliberately-different lenses.
pub fn default_participants() -> Vec<Participant> {
    vec![
        Participant {
            name: "Architect".into(),
            persona: "You are the Architect. Focus on structure, interfaces, and \
                      long-term maintainability. Propose a concrete design."
                .into(),
        },
        Participant {
            name: "Skeptic".into(),
            persona: "You are the Skeptic. Hunt for edge cases, failure modes, \
                      hidden assumptions, and risks in what others propose."
                .into(),
        },
        Participant {
            name: "Pragmatist".into(),
            persona: "You are the Pragmatist. Push for the simplest thing that \
                      ships, cut scope, and object to over-engineering."
                .into(),
        },
    ]
}

/// A single utterance in the discussion.
#[derive(Debug, Clone)]
pub struct Turn {
    pub speaker: String,
    pub content: String,
}

/// The result of a discussion.
#[derive(Debug, Clone)]
pub struct DiscussionOutcome {
    pub transcript: Vec<Turn>,
    pub synthesis: String,
}

/// An event emitted as the discussion proceeds.
#[derive(Debug, Clone)]
pub enum DiscussionEvent {
    Turn(Turn),
    Synthesis(String),
}

/// Runs a multi-agent discussion.
pub struct Discussion {
    runtime: Arc<dyn ModelRuntime>,
    model: ModelRef,
    participants: Vec<Participant>,
    rounds: u32,
}

impl Discussion {
    pub fn new(runtime: Arc<dyn ModelRuntime>, model: ModelRef) -> Self {
        Self {
            runtime,
            model,
            participants: default_participants(),
            rounds: 2,
        }
    }

    pub fn with_participants(mut self, participants: Vec<Participant>) -> Self {
        self.participants = participants;
        self
    }

    pub fn with_rounds(mut self, rounds: u32) -> Self {
        self.rounds = rounds.max(1);
        self
    }

    /// Run the discussion: each participant speaks each round (seeing the
    /// transcript so far), then a synthesizer concludes.
    pub async fn run(
        &self,
        topic: &str,
        observer: &mut dyn FnMut(DiscussionEvent),
        cancellation: &CancellationToken,
    ) -> Result<DiscussionOutcome, OrchestratorError> {
        let mut transcript: Vec<Turn> = Vec::new();

        for _round in 0..self.rounds {
            for participant in &self.participants {
                if cancellation.is_cancelled() {
                    return Err(OrchestratorError::Cancelled);
                }
                let user = format!(
                    "Topic: {topic}\n\nDiscussion so far:\n{}\n\nAs {}, add your \
                     perspective in 2-4 sentences. Engage with what others said \
                     (agree/disagree, with reasons). Do not repeat yourself.",
                    render_transcript(&transcript),
                    participant.name,
                );
                let content = self.say(&participant.persona, &user, cancellation).await?;
                let turn = Turn {
                    speaker: participant.name.clone(),
                    content,
                };
                observer(DiscussionEvent::Turn(turn.clone()));
                transcript.push(turn);
            }
        }

        let synthesis = self
            .say(
                "You are the Synthesizer. Read the whole discussion and produce a \
                 single concrete conclusion: the decision, the plan, and any \
                 unresolved risks. Be decisive.",
                &format!(
                    "Topic: {topic}\n\nDiscussion:\n{}\n\nSynthesize the conclusion.",
                    render_transcript(&transcript)
                ),
                cancellation,
            )
            .await?;
        observer(DiscussionEvent::Synthesis(synthesis.clone()));

        Ok(DiscussionOutcome {
            transcript,
            synthesis,
        })
    }

    async fn say(
        &self,
        system: &str,
        user: &str,
        cancellation: &CancellationToken,
    ) -> Result<String, OrchestratorError> {
        let mut request = ModelRequest::new(
            self.model.clone(),
            vec![
                Message::text(Role::System, system),
                Message::text(Role::User, user),
            ],
        );
        request.max_output_tokens = Some(512);
        request.temperature = Some(0.4);
        let response = self
            .runtime
            .generate(request, cancellation.child_token())
            .await?;
        Ok(response.message.text_content().trim().to_string())
    }
}

fn render_transcript(transcript: &[Turn]) -> String {
    if transcript.is_empty() {
        return "(nothing yet — you speak first)".to_string();
    }
    transcript
        .iter()
        .map(|t| format!("{}: {}", t.speaker, t.content))
        .collect::<Vec<_>>()
        .join("\n")
}
