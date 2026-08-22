use crate::terminal::{KeyCode, KeyEvent, KeyModifiers};
use std::num::NonZeroU64;
use std::sync::Arc;
use tea_core::compaction::{AutomaticCompactionPolicy, ContextBudgetSource, OverflowRecovery};
use tea_core::provider::{ConfiguredProvider, ProviderConfiguration};

use super::error::AppError;
use super::host::model_candidates;
use super::preferences::save_last_model;
use super::runtime::App;
use super::session::{SessionRecord, SessionStore};
use super::state::{Picker, UiSurface};
use super::support::utc_date;

impl App {
    pub(super) fn open_model_picker(&mut self) {
        if self.agent_is_active() {
            self.state.notice("model changes require an idle agent");
            return;
        }
        self.state.picker = Some(Picker::Model {
            filter: String::new(),
            selected: 0,
        });
        self.state.set_surface(UiSurface::ModelPicker);
    }

    pub(super) fn open_session_picker(&mut self) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state.notice("session changes require an idle agent");
            return Ok(());
        }
        let (Some(home), Some(workspace)) = (self.tea_home.as_ref(), self.workspace.as_ref())
        else {
            return Err(AppError::Setup("Tea home is not initialized".into()));
        };
        let entries = SessionStore::new(home).for_workspace(workspace).list()?;
        if entries.is_empty() {
            self.state.close_surface();
            self.state.notice("no saved sessions");
            return Ok(());
        }
        self.state.picker = Some(Picker::Session {
            filter: String::new(),
            selected: 0,
            entries,
        });
        self.state.set_surface(UiSurface::SessionPicker);
        Ok(())
    }

    pub(super) fn resume_session(&mut self, id: &str) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state.notice("session changes require an idle agent");
            return Ok(());
        }
        let (Some(home), Some(workspace)) = (self.tea_home.as_ref(), self.workspace.as_ref())
        else {
            return Err(AppError::Setup("Tea home is not initialized".into()));
        };
        let record = SessionStore::new(home).for_workspace(workspace).load(id)?;
        if !record.cwd.is_empty() && record.cwd != workspace.to_string_lossy() {
            return Err(AppError::Setup(format!(
                "session belongs to workspace {}; current workspace is {}",
                record.cwd,
                workspace.display()
            )));
        }
        let messages = record.messages.clone();
        tea_core::Agent::validate_messages(&messages)?;
        if let Some(model) = record.model.clone() {
            self.select_model(model.provider, model.model)?;
        }
        let agent = self.agent_or_setup()?.clone();
        agent.replace_thinking_level(record.thinking_level)?;
        agent.restore_messages(messages.clone())?;
        self.state.restore_messages(&messages);
        self.state.set_snapshot(agent.snapshot());
        self.current_session = Some(record);
        self.state.close_surface();
        self.state.notice(format!("resumed session {id}"));
        Ok(())
    }

    pub(super) fn new_session(&mut self) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state.notice("new session requires an idle agent");
            return Ok(());
        }
        if !self.persist_session() {
            self.state
                .notice("new session cancelled; the current session was not saved");
            return Ok(());
        }
        let agent = self.agent_or_setup()?.clone();
        agent.reset()?;
        let thinking_level = agent.snapshot().thinking_level;
        self.state.clear_transcript();
        self.state.clear_history();
        self.state.composer_mut().clear();
        self.state.set_snapshot(agent.snapshot());
        self.state.close_surface();
        self.current_session = Some(
            SessionRecord::new(self.state.selected_model.clone(), thinking_level).with_workspace(
                self.workspace
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        );
        self.state.notice("new session");
        Ok(())
    }

    pub(super) fn handle_picker_key(&mut self, key: KeyEvent) -> Result<(), AppError> {
        match key.code {
            KeyCode::Esc => self.state.close_surface(),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Backspace => self.picker_backspace(),
            KeyCode::Enter => self.commit_picker()?,
            KeyCode::Char(character)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.picker_insert(&character.to_string())?
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn picker_insert(&mut self, text: &str) -> Result<(), AppError> {
        let Some(picker) = self.state.picker.as_mut() else {
            return Ok(());
        };
        match picker {
            Picker::Model { filter, selected } => {
                filter.push_str(text);
                *selected = 0;
            }
            Picker::CustomModel { input, .. } => input.push_str(text),
            Picker::Session {
                filter, selected, ..
            } => {
                filter.push_str(text);
                *selected = 0;
            }
        }
        Ok(())
    }

    fn picker_backspace(&mut self) {
        let Some(picker) = self.state.picker.as_mut() else {
            return;
        };
        match picker {
            Picker::Model { filter, selected } => {
                filter.pop();
                *selected = 0;
            }
            Picker::CustomModel { input, .. } => {
                input.pop();
            }
            Picker::Session {
                filter, selected, ..
            } => {
                filter.pop();
                *selected = 0;
            }
        }
    }

    fn picker_move(&mut self, delta: isize) {
        let Some(picker) = self.state.picker.as_mut() else {
            return;
        };
        let length = match picker {
            Picker::Model { filter, .. } => model_candidates(&self.registry, filter).len(),
            Picker::CustomModel { .. } => return,
            Picker::Session {
                filter, entries, ..
            } => entries
                .iter()
                .filter(|entry| {
                    let model = entry
                        .model
                        .as_ref()
                        .map(|model| format!("{} {}", model.provider, model.model))
                        .unwrap_or_default();
                    format!("{} {model}", entry.id)
                        .to_ascii_lowercase()
                        .contains(&filter.to_ascii_lowercase())
                })
                .count(),
        };
        let selected = match picker {
            Picker::Model { selected, .. } => selected,
            Picker::CustomModel { .. } => return,
            Picker::Session { selected, .. } => selected,
        };
        if length != 0 {
            *selected = (*selected as isize + delta).rem_euclid(length as isize) as usize;
        }
    }

    fn commit_picker(&mut self) -> Result<(), AppError> {
        let Some(picker) = self.state.picker.clone() else {
            return Ok(());
        };
        match picker {
            Picker::Model { filter, selected } => {
                let candidates = model_candidates(&self.registry, &filter);
                if let Some(candidate) = candidates.get(selected).copied() {
                    if let Some(model) = candidate.model_id() {
                        if let Err(error) =
                            self.select_model(candidate.provider.to_owned(), model.to_owned())
                        {
                            self.state.notice(error.to_string());
                            self.state.picker = Some(Picker::Model { filter, selected });
                            self.state.set_surface(UiSurface::ModelPicker);
                        }
                    } else {
                        self.state.picker = Some(Picker::CustomModel {
                            provider: candidate.provider.to_owned(),
                            input: String::new(),
                        });
                        self.state.set_surface(UiSurface::CustomModel);
                    }
                }
            }
            Picker::CustomModel { provider, input } => {
                if input.trim().is_empty() {
                    self.state.notice("custom model ID cannot be empty");
                } else {
                    if let Err(error) = self.select_model(provider.clone(), input.clone()) {
                        self.state.notice(error.to_string());
                        self.state.picker = Some(Picker::CustomModel { provider, input });
                        self.state.set_surface(UiSurface::CustomModel);
                    }
                }
            }
            Picker::Session {
                filter,
                selected,
                entries,
            } => {
                let matches = entries
                    .iter()
                    .filter(|entry| {
                        let model = entry
                            .model
                            .as_ref()
                            .map(|model| format!("{} {}", model.provider, model.model))
                            .unwrap_or_default();
                        format!("{} {model}", entry.id)
                            .to_ascii_lowercase()
                            .contains(&filter.to_ascii_lowercase())
                    })
                    .collect::<Vec<_>>();
                if let Some(summary) = matches.get(selected) {
                    let id = summary.id.clone();
                    if let Err(error) = self.resume_session(&id) {
                        self.state.notice(error.to_string());
                        self.state.picker = Some(Picker::Session {
                            filter,
                            selected,
                            entries,
                        });
                        self.state.set_surface(UiSurface::SessionPicker);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn select_model(&mut self, provider: String, model: String) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state.notice("model changes require an idle agent");
            return Ok(());
        }
        if provider != "local" && self.options.local_context_window().is_some() {
            return Err(AppError::Setup(
                "--local-context-window requires --provider local".into(),
            ));
        }
        let configured = self.configured_provider(&provider, &model)?;
        let descriptor = configured.descriptor.clone();
        let configured_provider = configured.provider;
        self.agent_or_setup()?
            .replace_model_provider(descriptor.clone(), Arc::clone(&configured_provider))?;
        if let Some(compactor) = &self.compactor {
            compactor.configure(descriptor.clone(), Arc::clone(&configured_provider));
        }
        let context_window = self.options.local_context_window().or_else(|| {
            self.registry
                .provider(&provider)
                .and_then(|entry| entry.model(&model))
                .and_then(|model| model.context_window)
                .and_then(NonZeroU64::new)
        });
        let policy = if self.compactor.is_some() {
            context_window
                .map(automatic_compaction_policy)
                .unwrap_or_else(AutomaticCompactionPolicy::disabled)
        } else {
            AutomaticCompactionPolicy::disabled()
        };
        self.agent_or_setup()?
            .replace_automatic_compaction(policy)?;
        self.state.automatic_compaction_enabled =
            self.agent_or_setup()?.automatic_compaction().enabled;
        self.state.selected_context_window = context_window;
        self.state.selected_model = Some(descriptor.clone());
        if let Some(session) = self.current_session.as_mut() {
            session.model = self.state.selected_model.clone();
        }
        self.state.context_estimate = None;
        self.state.close_surface();
        self.state.set_snapshot(self.agent_or_setup()?.snapshot());
        self.state.notice("model selected");
        if let Some(home) = self.tea_home.as_ref() {
            if let Err(error) = save_last_model(home, &descriptor) {
                self.state.notice(format!(
                    "model selected but preference was not saved: {error}"
                ));
            }
        }
        Ok(())
    }

    fn configured_provider(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<ConfiguredProvider, AppError> {
        let descriptor = self
            .registry
            .resolve_model(provider, model.to_owned())?
            .into_descriptor();
        let configuration = match provider {
            "openrouter" => {
                let key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
                    AppError::Setup("OPENROUTER_API_KEY is required for OpenRouter".into())
                })?;
                let config = tea_core::provider::openrouter::OpenRouterConfig::try_new(key, model)
                    .map_err(|error| AppError::Setup(error.to_string()))?;
                #[cfg(feature = "pty-harness")]
                let config = test_openrouter_config(config)?;
                ProviderConfiguration::OpenRouter(config)
            }
            "command-code" => {
                let key = std::env::var("COMMANDCODE_API_KEY").map_err(|_| {
                    AppError::Setup("COMMANDCODE_API_KEY is required for Command Code".into())
                })?;
                let workspace = self
                    .workspace
                    .as_ref()
                    .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?;
                let host = tea_core::provider::commandcode::CommandCodeHostContext::new(
                    workspace.to_string_lossy(),
                    utc_date(),
                    std::env::consts::OS,
                )
                .map_err(|error| AppError::Setup(error.to_string()))?;
                ProviderConfiguration::CommandCode(
                    tea_core::provider::commandcode::CommandCodeConfig::new(key, model, host)
                        .map_err(|error| AppError::Setup(error.to_string()))?,
                )
            }
            "local" => {
                let base_url = self
                    .options
                    .local_base_url()
                    .map(|value| super::runtime::os_text(value, "--local-base-url"))
                    .transpose()?
                    .unwrap_or_else(|| tea_core::provider::local::DEFAULT_BASE_URL.to_owned());
                let config = tea_core::provider::local::LocalConfig::try_new(base_url, model)
                    .map_err(|error| AppError::Setup(error.to_string()))?;
                ProviderConfiguration::Local(config)
            }
            _ => {
                return Err(AppError::Setup(format!(
                    "provider {provider:?} is not compiled in"
                )))
            }
        };
        self.registry
            .build(descriptor, configuration)
            .map_err(Into::into)
    }
}

pub(super) fn automatic_compaction_policy(context_window: NonZeroU64) -> AutomaticCompactionPolicy {
    let capacity = context_window.get();
    // Reserve room for the summary request and keep a bounded intact suffix. Large
    // OpenRouter windows use fixed practical bounds; smaller windows retain proportional room.
    AutomaticCompactionPolicy {
        enabled: true,
        context_budget: ContextBudgetSource::ContextWindow(context_window),
        reserved_tokens: (capacity / 4).min(16_384),
        minimum_headroom_tokens: (capacity / 4).min(16_384),
        recent_tokens: (capacity / 2).min(20_000),
        overflow_recovery: OverflowRecovery::CompactAndRetry,
        max_compactions_per_run: 4,
        max_overflow_retries_per_run: 1,
    }
}

#[cfg(feature = "pty-harness")]
fn test_openrouter_config(
    config: tea_core::provider::openrouter::OpenRouterConfig,
) -> Result<tea_core::provider::openrouter::OpenRouterConfig, AppError> {
    let Some(url) = std::env::var_os("TEA_AGENT_TEST_OPENROUTER_URL") else {
        return Ok(config);
    };
    let url = url.to_str().ok_or_else(|| {
        AppError::Setup("TEA_AGENT_TEST_OPENROUTER_URL must be valid UTF-8".into())
    })?;
    Ok(config.with_test_completion_url(url))
}
