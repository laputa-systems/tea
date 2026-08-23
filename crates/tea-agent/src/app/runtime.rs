use crate::render;
use crate::terminal::{TerminalError, TerminalGuard};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::time::Duration;
use tea_core::coding::DefaultCodingTools;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::harness::HarnessError;
use tea_core::runtime::{HarnessEvent, SessionEvent, TeaEvent, TeaEventSubscription};
use tea_core::agent::AgentConfiguration;
use tea_core::error::CoreError;
use tea_providers::ProviderRegistry;
use tea_tui::Size;

use super::cli::CliOptions;
use super::compaction::ProviderCompactor;
use super::error::AppError;
use super::host::host_configuration;
use super::mock;
use super::preferences::load_last_model;
use super::state::{AppState, UiStatus};
use std::sync::Arc;
use tea_core::state::ThinkingLevel;

/// Assembled v1 terminal application.
pub struct App {
    pub(super) options: CliOptions,
    pub(super) state: AppState,
    /// Immutable prompt, tools, and hooks captured by every durable epoch.
    pub(super) configuration: Option<AgentConfiguration>,
    pub(super) compactor: Option<Arc<ProviderCompactor>>,
    /// Host-selected policy captured with the next immutable durable epoch.
    pub(super) automatic_compaction: AutomaticCompactionPolicy,
    /// Provider selected by the host for the next immutable durable epoch.
    pub(super) configured_provider: Option<Arc<dyn tea_core::scheduler::ModelProvider>>,
    /// The one session-owned durable supervisor for the current terminal session.
    pub(super) durable_harness: Option<Arc<super::durable::HostHarness>>,
    /// Bounded application events for the durable supervisor.
    pub(super) durable_subscription: Option<TeaEventSubscription>,
    /// Completion channel for the current durable operation.
    pub(super) durable_task: Option<Receiver<Result<(), HarnessError>>>,
    pub(super) tea_home: Option<PathBuf>,
    pub(super) registry: ProviderRegistry,
    pub(super) workspace: Option<PathBuf>,
    /// The idle prompt handed to the current run, retained only to restore local input after a
    /// failed or cancelled operation. The durable session remains the transcript source of truth.
    pub(super) submitted_prompt: Option<String>,
    /// Number of front-contiguous semantic entries already written once into
    /// native terminal scrollback for this presentation generation.
    pub(super) committed_entries: usize,
    /// Last semantic projection replacement rendered by this terminal host.
    pub(super) rendered_projection_generation: u64,
    pub(super) quitting: bool,
}

impl App {
    /// Assemble an application from explicit command-line values.
    pub fn new(options: CliOptions) -> Self {
        Self {
            options,
            state: AppState::new(),
            configuration: None,
            compactor: None,
            automatic_compaction: AutomaticCompactionPolicy::disabled(),
            configured_provider: None,
            durable_harness: None,
            durable_subscription: None,
            durable_task: None,
            tea_home: None,
            registry: ProviderRegistry::new(),
            workspace: None,
            submitted_prompt: None,
            committed_entries: 0,
            rendered_projection_generation: 0,
            quitting: false,
        }
    }

    /// Initialize the durable host configuration and run the terminal loop on Smol.
    pub fn run(&mut self) -> Result<(), AppError> {
        self.assemble_host()?;
        let mut terminal = crate::terminal::TerminalGuard::enter()?;
        smol::block_on(self.event_loop(&mut terminal))
    }

    /// Run one explicit prompt without entering terminal mode, writing only streamed assistant
    /// text to stdout before exiting.
    pub fn run_prompt(&mut self, prompt: String) -> Result<(), AppError> {
        if prompt.trim().is_empty() {
            return Err(AppError::Setup("-p/--prompt must not be empty".into()));
        }
        if self.options.provider().is_none()
            || (self.options.model().is_none()
                && self.options.provider() != Some(OsStr::new(mock::PROVIDER_ID)))
        {
            return Err(AppError::Setup(
                "-p/--prompt requires --provider and --model (except --provider mock)".into(),
            ));
        }
        self.assemble_host()?;
        let harness = self.ensure_durable_harness()?;
        let subscription = self.durable_subscription.take().ok_or_else(|| {
            AppError::Setup("durable event subscription is not initialized".into())
        })?;
        smol::block_on(super::durable::stream_host_prompt(
            harness,
            subscription,
            prompt,
        ))
    }

    /// Borrow startup options.
    pub fn options(&self) -> &CliOptions {
        &self.options
    }

