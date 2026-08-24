use crate::render;
use crate::terminal::{TerminalError, TerminalGuard};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::time::Duration;
use tea_core::agent::AgentConfiguration;
use tea_core::coding::TeaCodingToolsV2;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::error::CoreError;
use tea_core::harness::HarnessError;
use tea_core::runtime::{HarnessEvent, SessionEvent, TeaEvent, TeaEventSubscription};
use tea_providers::ProviderRegistry;
use tea_tui::Size;

use super::cli::CliOptions;
use super::compaction::ProviderCompactor;
use super::config::{load_tui_config, TuiConfig};
use super::error::AppError;
use super::host::host_configuration;
use super::mock;
use super::nonblocking_operations::NonblockingCodingOperations;
use super::provider_factory::ProviderFactory;
use super::state::{AppState, UiStatus};
use std::sync::Arc;
use tea_core::state::ThinkingLevel;

enum RootTaskCompletion {
    Settled(Result<(), HarnessError>),
    Disconnected,
}

pub(super) struct OwnedRootTask {
    receiver: Receiver<Result<(), HarnessError>>,
    task: smol::Task<()>,
    completion: Option<RootTaskCompletion>,
}

impl OwnedRootTask {
    fn new(receiver: Receiver<Result<(), HarnessError>>, task: smol::Task<()>) -> Self {
        Self {
            receiver,
            task,
            completion: None,
        }
    }

