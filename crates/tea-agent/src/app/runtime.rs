use crate::grid::Grid;
use crate::render;
use crate::terminal::TerminalGuard;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::time::Duration;
use tea_core::compaction::CompactionHandle;
use tea_core::event::AgentEventKind;
use tea_core::provider::ProviderRegistry;
use tea_core::state::AgentPhase;
use tea_core::{
    Agent, AgentConfiguration, CoreError, DefaultCodingTools, LosslessEventSubscription, RunHandle,
};

use super::cli::CliOptions;
use super::compaction::ProviderCompactor;
use super::error::AppError;
use super::host::{build_host_agent_with_thinking, compose_tea_configuration};
use super::preferences::load_last_model;
use super::session::{SessionRecord, SessionStore};
use super::state::{AppState, UiStatus};
use super::support::composer_cursor;
use super::tea::{load_tea_extensions, resolve_tea_home, TeaExtensions};
use std::sync::Arc;

/// Assembled v0 terminal application.
#[derive(Debug)]
pub struct App {
    pub(super) options: CliOptions,
    pub(super) state: AppState,
    pub(super) core: Option<Agent>,
    pub(super) compactor: Option<Arc<ProviderCompactor>>,
    pub(super) tea_home: Option<PathBuf>,
    pub(super) tea_extensions: TeaExtensions,
    pub(super) session_store: Option<SessionStore>,
    pub(super) current_session: Option<SessionRecord>,
    pub(super) tea_base_configuration: Option<AgentConfiguration>,
    pub(super) registry: ProviderRegistry,
    pub(super) workspace: Option<PathBuf>,
    pub(super) subscription: Option<LosslessEventSubscription>,
    pub(super) active_task: Option<Receiver<Result<(), CoreError>>>,
    /// The idle prompt handed to the current run, retained only to restore local input after a
    /// failed or cancelled operation. The core remains the transcript source of truth.
    pub(super) submitted_prompt: Option<String>,
    pub(super) previous_grid: Option<Grid>,
    pub(super) quitting: bool,
}

impl App {
    /// Assemble an application from explicit command-line values.
    pub fn new(options: CliOptions) -> Self {
        Self {
            options,
            state: AppState::new(),
            core: None,
            compactor: None,
            tea_home: None,
            tea_extensions: TeaExtensions::default(),
            session_store: None,
            current_session: None,
            tea_base_configuration: None,
            registry: ProviderRegistry::new(),
            workspace: None,
            subscription: None,
            active_task: None,
            submitted_prompt: None,
            previous_grid: None,
            quitting: false,
        }
    }

    /// Initialize the core boundary and run the terminal loop on Smol.
    pub fn run(&mut self) -> Result<(), AppError> {
        self.assemble_agent()?;
        let mut terminal = crate::terminal::TerminalGuard::enter()?;
        smol::block_on(self.event_loop(&mut terminal))
    }