    /// Borrow presentation-only state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Mutably borrow presentation-only state.
    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub(super) fn assemble_host(&mut self) -> Result<(), AppError> {
        if self.configuration.is_some() {
            return Ok(());
        }
        let workspace = match self.options.cwd() {
            Some(path) => path.to_path_buf(),
            None => std::env::current_dir().map_err(|error| {
                AppError::Setup(format!("cannot read current directory: {error}"))
            })?,
        };
        let tools = DefaultCodingTools::new(&workspace)
            .map_err(|error| AppError::Setup(format!("invalid --cwd: {error}")))?;
        self.workspace = Some(tools.workspace().as_path().to_path_buf());
        let configuration = if self.options.provider() == Some(OsStr::new(mock::PROVIDER_ID)) {
            mock::configuration()
        } else {
            host_configuration(tools)?
        };
        let compactor = Arc::new(ProviderCompactor::default());
        self.compactor = Some(compactor);
        let home = resolve_tea_home(self.options.tea_home())?;
        self.configuration = Some(configuration);
        self.tea_home = Some(home);
        self.state.set_thinking_level(self.options.thinking_level());
        self.state.welcome_line();

        let explicit_provider = self.options.provider().map(OsStr::to_owned);
        let explicit_model = self.options.model().map(OsStr::to_owned);
        match (explicit_provider.as_deref(), explicit_model.as_deref()) {
            (None, None) => {
                self.restore_last_model(None);
            }
            (Some(provider), None) if provider == OsStr::new(mock::PROVIDER_ID) => {
                self.select_model(mock::PROVIDER_ID.into(), mock::DEFAULT_MODEL_ID.into())?
            }
            (Some(provider), None) => {
                if !self.restore_last_model(provider.to_str()) {
                    self.state.notice("select a model with /model");
                }
            }
            (Some(provider), Some(model)) => {
                self.select_model(os_text(provider, "--provider")?, os_text(model, "--model")?)?
            }
            (None, Some(_)) => {
                return Err(AppError::Setup(
                    "--model requires an explicit --provider".into(),
                ));
            }
        }
        Ok(())
    }

    fn restore_last_model(&mut self, provider_filter: Option<&str>) -> bool {
        let Some(home) = self.tea_home.as_ref() else {
            return false;
        };
        match load_last_model(home) {
            Ok(Some(model))
                if provider_filter.is_none_or(|provider| provider == model.provider) =>
            {
                let provider = model.provider.clone();
                let model_id = model.model.clone();
                self.state.selected_model = Some(model);
                if let Err(error) = self.select_model(provider, model_id) {
                    self.state
                        .error(format!("last model could not be restored: {error}"));
                }
                true
            }
            Ok(Some(_)) | Ok(None) => false,
            Err(error) => {
                self.state
                    .error(format!("last model could not be read: {error}"));
                true
            }
        }
    }

    async fn event_loop(&mut self, terminal: &mut TerminalGuard) -> Result<(), AppError> {
        loop {
            self.drain_events();
            self.reap_task();
            if self.quitting && self.durable_task.is_none() {
                break;
            }
            self.redraw(terminal)?;
            if let Some(event) = terminal.poll_event(Duration::from_millis(20))? {
                self.handle_terminal_event(terminal, event)?;
            }
            // Terminal input is synchronous by design. Yield after each poll
            // so the caller-owned Smol executor drives model/tool work.
            smol::future::yield_now().await;
        }
        Ok(())
    }