    #[cfg(test)]
    pub(super) fn completed_for_test(receiver: Receiver<Result<(), HarnessError>>) -> Self {
        let task = smol::spawn(async {});
        smol::block_on(async {
            while !task.is_finished() {
                smol::future::yield_now().await;
            }
        });
        Self::new(receiver, task)
    }
}

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
    pub(super) durable_task: Option<OwnedRootTask>,
    pub(super) tea_home: Option<PathBuf>,
    /// Global terminal policy loaded once from the resolved Tea home. It is
    /// intentionally absent from library and `tea session` command paths.
    pub(super) tui_config: Option<TuiConfig>,
    pub(super) registry: ProviderRegistry,
    /// Lazy host-owned adapter construction for root and future child lanes.
    pub(super) provider_factory: Option<Arc<ProviderFactory>>,
    pub(super) workspace: Option<PathBuf>,
    /// The idle prompt handed to the current run, retained only to restore local input after a
    /// failed or cancelled operation. The durable session remains the transcript source of truth.
    pub(super) submitted_prompt: Option<String>,
    /// Accepted extension controls held until the current durable operation
    /// settles. They are host-local presentation requests, never fake user
    /// messages or mutations of an in-flight provider request.
    pub(super) queued_extension_commands: Vec<(String, String)>,
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
            tui_config: None,
            registry: ProviderRegistry::new(),
            provider_factory: None,
            workspace: None,
            submitted_prompt: None,
            queued_extension_commands: Vec::new(),
            committed_entries: 0,
            rendered_projection_generation: 0,
            quitting: false,
        }
    }

    /// Initialize the durable host configuration and run the terminal loop on Smol.
    pub fn run(&mut self) -> Result<(), AppError> {
        self.assemble_host()?;
        let loop_result = {
            let mut terminal = crate::terminal::TerminalGuard::enter()?;
            smol::block_on(self.event_loop(&mut terminal))
        };
        // Terminal I/O failure is presentation failure, not permission to
        // abandon the durable root future. Restore the terminal first, then
        // request cancellation and drive the owned task through settlement.
        let settlement = smol::block_on(self.settle_owned_root());
        match (loop_result, settlement) {
            (result, Ok(())) => result,
            (Ok(()), Err(error)) => Err(error),
            (Err(loop_error), Err(cleanup_error)) => Err(AppError::Setup(format!(
                "{loop_error}; durable root cleanup requires recovery: {cleanup_error}"
            ))),
        }
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
        let home = match self.tea_home.as_ref() {
            Some(home) => home.clone(),
            None => {
                let home = resolve_tea_home(self.options.tea_home())?;
                self.tea_home = Some(home.clone());
                home
            }
        };
        if self.tui_config.is_none() {
            self.tui_config = Some(load_tui_config(&home)?);
        }
        let subagent_footer = self.tui_config.as_ref().and_then(|config| {
            config
                .features
                .subagents
                .then_some((0, config.subagents.max_concurrent.get()))
        });
        self.state.set_subagent_activity(subagent_footer);
        let workspace = match self.options.cwd() {
            Some(path) => path.to_path_buf(),
            None => std::env::current_dir().map_err(|error| {
                AppError::Setup(format!("cannot read current directory: {error}"))
            })?,
        };
        let tools =
            TeaCodingToolsV2::with_operations(&workspace, Arc::new(NonblockingCodingOperations))
                .map_err(|error| AppError::Setup(format!("invalid --cwd: {error}")))?;
        self.workspace = Some(tools.workspace().as_path().to_path_buf());
        let configuration = if self.options.provider() == Some(OsStr::new(mock::PROVIDER_ID)) {
            mock::configuration()
        } else {
            host_configuration(tools, &workspace.to_string_lossy())?
        };
        self.configuration = Some(configuration);
        self.state
            .set_extension_commands(super::durable::bundled_host_commands()?);
        self.state.set_thinking_level(self.options.thinking_level());
        self.state.welcome_line();

        let explicit_provider = self.options.provider().map(OsStr::to_owned);
        let explicit_model = self.options.model().map(OsStr::to_owned);
        match (explicit_provider.as_deref(), explicit_model.as_deref()) {
            (None, None) => {}
            (Some(provider), None) if provider == OsStr::new(mock::PROVIDER_ID) => {
                self.select_model(mock::PROVIDER_ID.into(), mock::DEFAULT_MODEL_ID.into())?
            }
            (Some(_), None) => self.state.notice("select a model with /model"),
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

    /// Construct root/provider authority only when a descriptor actually needs it.
    ///
    /// This keeps feature-disabled idle startup free of any child-provider factory,
    /// credential load, or adapter construction. The same factory later supplies
    /// exact provider configuration for root and explicitly enabled child lanes.
    pub(super) fn provider_factory(&mut self) -> Result<Arc<ProviderFactory>, AppError> {
        if self.provider_factory.is_none() {
            let workspace = self
                .workspace
                .as_ref()
                .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?;
            let local_base_url = self
                .options
                .local_base_url()
                .map(|value| os_text(value, "--local-base-url"))
                .transpose()?;
            self.provider_factory = Some(Arc::new(ProviderFactory::new(
                self.registry,
                local_base_url,
                self.options.local_context_window(),
                workspace.to_string_lossy().into_owned(),
            )));
        }
        self.provider_factory
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| AppError::Setup("provider factory could not initialize".into()))
    }

    /// Return terminal-local child authority only for an explicitly enabled
    /// application mode. The factory itself remains lazy: credentials and
    /// model adapters are not touched here.
    pub(super) fn subagent_host_config(
        &mut self,
    ) -> Result<Option<super::durable::HostSubagentConfig>, AppError> {
        let config = self
            .tui_config
            .as_ref()
            .ok_or_else(|| AppError::Setup("TUI config is not initialized".into()))?
            .clone();
        if !config.features.subagents {
            return Ok(None);
        }
        Ok(Some(super::durable::HostSubagentConfig {
            factory: self.provider_factory()?,
            config: config.subagents,
        }))
    }

    async fn event_loop(&mut self, terminal: &mut TerminalGuard) -> Result<(), AppError> {
        loop {
            self.drain_events();
            self.reap_task();
            // The detached root receiver is the terminal's structured join
            // boundary: core settles active children and their workspaces
            // before it sends this completion. Keep retrying the sticky root
            // abort through the narrow schedule gap before an epoch installs
            // its core agent; never exit early and orphan that cleanup.
            if self.quitting && self.durable_task.is_some() {
                self.request_root_abort(false);
            }
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

    async fn settle_owned_root(&mut self) -> Result<(), AppError> {
        self.quitting = true;
        while self.durable_task.is_some() {
            self.request_root_abort(false);
            self.drain_events();
            self.reap_task();
            if self.durable_task.is_some() {
                smol::future::yield_now().await;
            }
        }
        if let Some(harness) = self.durable_harness.as_ref() {
            super::durable::require_root_settled(harness)?;
        }
        Ok(())
    }

    fn drain_events(&mut self) {
        loop {
            let event = match self.durable_subscription.as_ref() {
                Some(subscription) => subscription.try_recv(),
                None => break,
            };
            match event {
                Ok(event) => self.project_durable_event(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    /// Project one durable event into root-facing UI state. Child event text,
    /// tools, and notices never become root transcript rows; only accounting
    /// is safe to aggregate before an explicit `wait_agent` report.
    pub(super) fn project_durable_event(&mut self, event: TeaEvent) {
        match event {
            TeaEvent::Agent { lane_id, event } if lane_id == tea_session::LaneId::main() => {
                self.state.apply_event(&event);
            }
            TeaEvent::Agent { event, .. } => self.state.apply_background_usage_event(&event),
            TeaEvent::Session(SessionEvent::OperationAccepted { lane_id, .. }) => {
                // The session writer appended the root user entry before this
                // event. Child acceptance must not duplicate the root draft in
                // local history.
                if lane_id == tea_session::LaneId::main() {
                    if let Some(prompt) = self.submitted_prompt.as_deref() {
                        self.state.record_history(prompt);
                    }
                }
                self.refresh_subagent_footer();
            }
            TeaEvent::Session(_) => self.refresh_subagent_footer(),
            TeaEvent::Harness(HarnessEvent::CandidateRejected {
                stage,
                code,
                diagnostic,
                ..
            }) => self.state.notice(format!(
                "harness candidate rejected at {stage:?} ({}) : {diagnostic}",
                code.as_str()
            )),
            TeaEvent::Harness(_) | TeaEvent::Artifact(_) => {}
        }
    }

    /// Refresh enabled-only child activity and all-lane accounting from the
    /// same durable snapshot. This makes the footer reconnect-safe while
    /// keeping child transcripts out of the native scrollback projection.
    fn refresh_subagent_footer(&mut self) {
        let Some(harness) = self.durable_harness.as_ref() else {
            return;
        };
        let Ok(snapshot) = harness.snapshot() else {
            return;
        };
        let Ok(graph) = tea_session::reduce_agent_graph(&snapshot) else {
            return;
        };
        let Some(policy) = graph.policy else {
            self.state.set_subagent_activity(None);
            return;
        };
        let active = graph
            .agents
            .values()
            .filter(|agent| {
                matches!(
                    agent.state,
                    tea_session::AgentState::Spawned
                        | tea_session::AgentState::Running
                        | tea_session::AgentState::Finalizing { .. }
                )
            })
            .count() as u32;
        let mut lane_ids = BTreeSet::from([tea_session::LaneId::main()]);
        lane_ids.extend(
            graph
                .agents
                .values()
                .map(|agent| agent.spawned.lane_id.clone()),
        );
        let mut aggregate = tea_session::Usage::default();
        for lane_id in lane_ids {
            let Ok(reduction) = tea_session::reduce_lane(snapshot.clone(), lane_id) else {
                return;
            };
            aggregate.saturating_add_assign(&reduction.usage_totals);
        }
        self.state
            .set_reported_usage(super::durable::core_usage(&aggregate));
        self.state
            .set_subagent_activity(Some((active, policy.max_concurrent)));
    }

    pub(super) fn reap_task(&mut self) {
        let completion = if let Some(owned) = self.durable_task.as_mut() {
            if owned.completion.is_none() {
                owned.completion = match owned.receiver.try_recv() {
                    Ok(result) => Some(RootTaskCompletion::Settled(result)),
                    Err(TryRecvError::Disconnected) => Some(RootTaskCompletion::Disconnected),
                    Err(TryRecvError::Empty) => None,
                };
            }
            owned
                .task
                .is_finished()
                .then(|| owned.completion.take())
                .flatten()
        } else {
            None
        };
        if let Some(completion) = completion {
            let _joined = self
                .durable_task
                .take()
                .expect("completed root task remains owned through reaping");
            match completion {
                RootTaskCompletion::Settled(Ok(())) => {
                    self.submitted_prompt = None;
                    self.state.status = UiStatus::Idle;
                    if self.quitting {
                        self.queued_extension_commands.clear();
                        self.refresh_subagent_footer();
                        return;
                    }
                    let queued_continuation = self.apply_queued_extension_commands();
                    if self.state.queued_message().is_some() {
                        self.start_queued_prompt();
                    } else if let Some((harness, extension_id, input)) = queued_continuation {
                        self.spawn_extension_continuation(harness, extension_id, input);
                    } else {
                        self.start_idle_extension_continuation();
                    }
                    self.refresh_subagent_footer();
                }
                RootTaskCompletion::Settled(Err(HarnessError::Core(CoreError::Cancelled))) => {
                    self.restore_submitted_prompt(
                        "cancelled; prompt restored for explicit re-submit",
                    );
                }
                RootTaskCompletion::Settled(Err(error)) => {
                    self.restore_submitted_prompt(format!(
                        "{error}; prompt restored for explicit re-submit"
                    ));
                }
                RootTaskCompletion::Disconnected => {
                    self.state
                        .notice("durable operation task ended unexpectedly");
                }
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

    /// Apply extension controls accepted while an operation was active before
    /// deciding whether an idle hook may continue it. A queued user prompt
    /// still takes priority over a returned internal continuation.
    fn apply_queued_extension_commands(
        &mut self,
    ) -> Option<(Arc<super::durable::HostHarness>, String, String)> {
        let commands = std::mem::take(&mut self.queued_extension_commands);
        if commands.is_empty() {
            return None;
        }
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            self.state
                .notice("discarded queued extension controls without a durable harness");
            return None;
        };
        let mut continuation = None;
        for (command, arguments) in commands {
            match harness.dispatch_extension_command(&command, arguments) {
                Ok(dispatch) => {
                    if let Some(notice) = dispatch.result.notice {
                        self.state.notice(notice);
                    }
                    if let Some(input) = dispatch.result.internal_input {
                        if continuation.is_some() {
                            self.state.notice(
                                "only one queued extension continuation may start after a run",
                            );
                        } else {
                            continuation = Some((dispatch.extension_id, input));
                        }
                    }
                }
                Err(error) => self.state.notice(error.to_string()),
            }
        }
        continuation.map(|(extension_id, input)| (harness, extension_id, input))
    }

    /// Ask resolved extensions whether the just-settled durable operation
    /// warrants one host-only continuation. Queued user input is handled
    /// first by `reap_task`, so this cannot leapfrog an explicit user action.
    fn start_idle_extension_continuation(&mut self) {
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            return;
        };
        match harness.evaluate_idle_extensions() {
            Ok(Some(continuation)) => {
                self.spawn_extension_continuation(
                    harness,
                    continuation.extension_id,
                    continuation.input,
                );
            }
            Ok(None) => {}
            Err(error) => self.state.notice(error.to_string()),
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
        let task = smol::spawn(async move {
            let _ = sender.send(harness.run_root_prompt(input).await.map(|_| ()));
        });
        self.durable_task = Some(OwnedRootTask::new(receiver, task));
        self.state.status = UiStatus::Active;
    }

    /// Drive the one recovery plan derived from an opened durable session.
    pub(super) fn spawn_durable_recovery(&mut self, harness: Arc<super::durable::HostHarness>) {
        let (sender, receiver) = sync_channel(1);
        let task = smol::spawn(async move {
            let _ = sender.send(harness.resume().await.map(|_| ()));
        });
        self.durable_task = Some(OwnedRootTask::new(receiver, task));
        self.state.status = UiStatus::Active;
    }

    /// Start a continuation through the same session-owned operation path as
    /// ordinary work. The `input` is retained as host-only context, not as a
    /// user message in the transcript.
    pub(super) fn spawn_extension_continuation(
        &mut self,
        harness: Arc<super::durable::HostHarness>,
        extension_id: String,
        input: String,
    ) {
        if self.durable_task.is_some() || harness.is_active() {
            self.state
                .notice("extension continuation requires an idle durable harness");
            return;
        }
        let (sender, receiver) = sync_channel(1);
        let task = smol::spawn(async move {
            let _ = sender.send(
                harness
                    .run_extension_continuation(extension_id, input)
                    .await
                    .map(|_| ()),
            );
        });
        self.durable_task = Some(OwnedRootTask::new(receiver, task));
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
            .ok_or_else(|| AppError::Setup("workspace is not initialized".into()))?
            .clone();
        let home = self
            .tea_home
            .as_ref()
            .ok_or_else(|| AppError::Setup("Tea home is not initialized".into()))?
            .clone();
        let automatic_compaction = self.automatic_compaction.clone();
        let subagents = self.subagent_host_config()?;
        let harness = super::durable::create_host_harness(super::durable::HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model,
            provider,
            thinking_level: Some(self.options.thinking_level()),
            compactor: self.compactor.clone(),
            automatic_compaction,
            subagents,
        })?;
        self.durable_subscription = Some(harness.subscribe_events()?);
        self.state
            .set_extension_commands(harness.extension_host_commands()?);
        self.durable_harness = Some(Arc::clone(&harness));
        self.refresh_subagent_footer();
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
        let subagents = self.subagent_host_config()?;

        // Drop the prior idle writer before opening another session. This is
        // also what lets a user select the currently displayed session again
        // without fighting its own advisory writer lock.
        self.durable_subscription = None;
        self.durable_harness = None;
        let harness = super::durable::reopen_host_harness(super::durable::HostHarnessReopen {
            tea_home: &home,
            workspace: &workspace,
            session_id: id,
            configuration,
            model,
            provider,
            compactor: self.compactor.clone(),
            automatic_compaction,
            subagents,
        })?;
        self.state.set_thinking_level(harness.thinking_level()?);
        let snapshot = harness.snapshot()?;
        let messages = super::durable::project_host_messages(&snapshot)?;
        self.state.restore_messages(&messages);
        self.durable_subscription = Some(harness.subscribe_events()?);
        let reduction = tea_session::reduce_lane(snapshot, tea_session::LaneId::main())
            .map_err(|error| AppError::Setup(error.to_string()))?;
        self.state.reported_usage = super::durable::core_usage(&reduction.usage_totals);
        self.state
            .set_extension_commands(harness.extension_host_commands()?);
        let recovery = reduction.lane_state.active_operation.is_some();
        self.durable_harness = Some(Arc::clone(&harness));
        self.refresh_subagent_footer();
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