    /// Run one explicit prompt without entering terminal mode, writing only streamed assistant
    /// text to stdout before exiting.
    pub fn run_prompt(&mut self, prompt: String) -> Result<(), AppError> {
        if prompt.trim().is_empty() {
            return Err(AppError::Setup("-p/--prompt must not be empty".into()));
        }
        if self.options.provider().is_none() || self.options.model().is_none() {
            return Err(AppError::Setup(
                "-p/--prompt requires --provider and --model".into(),
            ));
        }
        self.assemble_agent()?;
        let agent = self.agent_or_setup()?.clone();
        let subscription = agent.subscribe_lossless();
        let run = agent.start_prompt(prompt)?;
        smol::block_on(stream_prompt(run, subscription))
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

    /// Attach an explicitly configured agent for non-terminal integration tests.
    pub fn attach_agent(&mut self, agent: Agent) {
        self.state.set_snapshot(agent.snapshot());
        self.state.set_queue_snapshot(&agent);
        self.subscription = Some(agent.subscribe_lossless());
        self.core = Some(agent);
    }

    /// Borrow the attached core agent, if one exists.
    pub fn agent(&self) -> Option<&Agent> {
        self.core.as_ref()
    }

    /// Reload the explicit Tea registry into the current agent's future-run configuration.
    ///
    /// Core rejects this operation while a run is active. The previous configuration remains in
    /// place if discovery, policy loading, or the idle replacement fails.
    pub fn reload_tea_extensions(&mut self) -> Result<(), AppError> {
        self.reload_tea_extensions_inner(true)
    }

    fn reload_tea_extensions_after_settlement(&mut self) {
        // Tests and embedding integrations may attach an already-built agent without selecting
        // the TUI's Tea base configuration. Such an agent has no Tea snapshot to refresh.
        if self.tea_base_configuration.is_none() {
            return;
        }
        if let Err(error) = self.reload_tea_extensions_inner(false) {
            self.state.notice(format!(
                "Tea extensions were not reloaded; the previous snapshot remains active: {error}"
            ));
        }
    }

    fn reload_tea_extensions_inner(&mut self, announce: bool) -> Result<(), AppError> {
        let home = resolve_tea_home(self.options.tea_home())?;
        let extensions = load_tea_extensions(&home)?;
        let agent = self
            .core
            .as_ref()
            .ok_or_else(|| AppError::Setup("agent is not initialized".into()))?;
        let base = self
            .tea_base_configuration
            .as_ref()
            .ok_or_else(|| AppError::Setup("Tea base configuration is not initialized".into()))?;
        let configuration = compose_tea_configuration(base.clone(), &extensions, &home)?;
        agent.replace_configuration(configuration)?;
        self.tea_home = Some(home);
        self.tea_extensions = extensions;
        if announce {
            self.state.notice("Tea extensions reloaded");
        }
        Ok(())
    }

    pub(super) fn assemble_agent(&mut self) -> Result<(), AppError> {
        if self.core.is_some() {
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
        let builder = build_host_agent_with_thinking(tools, self.options.thinking_level())?;
        let compactor = Arc::new(ProviderCompactor::default());
        let compactor_capability: Arc<dyn tea_core::compaction::Compactor> = compactor.clone();
        let builder = builder.compactor(compactor_capability);
        self.compactor = Some(compactor);
        let home = resolve_tea_home(self.options.tea_home())?;
        let extensions = load_tea_extensions(&home)?;
        let agent = builder.build();
        let base = agent.configuration();
        let configuration = compose_tea_configuration(base.clone(), &extensions, &home)?;
        agent.replace_configuration(configuration)?;
        self.attach_agent(agent);
        self.tea_base_configuration = Some(base);
        self.tea_home = Some(home);
        self.tea_extensions = extensions;
        let workspace = self
            .workspace
            .as_ref()
            .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?;
        self.session_store = Some(
            SessionStore::new(
                self.tea_home
                    .as_ref()
                    .expect("Tea home was just initialized"),
            )
            .for_workspace(workspace),
        );
        self.current_session = Some(
            SessionRecord::new(None, self.options.thinking_level())
                .with_workspace(workspace.to_string_lossy()),
        );
        self.state.welcome_line();

        let explicit_provider = self.options.provider().map(OsStr::to_owned);
        let explicit_model = self.options.model().map(OsStr::to_owned);
        match (explicit_provider.as_deref(), explicit_model.as_deref()) {
            (None, None) => {
                self.restore_last_model(None);
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
                if let Err(error) = self.select_model(provider, model_id) {
                    self.state
                        .notice(format!("last model could not be restored: {error}"));
                }
                true
            }
            Ok(Some(_)) | Ok(None) => false,
            Err(error) => {
                self.state
                    .notice(format!("last model could not be read: {error}"));
                true
            }
        }
    }

    async fn event_loop(&mut self, terminal: &mut TerminalGuard) -> Result<(), AppError> {
        loop {
            self.drain_events();
            self.reap_task();
            if self.quitting && self.active_task.is_none() {
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
        let Some(subscription) = self.subscription.as_ref() else {
            return;
        };
        loop {
            match subscription.try_recv() {
                Ok(event) => self.state.apply_event(&event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if let Some(agent) = &self.core {
            self.state.set_snapshot(agent.snapshot());
            self.state.set_queue_snapshot(agent);
        }
    }

    pub(super) fn reap_task(&mut self) {
        let Some(receiver) = self.active_task.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.active_task = None;
                self.submitted_prompt = None;
                self.state.status = UiStatus::Idle;
                self.reload_tea_extensions_after_settlement();
                self.persist_session();
            }
            Ok(Err(CoreError::Cancelled)) => {
                self.active_task = None;
                self.restore_submitted_prompt("cancelled; prompt restored for explicit re-submit");
                self.reload_tea_extensions_after_settlement();
            }
            Ok(Err(error)) => {
                self.active_task = None;
                self.restore_submitted_prompt(format!(
                    "{error}; prompt restored for explicit re-submit"
                ));
                self.reload_tea_extensions_after_settlement();
            }
            Err(TryRecvError::Disconnected) => {
                self.active_task = None;
                self.state.notice("operation task ended unexpectedly");
            }
            Err(TryRecvError::Empty) => {}
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
        let (visible_lines, transcript_rows) =
            render::transcript_metrics(&self.state, width, height);
        self.state
            .set_viewport_metrics(visible_lines, transcript_rows);
        let current = render::render(&self.state, &self.registry, width, height);
        let diff = current.diff(self.previous_grid.as_ref());
        let cursor = composer_cursor(&self.state, width, height);
        if let Err(error) = terminal.draw(&diff, cursor) {
            // The terminal may have received part of a frame before a flush failed. The cell grid
            // retained by the app can no longer be trusted as the terminal's actual state, so
            // force the next successful draw to be a full repaint.
            self.previous_grid = None;
            return Err(error.into());
        }
        self.previous_grid = Some(current);
        Ok(())
    }

    pub(super) fn spawn_run(&mut self, run: RunHandle) {
        self.spawn_operation(async move { run.drive().await });
    }

    pub(super) fn spawn_compaction(&mut self, compaction: CompactionHandle) {
        self.spawn_operation(async move { compaction.drive().await });
    }

    fn spawn_operation<F>(&mut self, operation: F)
    where
        F: std::future::Future<Output = Result<(), CoreError>> + Send + 'static,
    {
        let (sender, receiver) = sync_channel(1);
        smol::spawn(async move {
            let _ = sender.send(operation.await);
        })
        .detach();
        self.active_task = Some(receiver);
        self.state.status = UiStatus::Active;
    }

    pub(super) fn agent_or_setup(&self) -> Result<&Agent, AppError> {
        self.core
            .as_ref()
            .ok_or_else(|| AppError::Setup("agent is not initialized".into()))
    }

    pub(super) fn agent_is_active(&self) -> bool {
        self.core
            .as_ref()
            .is_some_and(|agent| !matches!(agent.snapshot().phase, AgentPhase::Idle))
    }

    /// Persist the last fully settled canonical conversation. A failed save is a presentation
    /// notice only; it never changes core settlement or replaces the previous valid file.
    pub(super) fn persist_session(&mut self) -> bool {
        let (Some(store), Some(record), Some(agent)) = (
            self.session_store.clone(),
            self.current_session.as_mut(),
            self.core.as_ref(),
        ) else {
            return true;
        };
        let snapshot = agent.snapshot();
        if !matches!(snapshot.phase, AgentPhase::Idle) {
            return false;
        }
        // Match Pi's deferred-file behavior: an untouched/new session has no persisted file.
        if snapshot.messages.is_empty() {
            return true;
        }
        record.messages = snapshot.messages;
        record.model = snapshot.model;
        record.thinking_level = snapshot.thinking_level;
        if let Some(workspace) = self.workspace.as_ref() {
            record.cwd = workspace.to_string_lossy().into_owned();
        }
        if let Err(error) = store.save(record) {
            self.state.notice(format!("session was not saved: {error}"));
            return false;
        }
        true
    }
}

async fn stream_prompt(
    run: RunHandle,
    subscription: LosslessEventSubscription,
) -> Result<(), AppError> {
    let mut drive = Box::pin(run.drive());
    loop {
        drain_prompt_events(&subscription)?;
        if let Some(result) = smol::future::poll_once(&mut drive).await {
            drain_prompt_events(&subscription)?;
            result.map_err(AppError::from)?;
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(b"\n")
                .map_err(|error| AppError::Setup(format!("could not write response: {error}")))?;
            stdout
                .flush()
                .map_err(|error| AppError::Setup(format!("could not flush response: {error}")))?;
            return Ok(());
        }
        smol::future::yield_now().await;
    }
}

fn drain_prompt_events(subscription: &LosslessEventSubscription) -> Result<(), AppError> {
    let mut stdout = io::stdout().lock();
    let mut wrote = false;
    while let Ok(event) = subscription.try_recv() {
        if let AgentEventKind::MessageUpdate {
            text_delta: Some(text),
            ..
        } = event.kind
        {
            stdout
                .write_all(text.as_bytes())
                .map_err(|error| AppError::Setup(format!("could not write response: {error}")))?;
            wrote = true;
        }
    }
    if wrote {
        stdout
            .flush()
            .map_err(|error| AppError::Setup(format!("could not flush response: {error}")))?;
    }
    Ok(())
}

pub(super) fn os_text(value: &OsStr, flag: &str) -> Result<String, AppError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| AppError::Setup(format!("{flag} must be valid UTF-8")))
}
