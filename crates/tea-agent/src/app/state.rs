use crate::composer::Composer;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use tea_core::event::{
    AgentEventKind, AutomaticCompactionOutcome, CompactionOutcome, ProviderRequestSkipReason,
};
use tea_core::provider::ProviderRegistry;
use tea_core::state::{AgentMessage, ToolCallId};
use tea_core::{AgentEvent, ModelDescriptor, ThinkingLevel, Usage};

use super::commands;
use super::durable::DurableSessionSummary;
use super::host::{model_candidates, overlay_lines};

/// Typed transcript entry contract for presentation consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptEntry {
    Welcome {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
    },
    Tool(ToolProjection),
    Notice {
        text: String,
        severity: NoticeSeverity,
    },
    Error {
        text: String,
    },
}

/// Generic tool lifecycle projection retained independently from rendered text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProjection {
    pub call_id: ToolCallId,
    /// Core event sequence most recently associated with this lifecycle.
    pub sequence: Option<u64>,
    pub transcript_index: usize,
    pub tool_name: String,
    pub arguments: String,
    pub latest_progress: Option<String>,
    pub settled_result: Option<String>,
    pub state: ToolState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeSeverity {
    Info,
    Warning,
}

/// Temporary presentation surfaces. Core/session/provider state remains outside this enum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiSurface {
    #[default]
    None,
    Help,
    ModelPicker,
    CustomModel,
    SessionPicker,
    /// Full-transcript/detail inspection surface.
    ToolDetail,
}

/// Generic tool lifecycle state for compact rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolState {
    /// Tool has been admitted and started.
    Started,
    /// Tool has emitted an update.
    Progress,
    /// Tool completed successfully.
    Completed,
    /// Tool completed with an error.
    Failed,
}

/// Presentation-only status for the fixed status line.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub enum UiStatus {
    /// No operation currently owns the core agent.
    #[default]
    Idle,
    /// A model/tool or compaction operation is active.
    Active,
    /// A concise local notice is displayed.
    Notice(String),
    /// A local error is displayed with error styling.
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Picker {
    Model {
        filter: String,
        selected: usize,
    },
    CustomModel {
        provider: String,
        input: String,
    },
    Session {
        filter: String,
        selected: usize,
        entries: Vec<DurableSessionSummary>,
    },
}

/// Terminal-owned state: event-derived rows plus local input and overlay state.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    /// Raw, typed presentation entries. This is the only transcript collection
    /// owned by the host; rendered labels are a renderer concern.
    pub(super) transcript: Vec<TranscriptEntry>,
    /// Core event sequence aligned with `transcript` entries. Local entries use
    /// `None`; retaining it keeps event identity available without formatting it
    /// into user-visible text.
    pub(super) transcript_sequences: Vec<Option<u64>>,
    pub(super) composer: Composer,
    pub(super) status: UiStatus,
    pub(super) viewport_offset: usize,
    pub(super) follow_output: bool,
    pub(super) visible_transcript_lines: usize,
    pub(super) transcript_rows: usize,
    pub(super) selected_model: Option<ModelDescriptor>,
    /// Presentation projection of the selected model's durable compaction policy.
    pub(super) automatic_compaction_enabled: bool,
    /// Effective context capacity selected by the host. This may be an explicit local override;
    /// the registry remains the fallback for catalog-backed models.
    pub(super) selected_context_window: Option<NonZeroU64>,
    pub(super) picker: Option<Picker>,
    pub(super) streaming_line: Option<usize>,
    /// Active generic tool rows keyed by the core-owned call identity.
    pub(super) active_tool_lines: BTreeMap<ToolCallId, usize>,
    /// The most recent core-emitted context estimate. `None` means the core has not supplied
    /// capacity-policy evidence for this projection; it is never inferred from rendered text.
    pub(super) context_estimate: Option<ContextEstimate>,
    /// In-memory prompt history for the current terminal invocation.
    pub(super) history: Vec<String>,
    /// Current history cursor; `None` means the live composer draft.
    pub(super) history_index: Option<usize>,
    /// Draft saved when history navigation first leaves the live composer.
    pub(super) history_draft: Option<String>,
    pub(super) surface: UiSurface,
    /// Payload for data-bearing temporary surfaces such as help.
    pub(super) surface_lines: Vec<String>,
    /// First unwrapped payload line shown by the temporary surface viewer.
    /// It is separate from transcript scrolling so return-to-live preserves
    /// the transcript's own viewport and follow state.
    pub(super) surface_offset: usize,
    pub(super) slash_completion: Option<SlashCompletion>,
    /// Field-wise provider accounting observed directly from the lossless event stream.
    pub(super) reported_usage: Usage,
    /// Reasoning effort used by future prompts.
    pub(super) thinking_level: ThinkingLevel,
}