    fn drain_events(&mut self) {
        if let Some(subscription) = self.durable_subscription.as_ref() {
            loop {
                match subscription.try_recv() {
                    Ok(TeaEvent::Agent(event)) => self.state.apply_event(&event),
                    Ok(TeaEvent::Session(SessionEvent::OperationAccepted { .. })) => {
                        // The session writer appended the user entry before
                        // publishing this event. This cache is therefore
                        // session-derived even before a reopen rebuilds it
                        // from the authoritative log.
                        if let Some(prompt) = self.submitted_prompt.as_deref() {
                            self.state.record_history(prompt);
                        }
                    }
                    Ok(TeaEvent::Harness(HarnessEvent::CandidateRejected {
                        stage,
                        code,
                        diagnostic,
                        ..
                    })) => self.state.notice(format!(
                        "harness candidate rejected at {stage:?} ({}) : {diagnostic}",
                        code.as_str()
                    )),
                    Ok(TeaEvent::Harness(_) | TeaEvent::Session(_) | TeaEvent::Artifact(_)) => {}
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            // A managed durable epoch is the authoritative transcript and
            // accounting source for this terminal session. Do not overwrite
            // its projection with the idle configuration agent below.
        }
    }

    pub(super) fn reap_task(&mut self) {
        if let Some(receiver) = self.durable_task.as_ref() {
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    self.durable_task = None;
                    self.submitted_prompt = None;
                    self.state.status = UiStatus::Idle;
                    self.start_queued_prompt();
                }
                Ok(Err(HarnessError::Core(CoreError::Cancelled))) => {
                    self.durable_task = None;
                    self.restore_submitted_prompt(
                        "cancelled; prompt restored for explicit re-submit",
                    );
                }
                Ok(Err(error)) => {
                    self.durable_task = None;
                    self.restore_submitted_prompt(format!(
                        "{error}; prompt restored for explicit re-submit"
                    ));
                }
                Err(TryRecvError::Disconnected) => {
                    self.durable_task = None;
                    self.state
                        .notice("durable operation task ended unexpectedly");
                }
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    fn start_queued_prompt(&mut self) {
        let Some(input) = self.state.take_queued_message() else {
            return;
        };
        if self.configured_provider.is_none() {
            self.state.composer_mut().replace_from_editor(input);
            self.state.notice("select a model first");
            self.open_model_picker();
            return;
        }
        match self.ensure_durable_harness() {
            Ok(harness) => {
                self.submitted_prompt = Some(input.clone());
                self.spawn_durable_prompt(harness, input);
            }
            Err(error) => {
                self.state.composer_mut().replace_from_editor(input);
                self.state.notice(error.to_string());
            }
        }
    }

    fn restore_submitted_prompt(&mut self, notice: impl Into<String>) {
        if self.state.composer().text().is_empty() {
            if let Some(prompt) = self.submitted_prompt.take() {
                self.state.composer_mut().replace_from_editor(prompt);
            }
        }
        self.state.notice(notice);
    }

    fn redraw(&mut self, terminal: &mut TerminalGuard) -> Result<(), AppError> {
        let (width, height) = terminal.size()?;
        let size = Size { width, height };
        if self.rendered_projection_generation != self.state.projection_generation() {
            self.committed_entries = 0;
            self.rendered_projection_generation = self.state.projection_generation();
        }

        if !matches!(self.state.surface(), super::state::UiSurface::None) {
            let presentation = render::surface_presentation(&self.state, &self.registry, size);
            terminal
                .renderer_mut()?
                .draw_surface(&presentation.lines, size, presentation.cursor)
                .map_err(TerminalError::Io)?;
            return Ok(());
        }

        let stable = render::stable_prefix(self.state.transcript());
        if stable > self.committed_entries {
            let lines = render::committed_lines(&self.state, self.committed_entries, stable, width);
            terminal
                .renderer_mut()?
                .commit(&lines)
                .map_err(TerminalError::Io)?;
            self.committed_entries = stable;
        }
        let presentation =
            render::main_presentation(&self.state, &self.registry, size, self.committed_entries);
        terminal
            .renderer_mut()?
            .draw_live(&presentation.live, size, presentation.cursor)
            .map_err(TerminalError::Io)?;
        Ok(())
    }

    /// Begin one new prompt through the session-owned durable supervisor.
    pub(super) fn spawn_durable_prompt(
        &mut self,
        harness: Arc<super::durable::HostHarness>,
        input: String,
    ) {
        let (sender, receiver) = sync_channel(1);
        smol::spawn(async move {
            let _ = sender.send(harness.run_prompt(input).await.map(|_| ()));
        })
        .detach();
        self.durable_task = Some(receiver);
        self.state.status = UiStatus::Active;
    }

    /// Drive the one recovery plan derived from an opened durable session.
    pub(super) fn spawn_durable_recovery(&mut self, harness: Arc<super::durable::HostHarness>) {
        let (sender, receiver) = sync_channel(1);
        smol::spawn(async move {
            let _ = sender.send(harness.resume().await.map(|_| ()));
        })
        .detach();
        self.durable_task = Some(receiver);
        self.state.status = UiStatus::Active;
    }

    pub(super) fn agent_is_active(&self) -> bool {
        self.durable_harness
            .as_ref()
            .is_some_and(|harness| harness.is_active())
            || self.durable_task.is_some()
    }

    pub(super) fn set_thinking_level(&mut self, level: ThinkingLevel) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state.notice("thinking changes require an idle agent");
            return Ok(());
        }
        if let Some(harness) = self.durable_harness.as_ref() {
            harness
                .replace_thinking_level(level)
                .map_err(|error| AppError::Setup(error.to_string()))?;
        }
        self.options.set_thinking_level(level);
        self.state.set_thinking_level(level);
        Ok(())
    }

    /// Lazily create the one immutable managed harness for this terminal
    /// session after provider/model selection. Construction persists the
    /// initial revision before returning, so callers can immediately route a
    /// prompt through it without an unmanaged execution path.
    pub(super) fn ensure_durable_harness(
        &mut self,
    ) -> Result<Arc<super::durable::HostHarness>, AppError> {
        if let Some(harness) = &self.durable_harness {
            return Ok(Arc::clone(harness));
        }
        let configuration = self
            .configuration
            .as_ref()
            .cloned()
            .ok_or_else(|| AppError::Setup("host configuration is not initialized".into()))?;
        let model = self
            .state
            .selected_model
            .clone()
            .ok_or_else(|| AppError::Setup("model is not selected".into()))?;
        let provider = self
            .configured_provider
            .clone()
            .ok_or_else(|| AppError::Setup("provider is not configured".into()))?;
        let workspace = self
            .workspace
            .as_ref()
            .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?;
        let home = self
            .tea_home
            .as_ref()
            .ok_or_else(|| AppError::Setup("Tea home is not initialized".into()))?;
        let automatic_compaction = self.automatic_compaction.clone();
        let harness = super::durable::create_host_harness(
            home,
            workspace,
            configuration,
            model,
            provider,
            self.options.thinking_level(),
            self.compactor.clone(),
            automatic_compaction,
        )?;
        self.durable_subscription = Some(harness.subscribe_events()?);
        self.durable_harness = Some(Arc::clone(&harness));
        Ok(harness)
    }

    /// Replace the idle terminal's current durable writer with an existing
    /// session selected from the explicit workspace-scoped picker. Recovery
    /// begins immediately when the reducer reports an open operation.
    pub(super) fn reopen_durable_session(&mut self, id: &str) -> Result<(), AppError> {
        if self.agent_is_active() {
            return Err(AppError::Setup(
                "session changes require an idle durable harness".into(),
            ));
        }
        let configuration = self
            .configuration
            .as_ref()
            .cloned()
            .ok_or_else(|| AppError::Setup("host configuration is not initialized".into()))?;
        let model = self
            .state
            .selected_model
            .clone()
            .ok_or_else(|| AppError::Setup("model is not selected".into()))?;
        let provider = self
            .configured_provider
            .clone()
            .ok_or_else(|| AppError::Setup("provider is not configured".into()))?;
        let workspace = self
            .workspace
            .as_ref()
            .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?
            .clone();
        let home = self
            .tea_home
            .as_ref()
            .ok_or_else(|| AppError::Setup("Tea home is not initialized".into()))?
            .clone();
        let automatic_compaction = self.automatic_compaction.clone();

        // Drop the prior idle writer before opening another session. This is
        // also what lets a user select the currently displayed session again
        // without fighting its own advisory writer lock.
        self.durable_subscription = None;
        self.durable_harness = None;
        let harness = super::durable::reopen_host_harness(
            &home,
            &workspace,
            id,
            configuration,
            model,
            provider,
            self.compactor.clone(),
            automatic_compaction,
        )?;
        self.state.set_thinking_level(harness.thinking_level()?);
        let snapshot = harness.snapshot()?;
        let messages = super::durable::project_host_messages(&snapshot)?;
        self.state.restore_messages(&messages);
        self.durable_subscription = Some(harness.subscribe_events()?);
        let reduction = tea_session::reduce_lane(snapshot, tea_session::LaneId::main())
            .map_err(|error| AppError::Setup(error.to_string()))?;
        self.state.reported_usage = super::durable::core_usage(&reduction.usage_totals);
        let recovery = reduction.lane_state.active_operation.is_some();
        self.durable_harness = Some(Arc::clone(&harness));
        self.submitted_prompt = None;
        self.state.close_surface();
        if recovery {
            self.spawn_durable_recovery(harness);
            self.state.notice("resumed durable session recovery");
        } else {
            self.state.status = UiStatus::Idle;
            self.state.notice(format!("resumed durable session {id}"));
        }
        Ok(())
    }
}

pub(super) fn os_text(value: &OsStr, flag: &str) -> Result<String, AppError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| AppError::Setup(format!("{flag} must be valid UTF-8")))
}

/// Resolve the terminal-owned durable-state root without exposing home lookup
/// to `tea-core` or the durable crates.
fn resolve_tea_home(override_path: Option<&Path>) -> Result<PathBuf, AppError> {
    if let Some(path) = override_path {
        if path.as_os_str().is_empty() {
            return Err(AppError::Setup("--tea-home must not be empty".into()));
        }
        return Ok(path.to_path_buf());
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| AppError::Setup("could not resolve the user home directory".into()))?;
    Ok(PathBuf::from(home).join(".tea"))
}