/// State for the literal-prefix slash completion menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SlashCompletion {
    pub(super) prefix: String,
    pub(super) selected: usize,
    pub(super) matches: Vec<String>,
}

/// Context-policy information carried by the core event stream for footer projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ContextEstimate {
    pub(super) tokens: Option<u64>,
    pub(super) message_count: usize,
}

impl AppState {
    /// Create an empty projection.
    pub fn new() -> Self {
        Self {
            follow_output: true,
            ..Self::default()
        }
    }

    /// Open or close the temporary full-transcript/detail surface.
    pub(super) fn toggle_tool_detail(&mut self) {
        if self.surface == UiSurface::ToolDetail {
            self.close_surface();
            return;
        }
        self.set_surface_lines(
            UiSurface::ToolDetail,
            full_transcript_detail_lines(&self.transcript),
        );
    }

    /// Apply one typed core event after its reducer has committed state.
    pub fn apply_event(&mut self, event: &AgentEvent) {
        let sequence = Some(event.sequence.0);
        match &event.kind {
            AgentEventKind::AgentStart => self.status = UiStatus::Active,
            AgentEventKind::MessageStart { message } => {
                if let tea_core::AgentMessage::User { content, .. } = message {
                    self.push_entry(
                        sequence,
                        TranscriptEntry::User {
                            text: content.clone(),
                        },
                    );
                }
            }
            AgentEventKind::MessageUpdate {
                message,
                text_delta,
            } => {
                if let (tea_core::AgentMessage::Assistant { .. }, Some(delta)) =
                    (message, text_delta)
                {
                    if let Some(index) = self.streaming_line {
                        if let Some(TranscriptEntry::Assistant { text, .. }) =
                            self.transcript.get_mut(index)
                        {
                            text.push_str(delta);
                        }
                    } else {
                        self.push_entry(
                            sequence,
                            TranscriptEntry::Assistant {
                                text: delta.clone(),
                                streaming: true,
                            },
                        );
                        self.streaming_line = self.transcript.len().checked_sub(1);
                    }
                }
            }
            AgentEventKind::MessageEnd { message } => {
                if let tea_core::AgentMessage::Assistant {
                    content,
                    error_message,
                    ..
                } = message
                {
                    if let Some(index) = self.streaming_line {
                        if let Some(error) = error_message {
                            if let Some(entry) = self.transcript.get_mut(index) {
                                *entry = TranscriptEntry::Error {
                                    text: error.clone(),
                                };
                                self.transcript_sequences[index] = sequence;
                            }
                        } else if let Some(TranscriptEntry::Assistant { text, streaming }) =
                            self.transcript.get_mut(index)
                        {
                            *text = content.clone();
                            *streaming = false;
                            self.transcript_sequences[index] = sequence;
                        }
                    } else if let Some(error) = error_message {
                        self.push_entry(
                            sequence,
                            TranscriptEntry::Error {
                                text: error.clone(),
                            },
                        );
                    } else {
                        self.push_entry(
                            sequence,
                            TranscriptEntry::Assistant {
                                text: content.clone(),
                                streaming: false,
                            },
                        );
                    }
                    self.streaming_line = None;
                }
            }
            AgentEventKind::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let index = self.transcript.len();
                self.push_entry(
                    sequence,
                    TranscriptEntry::Tool(ToolProjection {
                        call_id: tool_call_id.clone(),
                        sequence,
                        transcript_index: index,
                        tool_name: tool_name.clone(),
                        arguments: arguments.as_str().to_owned(),
                        latest_progress: None,
                        settled_result: None,
                        state: ToolState::Started,
                    }),
                );
                self.active_tool_lines.insert(tool_call_id.clone(), index);
            }
            AgentEventKind::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                update,
            } => {
                self.update_tool_line(
                    tool_call_id,
                    sequence,
                    tool_name,
                    ToolState::Progress,
                    Some(update.content.clone()),
                    None,
                );
            }
            AgentEventKind::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                ..
            } => {
                self.update_tool_line(
                    tool_call_id,
                    sequence,
                    tool_name,
                    if result.is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Completed
                    },
                    None,
                    Some(result.content.clone()),
                );
                self.active_tool_lines.remove(tool_call_id);
            }
            // Usage is projected by the attached snapshot/footer; a transcript row
            // would duplicate accounting and blur unknown values with zeroes.
            AgentEventKind::ModelTurnUsage { accounting } => {
                self.reported_usage.accumulate(accounting.usage.clone());
            }
            AgentEventKind::CompactionStart { .. } => {
                self.status = UiStatus::Active;
            }
            AgentEventKind::CompactionResult {
                retained_message_count,
                ..
            } => {
                self.notice(format!(
                    "compaction retained {retained_message_count} messages"
                ));
            }
            AgentEventKind::CompactionEnd { outcome } => match outcome {
                CompactionOutcome::Succeeded {
                    retained_message_count,
                } => self.notice(format!(
                    "compaction complete: {retained_message_count} messages"
                )),
                CompactionOutcome::Failed { message } => {
                    self.notice(format!("compaction failed: {message}"))
                }
                CompactionOutcome::Cancelled => self.notice("compaction cancelled"),
            },
            AgentEventKind::AutomaticCompactionStart { .. } => {
                self.status = UiStatus::Active;
            }
            AgentEventKind::AutomaticCompactionEnd { outcome, .. } => match outcome {
                AutomaticCompactionOutcome::Succeeded { .. } => {
                    self.notice("automatic compaction complete")
                }
                AutomaticCompactionOutcome::Failed { message } => {
                    self.notice(format!("automatic compaction failed: {message}"))
                }
                AutomaticCompactionOutcome::Cancelled => {
                    self.notice("automatic compaction cancelled")
                }
                AutomaticCompactionOutcome::LimitReached => {
                    self.notice("automatic compaction limit reached")
                }
                AutomaticCompactionOutcome::StillAboveThreshold => self.notice(
                    "automatic compaction complete; retained context remains above threshold",
                ),
                AutomaticCompactionOutcome::Unavailable => {
                    self.notice("automatic compaction unavailable")
                }
            },
            AgentEventKind::ContextEstimate {
                estimated_context_tokens,
                message_count,
                ..
            } => {
                self.context_estimate = Some(ContextEstimate {
                    tokens: *estimated_context_tokens,
                    message_count: *message_count,
                });
            }
            AgentEventKind::ProviderRequestSkipped { reason } => self.notice(match reason {
                ProviderRequestSkipReason::AutomaticCompaction => {
                    "provider request deferred for automatic compaction"
                }
                ProviderRequestSkipReason::ToolCircuitBreaker => {
                    "provider request skipped after terminal tool failure"
                }
            }),
            AgentEventKind::ToolFailureObserved {
                disposition,
                consecutive_count,
                terminal,
                ..
            } => self.notice(format!(
                "tool failure {disposition:?} (consecutive {consecutive_count}){}",
                if *terminal { "; ending run" } else { "" }
            )),
            AgentEventKind::TurnEnd { reason, .. } => match reason {
                tea_core::state::StopReason::Error => {
                    self.notice("turn failed; prompt remains available to retry")
                }
                tea_core::state::StopReason::Aborted => self.notice("turn aborted"),
                tea_core::state::StopReason::Cancelled => self.notice("turn cancelled"),
                _ => {}
            },
            AgentEventKind::AgentEnd { .. } => {
                if matches!(self.status, UiStatus::Active) {
                    self.status = UiStatus::Idle;
                }
            }
            AgentEventKind::CompactionLifecycle { .. }
            | AgentEventKind::ProviderRequestObserved { .. }
            | AgentEventKind::TurnStart { .. } => {}
        }
    }

    /// Rebuild the visible transcript from a restored canonical conversation.
    ///
    /// These rows deliberately have no event sequence: loading a session is a host projection,
    /// not a replay of historical core events. Future events continue from the live subscription.
    pub(super) fn restore_messages(&mut self, messages: &[AgentMessage]) {
        self.clear_transcript();
        for message in messages {
            match message {
                AgentMessage::User { content, .. } => {
                    self.push_entry(
                        None,
                        TranscriptEntry::User {
                            text: content.clone(),
                        },
                    );
                }
                AgentMessage::Assistant {
                    content,
                    error_message,
                    ..
                } => {
                    if let Some(error) = error_message {
                        self.push_entry(
                            None,
                            TranscriptEntry::Error {
                                text: error.clone(),
                            },
                        );
                    } else {
                        self.push_entry(
                            None,
                            TranscriptEntry::Assistant {
                                text: content.clone(),
                                streaming: false,
                            },
                        );
                    }
                }
                AgentMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    content,
                    is_error,
                    ..
                } => {
                    let state = if *is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Completed
                    };
                    let transcript_index = self.transcript.len();
                    self.push_entry(
                        None,
                        TranscriptEntry::Tool(ToolProjection {
                            call_id: tool_call_id.clone(),
                            sequence: None,
                            transcript_index,
                            tool_name: tool_name.clone(),
                            arguments: String::new(),
                            latest_progress: None,
                            settled_result: Some(content.clone()),
                            state,
                        }),
                    );
                }
            }
        }
    }

    /// Borrow the raw typed transcript projection.
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    /// Snapshot the typed presentation contract for callers that need ownership.
    pub fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        self.transcript.clone()
    }

    /// Borrow the local composer.
    pub fn composer(&self) -> &Composer {
        &self.composer
    }

    /// Mutably borrow the local composer.
    pub fn composer_mut(&mut self) -> &mut Composer {
        &mut self.composer
    }

    /// Borrow the presentation status.
    pub fn status(&self) -> &UiStatus {
        &self.status
    }

    /// Return the active temporary presentation surface.
    pub fn surface(&self) -> UiSurface {
        self.surface
    }

    /// Borrow the temporary surface payload, if the active surface owns one.
    pub fn surface_lines(&self) -> Option<&[String]> {
        (!self.surface_lines.is_empty()).then_some(self.surface_lines.as_slice())
    }

    /// Return the first unwrapped line displayed by the active surface.
    pub(crate) fn surface_offset(&self) -> usize {
        self.surface_offset
    }

    pub(super) fn set_surface(&mut self, surface: UiSurface) {
        self.surface = surface;
        self.surface_lines.clear();
        self.surface_offset = 0;
    }

    pub(super) fn set_surface_lines(&mut self, surface: UiSurface, lines: Vec<String>) {
        self.surface = surface;
        self.surface_lines = lines;
        self.surface_offset = 0;
        self.picker = None;
        self.slash_completion = None;
    }

    pub(super) fn close_surface(&mut self) {
        self.surface = UiSurface::None;
        self.surface_lines.clear();
        self.surface_offset = 0;
        self.picker = None;
        self.slash_completion = None;
    }

    pub(crate) fn update_slash_completion(&mut self, matches: Vec<String>) {
        if matches.is_empty() {
            self.slash_completion = None;
            return;
        }
        let prefix = self.composer.text().to_owned();
        let selected = self
            .slash_completion
            .as_ref()
            .map_or(0, |menu| menu.selected.min(matches.len() - 1));
        self.slash_completion = Some(SlashCompletion {
            prefix,
            selected,
            matches,
        });
    }

    pub(super) fn move_slash_completion(&mut self, delta: isize) {
        let Some(menu) = self.slash_completion.as_mut() else {
            return;
        };
        if !menu.matches.is_empty() {
            menu.selected =
                (menu.selected as isize + delta).rem_euclid(menu.matches.len() as isize) as usize;
        }
    }

    pub(super) fn selected_slash_completion(&self) -> Option<&str> {
        self.slash_completion
            .as_ref()
            .and_then(|menu| menu.matches.get(menu.selected))
            .map(String::as_str)
    }

    /// Return visible command names, descriptions, and selection state.
    pub(crate) fn slash_completion_rows(&self, max_rows: usize) -> Vec<(String, String, bool)> {
        let Some(menu) = self.slash_completion.as_ref() else {
            return Vec::new();
        };
        let visible = max_rows.min(menu.matches.len());
        if visible == 0 {
            return Vec::new();
        }
        let start = menu
            .selected
            .saturating_sub(visible.saturating_sub(1))
            .min(menu.matches.len().saturating_sub(visible));
        menu.matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, command)| {
                let help =
                    commands::find(command).map_or_else(String::new, |spec| spec.help.to_owned());
                (command.clone(), help, index == menu.selected)
            })
            .collect()
    }

    /// Return the number of command candidates before the visible row cap.
    pub(crate) fn slash_completion_count(&self) -> usize {
        self.slash_completion
            .as_ref()
            .map_or(0, |menu| menu.matches.len())
    }

    /// Return the requested transcript top row for manual scrolling.
    pub fn viewport_offset(&self) -> usize {
        self.viewport_offset
    }

    /// Whether output should continue to follow the newest event.
    pub fn follows_output(&self) -> bool {
        self.follow_output
    }

    /// Return the compact, event-derived telemetry lines for the fixed footer.
    pub(crate) fn footer_lines(&self, registry: &ProviderRegistry) -> [String; 2] {
        let selected = self.selected_model.as_ref();
        let model = selected
            .map(|model| format!("{}/{}", model.provider, model.model))
            .unwrap_or_else(|| "provider/model unknown".into());
        let hint = if self.composer.text().starts_with('/') {
            format!(
                "commands: {}",
                commands::all()
                    .iter()
                    .map(|command| command.name)
                    .collect::<Vec<_>>()
                    .join(" · ")
            )
        } else {
            match &self.status {
                UiStatus::Active => format!(
                    "⏺ Asking · {model} · effort {}",
                    super::support::thinking_level_name(self.thinking_level)
                ),
                UiStatus::Idle | UiStatus::Notice(_) | UiStatus::Error(_) => {
                    format!(
                        "{model} · effort {}",
                        super::support::thinking_level_name(self.thinking_level)
                    )
                }
            }
        };
        let capacity = self
            .selected_context_window
            .map(NonZeroU64::get)
            .or_else(|| {
                selected
                    .and_then(|model| registry.provider(&model.provider)?.model(&model.model))
                    .and_then(|model| model.context_window)
            });
        let context = format!(
            "ctx {}%/{}{}",
            self.context_estimate
                .as_ref()
                .and_then(|estimate| estimate.tokens)
                .map(|tokens| format_context_percent(Some(tokens), capacity))
                .unwrap_or_else(|| "?".into()),
            capacity
                .map(super::support::format_compact_tokens)
                .unwrap_or_else(|| "?".into()),
            if self.automatic_compaction_enabled {
                " (auto)"
            } else {
                ""
            }
        );
        let mut stats = vec![context];
        if let Some(cost) = self.reported_usage.cost.as_deref() {
            stats.push(format!("${cost}"));
        }
        if let Some(tokens) = self.reported_usage.input_tokens {
            stats.push(format!("↑{}", super::support::format_compact_tokens(tokens)));
        }
        if let Some(tokens) = self.reported_usage.output_tokens {
            stats.push(format!("↓{}", super::support::format_compact_tokens(tokens)));
        }
        if let Some(tokens) = self.reported_usage.reasoning_tokens {
            stats.push(format!(
                "reason {}",
                super::support::format_compact_tokens(tokens)
            ));
        }
        if let Some(tokens) = self.reported_usage.cache_read_tokens {
            stats.push(format!("R{}", super::support::format_compact_tokens(tokens)));
        }
        if let Some(tokens) = self.reported_usage.cache_write_tokens {
            stats.push(format!("W{}", super::support::format_compact_tokens(tokens)));
        }
        if let (Some(input), Some(read), Some(write)) = (
            self.reported_usage.input_tokens,
            self.reported_usage.cache_read_tokens,
            self.reported_usage.cache_write_tokens,
        ) {
            let prompt_tokens = input.saturating_add(read).saturating_add(write);
            if prompt_tokens != 0 {
                stats.push(format!(
                    "CH{}%",
                    read.saturating_mul(100) / prompt_tokens
                ));
            }
        }
        [hint, stats.join(" · ")]
    }

    pub(crate) fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.thinking_level = level;
    }

    pub(crate) fn thinking_level(&self) -> ThinkingLevel {
        self.thinking_level
    }

    /// Return the transient footer notice separately from the stable model line.
    pub(crate) fn footer_notice(&self) -> Option<(&str, bool)> {
        match &self.status {
            UiStatus::Notice(notice) => Some((notice, false)),
            UiStatus::Error(error) => Some((error, true)),
            UiStatus::Idle | UiStatus::Active => None,
        }
    }

    /// Return v1 picker lines for the renderer, if an overlay is active.
    pub fn picker_lines(&self, registry: &ProviderRegistry) -> Option<Vec<String>> {
        self.picker_lines_visible(registry, usize::MAX)
    }

    pub(crate) fn picker_lines_visible(
        &self,
        registry: &ProviderRegistry,
        max_rows: usize,
    ) -> Option<Vec<String>> {
        let picker = self.picker.as_ref()?;
        Some(match picker {
            Picker::Model { filter, selected } => {
                let candidates = model_candidates(registry, filter);
                let display = candidates
                    .iter()
                    .copied()
                    .map(|candidate| candidate.label())
                    .collect::<Vec<_>>();
                overlay_lines("Models", filter, &display, *selected, max_rows)
            }
            Picker::CustomModel { provider, input } => vec![
                format!("custom model for {provider}"),
                format!("> {input}"),
                "Enter selects; Esc cancels".into(),
            ],
            Picker::Session {
                filter,
                selected,
                entries,
            } => {
                let filter_lower = filter.to_ascii_lowercase();
                let rows = entries
                    .iter()
                    .filter(|entry| {
                        let model = entry
                            .model
                            .as_ref()
                            .map(|model| format!("{} {}", model.provider, model.model))
                            .unwrap_or_default();
                        format!("{} {model}", entry.id)
                            .to_ascii_lowercase()
                            .contains(&filter_lower)
                    })
                    .map(|entry| {
                        let model = entry
                            .model
                            .as_ref()
                            .map(|model| format!("{}/{}", model.provider, model.model))
                            .unwrap_or_else(|| "unknown model".into());
                        format!("{} · {model} · durable", entry.id)
                    })
                    .collect::<Vec<_>>();
                overlay_lines("Sessions", filter, &rows, *selected, max_rows)
            }
        })
    }

    pub(super) fn push_entry(&mut self, sequence: Option<u64>, entry: TranscriptEntry) {
        self.transcript.push(entry);
        self.transcript_sequences.push(sequence);
    }

    fn update_tool_line(
        &mut self,
        tool_call_id: &ToolCallId,
        sequence: Option<u64>,
        tool_name: &str,
        state: ToolState,
        progress: Option<String>,
        result: Option<String>,
    ) {
        if let Some(index) = self.active_tool_lines.get(tool_call_id).copied() {
            if let Some(TranscriptEntry::Tool(projection)) = self.transcript.get_mut(index) {
                projection.sequence = sequence;
                projection.tool_name = tool_name.to_owned();
                projection.state = state;
                if progress.is_some() {
                    projection.latest_progress = progress;
                }
                if result.is_some() {
                    projection.settled_result = result;
                }
                self.transcript_sequences[index] = sequence;
                return;
            }
        }
        let index = self.transcript.len();
        self.push_entry(
            sequence,
            TranscriptEntry::Tool(ToolProjection {
                call_id: tool_call_id.clone(),
                sequence,
                transcript_index: index,
                tool_name: tool_name.to_owned(),
                arguments: String::new(),
                latest_progress: progress,
                settled_result: result,
                state,
            }),
        );
        self.active_tool_lines.insert(tool_call_id.clone(), index);
    }

    pub(super) fn notice(&mut self, text: impl Into<String>) {
        self.status = UiStatus::Notice(text.into());
    }

    pub(super) fn error(&mut self, text: impl Into<String>) {
        self.status = UiStatus::Error(text.into());
    }

    pub(crate) fn welcome_line(&mut self) {
        self.push_entry(
            None,
            TranscriptEntry::Welcome {
                text: format!(
                    "tea v{} · Run /help for commands",
                    env!("CARGO_PKG_VERSION")
                ),
            },
        );
    }

    pub(super) fn record_history(&mut self, prompt: &str) {
        if prompt.trim().is_empty() {
            return;
        }
        if self
            .history
            .last()
            .is_none_or(|previous| previous != prompt)
        {
            self.history.push(prompt.to_owned());
        }
        self.history_index = None;
        self.history_draft = None;
    }

    pub(super) fn begin_history_navigation(&mut self) {
        if self.history_index.is_none() {
            self.history_draft = Some(self.composer.text().to_owned());
        }
    }

    pub(super) fn history_previous(&mut self) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        let index = self
            .history_index
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_index = Some(index);
        self.history.get(index).cloned()
    }

    pub(super) fn history_next(&mut self) -> Option<String> {
        let Some(index) = self.history_index else {
            return None;
        };
        let next = index + 1;
        if next >= self.history.len() {
            self.history_index = None;
            return Some(self.history_draft.take().unwrap_or_default());
        }
        self.history_index = Some(next);
        self.history.get(next).cloned()
    }

    pub(super) fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.transcript_sequences.clear();
        self.streaming_line = None;
        self.active_tool_lines.clear();
        self.reported_usage = Usage::default();
        self.viewport_offset = 0;
        self.follow_output = true;
    }

    pub(super) fn clear_history(&mut self) {
        self.history.clear();
        self.history_index = None;
        self.history_draft = None;
    }

    pub(super) fn page_up(&mut self, lines: usize) {
        let current = if self.follow_output {
            self.visible_transcript_lines
                .saturating_sub(self.transcript_rows)
        } else {
            self.viewport_offset
        };
        self.follow_output = false;
        self.viewport_offset = current.saturating_sub(lines);
    }

    pub(super) fn page_down(&mut self, lines: usize) {
        self.viewport_offset = self.viewport_offset.saturating_add(lines);
        if self.viewport_offset
            >= self
                .visible_transcript_lines
                .saturating_sub(self.transcript_rows)
        {
            self.follow_output = true;
        }
    }

    /// Scroll a temporary surface without changing live transcript follow state.
    pub(super) fn page_surface_up(&mut self, lines: usize) {
        self.surface_offset = self.surface_offset.saturating_sub(lines);
    }

    /// Scroll a temporary surface without changing live transcript follow state.
    pub(super) fn page_surface_down(&mut self, lines: usize) {
        self.surface_offset = self
            .surface_offset
            .saturating_add(lines)
            .min(self.surface_lines.len().saturating_sub(1));
    }

    pub(super) fn follow_end(&mut self) {
        self.follow_output = true;
        self.viewport_offset = self.transcript.len();
    }

    pub(super) fn set_viewport_metrics(
        &mut self,
        visible_transcript_lines: usize,
        transcript_rows: usize,
    ) {
        self.visible_transcript_lines = visible_transcript_lines;
        self.transcript_rows = transcript_rows;
    }
}

fn full_transcript_detail_lines(entries: &[TranscriptEntry]) -> Vec<String> {
    if entries.is_empty() {
        return vec![
            "Full detail".into(),
            String::new(),
            "No transcript yet.".into(),
        ];
    }

    let mut lines = vec!["Full detail".into(), String::new()];
    for (index, entry) in entries.iter().enumerate() {
        if index != 0 {
            lines.push(String::new());
        }
        match entry {
            TranscriptEntry::Welcome { text } => {
                lines.push("Welcome".into());
                lines.extend(text.lines().map(str::to_owned));
            }
            TranscriptEntry::User { text } => {
                lines.push("User".into());
                lines.extend(text.lines().map(str::to_owned));
            }
            TranscriptEntry::Assistant { text, streaming } => {
                lines.push(if *streaming {
                    "Assistant (streaming)".into()
                } else {
                    "Assistant".into()
                });
                lines.extend(text.lines().map(str::to_owned));
            }
            TranscriptEntry::Tool(tool) => {
                lines.push(format!("Tool: {} ({:?})", tool.tool_name, tool.state));
                if !tool.arguments.is_empty() {
                    lines.push("Arguments".into());
                    lines.extend(tool.arguments.lines().map(str::to_owned));
                }
                if let Some(progress) = &tool.latest_progress {
                    lines.push("Progress".into());
                    lines.extend(progress.lines().map(str::to_owned));
                }
                if let Some(result) = &tool.settled_result {
                    lines.push("Result".into());
                    lines.extend(result.lines().map(str::to_owned));
                }
            }
            TranscriptEntry::Notice { text, severity } => {
                lines.push(format!("Notice ({severity:?})"));
                lines.extend(text.lines().map(str::to_owned));
            }
            TranscriptEntry::Error { text } => {
                lines.push("Error".into());
                lines.extend(text.lines().map(str::to_owned));
            }
        }
    }
    lines
}

fn format_context_percent(tokens: Option<u64>, capacity: Option<u64>) -> String {
    match (tokens, capacity.filter(|capacity| *capacity != 0)) {
        (Some(tokens), Some(capacity)) => {
            let percent = u128::from(tokens).saturating_mul(100) / u128::from(capacity);
            percent.to_string()
        }
        _ => "unknown".into(),
    }
}
