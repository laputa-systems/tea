use crate::render;
use crate::terminal::{TerminalError, TerminalGuard};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::time::Duration;
use tea_core::agent::AgentConfiguration;
use tea_core::coding::CodingHost;
use tea_core::compaction::AutomaticCompactionPolicy;
use tea_core::harness::HarnessError;
use tea_core::runtime::{
    HarnessEvent, IdleAuthorization, IdleDriveOutcome, SessionEvent, TeaEvent,
    TeaEventSubscription, TeaEventTryRecvError, TeaObservationSnapshot,
};
use tea_providers::ProviderRegistry;
use tea_session::{EntryId, LaneId, TurnCheckpointId};
use tea_tui::Size;

use super::compaction::ProviderCompactor;
use super::config::{load_tui_config, TuiConfig};
use super::error::AppError;
use super::host::host_configuration;
use super::mock;
use super::nonblocking_operations::NonblockingCodingOperations;
use super::provider_factory::ProviderFactory;
use super::state::{AppState, ToolState, TranscriptEntry, UiStatus};
use crate::cli::CliOptions;
use std::sync::Arc;
use tea_core::state::ThinkingLevel;

/// Footer notice once a user-cancelled root operation has fully settled.
pub(super) const CANCELLED_TURN_SETTLED_NOTICE: &str = "turn cancelled; input kept in the session";

pub(super) enum RootTaskOutcome {
    Drive(IdleDriveOutcome),
    Recovery,
}

enum RootTaskCompletion {
    Settled(Result<RootTaskOutcome, HarnessError>),
    Disconnected,
}

pub(super) struct OwnedRootTask {
    receiver: Receiver<Result<RootTaskOutcome, HarnessError>>,
    task: smol::Task<()>,
    completion: Option<RootTaskCompletion>,
}

impl OwnedRootTask {
    fn new(
        receiver: Receiver<Result<RootTaskOutcome, HarnessError>>,
        task: smol::Task<()>,
    ) -> Self {
        Self {
            receiver,
            task,
            completion: None,
        }
    }

    #[cfg(test)]
    pub(super) fn completed_for_test(
        receiver: Receiver<Result<RootTaskOutcome, HarnessError>>,
    ) -> Self {
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
    /// Number of front-contiguous semantic entries already written once into
    /// native terminal scrollback for this presentation generation.
    pub(super) committed_entries: usize,
    /// Durable identities corresponding to committed canonical rows. Local
    /// terminal-only rows are intentionally excluded, allowing an atomic
    /// snapshot to retain the EntryId-identical scrollback frontier.
    pub(super) committed_entry_ids: Vec<EntryId>,
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
            committed_entries: 0,
            committed_entry_ids: Vec::new(),
            rendered_projection_generation: 0,
            quitting: false,
        }
    }

    /// Initialize the durable host configuration and run the terminal loop on Smol.
    pub fn run(&mut self) -> Result<(), AppError> {
        self.assemble_host()?;
        let loop_result = {
            let mut terminal = crate::terminal::TerminalGuard::enter()?;
            let loop_result = smol::block_on(self.event_loop(&mut terminal));
            if self.quitting && loop_result.is_ok() {
                // A quit requested from a modal surface must still finish on
                // the main-screen status frame; the alternate surface is
                // temporary and is not meaningful shell scrollback.
                self.state.close_surface();
                let shutdown_result = self.redraw(&mut terminal).and_then(|()| {
                    terminal
                        .renderer_mut()
                        .map_err(AppError::from)?
                        .commit_live()
                        .map_err(TerminalError::Io)
                        .map_err(AppError::from)
                });
                match shutdown_result {
                    Ok(()) => loop_result,
                    Err(error) => Err(error),
                }
            } else {
                loop_result
            }
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
        self.ensure_execution_authority()?;
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
        let coding_host =
            CodingHost::with_operations(&workspace, Arc::new(NonblockingCodingOperations))
                .map_err(|error| AppError::Setup(format!("invalid --cwd: {error}")))?;
        self.workspace = Some(coding_host.workspace().as_path().to_path_buf());
        let configuration = if self.options.provider() == Some(OsStr::new(mock::PROVIDER_ID)) {
            mock::configuration()
        } else {
            host_configuration(&workspace.to_string_lossy())?
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
            (Some(_), None) => self.state.notice("select a model with /models"),
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
            let tea_home = self
                .tea_home
                .as_ref()
                .cloned()
                .ok_or_else(|| AppError::Setup("Tea home is not initialized".into()))?;
            let local_base_url = self
                .options
                .local_base_url()
                .map(|value| os_text(value, "--local-base-url"))
                .transpose()?;
            self.provider_factory = Some(Arc::new(ProviderFactory::new(
                self.registry,
                local_base_url,
                self.options.local_context_window(),
                tea_home,
            )));
        }
        self.provider_factory
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| AppError::Setup("provider factory could not initialize".into()))
    }

    /// Verify that the currently selected descriptor still has executable
    /// terminal authority before durable prompt admission.
    ///
    /// Session reopen deliberately installs descriptor-pinned lazy services so
    /// inspection and restoration do not require credentials. A normal prompt
    /// must nevertheless fail before its text becomes an accepted input when
    /// the host cannot currently construct that descriptor's adapter.
    pub(super) fn ensure_execution_authority(&mut self) -> Result<(), AppError> {
        let descriptor = self
            .state
            .selected_model
            .clone()
            .ok_or_else(|| AppError::Setup("select a model first".into()))?;
        self.provider_factory()?.configured(&descriptor)?;
        Ok(())
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
        Ok(Some(super::durable::HostSubagentConfig::terminal(
            self.provider_factory()?,
            config.subagents,
        )))
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

    pub(super) fn drain_events(&mut self) {
        loop {
            let event = match self.durable_subscription.as_ref() {
                Some(subscription) => subscription.try_recv(),
                None => break,
            };
            match event {
                Ok(event) => self.project_durable_event(event),
                Err(TeaEventTryRecvError::Empty) => break,
                Err(TeaEventTryRecvError::Lagged) => {
                    self.resubscribe_durable_projection(
                        "live updates lagged; refreshed durable state",
                    );
                    break;
                }
                Err(TeaEventTryRecvError::Disconnected) => {
                    self.resubscribe_durable_projection(
                        "live updates disconnected; refreshed durable state",
                    );
                    break;
                }
            }
        }
    }

    /// Project one durable event into root-facing UI state. Child event text,
    /// tools, and notices never become root transcript rows; only accounting
    /// is safe to aggregate before an explicit `wait_agent` report.
    pub(super) fn project_durable_event(&mut self, event: TeaEvent) {
        let terminal_previews = event.terminal_preview_identities();
        let completed_run = event.completed_observation_run().cloned();
        match event {
            TeaEvent::Agent { run, event } if run.lane_id == tea_session::LaneId::main() => {
                self.state.apply_observed_event(&run, &event);
            }
            TeaEvent::Agent { event, .. } => self.state.apply_background_usage_event(&event),
            TeaEvent::Preview(preview)
                if preview.identity().run.lane_id == tea_session::LaneId::main() =>
            {
                self.state.apply_preview(&preview);
            }
            TeaEvent::Preview(_) => {}
            TeaEvent::Session(
                SessionEvent::InputQueueChanged { lane_id, .. }
                | SessionEvent::OperationAccepted { lane_id, .. },
            ) => {
                // Durable queue membership and input dispatch are projected
                // directly from the session snapshot. Observation delivery is
                // lossy, so this event never supplies a user transcript row
                // or input identity itself; it only prompts the refresh.
                if lane_id == tea_session::LaneId::main() {
                    self.project_dispatched_user_messages();
                    if let Err(error) = self.refresh_runtime_input_projection() {
                        self.state.notice(error.to_string());
                    }
                }
                self.refresh_subagent_footer();
            }
            TeaEvent::Session(_) => {
                self.bind_current_durable_projection();
                if let Err(error) = self.refresh_runtime_input_projection() {
                    self.state.notice(error.to_string());
                }
                self.refresh_subagent_footer();
            }
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
        self.state.fence_previews(terminal_previews);
        if let Some(run) = completed_run {
            if run.lane_id == tea_session::LaneId::main() {
                self.state.clear_previews_for_run(&run);
            }
        }
    }

    /// Replace a terminal observation subscription after its bounded queue
    /// reports lag or disconnects. This is a projection-only repair: it never
    /// starts, resumes, aborts, or otherwise advances durable work.
    fn resubscribe_durable_projection(&mut self, notice: &str) {
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            return;
        };
        let subscription = match harness.subscribe_events() {
            Ok(subscription) => subscription,
            Err(error) => {
                self.state
                    .notice(format!("{notice}; could not refresh: {error}"));
                return;
            }
        };
        match self.restore_observation_snapshot(&subscription.snapshot) {
            Ok(()) => {
                self.durable_subscription = Some(subscription);
                if let Err(error) = self.refresh_runtime_input_projection() {
                    self.state.notice(format!(
                        "{notice}; could not refresh queued inputs: {error}"
                    ));
                    return;
                }
                self.state.notice(notice);
            }
            Err(error) => self
                .state
                .notice(format!("{notice}; could not rebuild projection: {error}")),
        }
    }

    /// Rebuild terminal-visible durable rows from the single session prefix
    /// captured with an event subscription, then layer only bounded root
    /// previews from that same frontier on top.
    fn restore_observation_snapshot(
        &mut self,
        snapshot: &TeaObservationSnapshot,
    ) -> Result<(), AppError> {
        let messages = super::durable::project_host_messages(&snapshot.session)?;
        let has_durable_projection =
            self.state
                .transcript()
                .iter()
                .enumerate()
                .any(|(index, entry)| {
                    self.state.transcript_entry_id(index).is_some()
                        || requires_durable_entry_id(entry)
                });
        let replaced = !messages.is_empty() || has_durable_projection;
        self.state.clear_previews();
        if replaced {
            self.state.restore_durable_messages(&messages);
        }
        for preview in &snapshot.view.previews {
            if preview.identity().run.lane_id == tea_session::LaneId::main() {
                self.state.apply_preview(preview);
            }
        }
        let reduction =
            tea_session::reduce_lane(snapshot.session.clone(), tea_session::LaneId::main())
                .map_err(|error| AppError::Setup(error.to_string()))?;
        self.state
            .set_session_id(Some(snapshot.session.header().session_id.to_string()));
        self.state
            .set_reported_usage(super::durable::core_usage(&reduction.usage_totals));
        if replaced {
            self.reconcile_committed_frontier();
        }
        Ok(())
    }

    /// Attach entry identities to the currently rendered durable prefix
    /// without replacing it. This is intentionally best-effort presentation
    /// work; the atomic subscription snapshot remains the resync authority.
    fn bind_current_durable_projection(&mut self) {
        let Some(harness) = self.durable_harness.as_ref() else {
            return;
        };
        let Ok(snapshot) = harness.snapshot() else {
            return;
        };
        let Ok(messages) = super::durable::project_host_messages(&snapshot) else {
            return;
        };
        self.state.bind_durable_messages(&messages);
    }

    /// Bind the rendered durable prefix, then append user rows for inputs the
    /// runtime has dispatched since. Accepted-but-queued inputs stay in the
    /// next-message slot until their dispatch commits a user entry.
    fn project_dispatched_user_messages(&mut self) {
        let Some(harness) = self.durable_harness.as_ref() else {
            return;
        };
        let Ok(snapshot) = harness.snapshot() else {
            return;
        };
        let Ok(messages) = super::durable::project_host_messages(&snapshot) else {
            return;
        };
        self.state.bind_durable_messages(&messages);
        self.state.append_unprojected_user_messages(&messages);
    }

    /// Keep only the longest exact durable identity prefix already emitted to
    /// native scrollback. A session branch replacement therefore appends only
    /// its changed suffix after a resnapshot instead of duplicating stale text.
    pub(super) fn reconcile_committed_frontier(&mut self) {
        let mut matching = 0;
        for index in 0..self.state.transcript().len() {
            let Some(entry_id) = self.state.transcript_entry_id(index) else {
                break;
            };
            if self.committed_entry_ids.get(matching) != Some(entry_id) {
                break;
            }
            matching = matching.saturating_add(1);
        }
        self.committed_entry_ids.truncate(matching);
        self.committed_entries = matching;
        self.rendered_projection_generation = self.state.projection_generation();
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
                RootTaskCompletion::Settled(Ok(outcome)) => {
                    self.state.status = UiStatus::Idle;
                    if let Err(error) = self.refresh_runtime_input_projection() {
                        self.state.notice(error.to_string());
                    }
                    let should_continue = matches!(
                        outcome,
                        RootTaskOutcome::Recovery
                            | RootTaskOutcome::Drive(
                                IdleDriveOutcome::Inputs { .. }
                                    | IdleDriveOutcome::ExtensionContinuation { .. }
                            )
                    );
                    if should_continue && !self.quitting {
                        self.start_runtime_idle_drive();
                    }
                    self.refresh_subagent_footer();
                }
                RootTaskCompletion::Settled(Err(error)) => {
                    self.state.status = UiStatus::Idle;
                    if let Err(refresh_error) = self.refresh_runtime_input_projection() {
                        self.state.notice(refresh_error.to_string());
                    }
                    if matches!(&error, HarnessError::RecoveryRequired { .. }) {
                        self.state.notice(
                            "durable recovery requires /continue; accepted inputs remain queued",
                        );
                    } else if matches!(
                        &error,
                        HarnessError::Core(tea_core::error::CoreError::Cancelled)
                    ) {
                        // The dispatched input is already committed history;
                        // cancellation settles the turn without restoring a
                        // local draft for re-submission.
                        self.state.notice(CANCELLED_TURN_SETTLED_NOTICE);
                    } else {
                        self.state.notice(error.to_string());
                    }
                    self.refresh_subagent_footer();
                }
                RootTaskCompletion::Disconnected => {
                    self.state.status = UiStatus::Idle;
                    self.state
                        .notice("durable operation task ended unexpectedly");
                }
            }
        }
    }

    /// Replace the terminal's combined next-message slot from durable queue
    /// state. A failed query leaves the prior projection untouched so an
    /// observer hiccup cannot erase visible accepted input.
    pub(super) fn refresh_runtime_input_projection(&mut self) -> Result<(), AppError> {
        let Some(harness) = self.durable_harness.as_ref() else {
            self.state.clear_queued_inputs();
            return Ok(());
        };
        let inputs = harness
            .queued_inputs()?
            .into_iter()
            .map(|input| (input.id().clone(), input.content().to_owned()))
            .collect();
        self.state.set_queued_inputs(inputs);
        Ok(())
    }

    /// Atomically return every input represented by the combined slot to an
    /// empty local composer. The projection changes only after the runtime
    /// commits the all-or-nothing withdrawal.
    pub(super) fn withdraw_projected_inputs(&mut self) -> Result<bool, AppError> {
        if !self.state.composer().text().is_empty() {
            return Ok(false);
        }
        let input_ids = self.state.queued_input_ids().to_vec();
        if input_ids.is_empty() {
            return Ok(false);
        }
        let Some(harness) = self.durable_harness.as_ref() else {
            return Ok(false);
        };
        let withdrawn = harness.withdraw_inputs(&input_ids)?;
        let restored = withdrawn
            .inputs()
            .iter()
            .map(|input| input.content())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.state.clear_queued_inputs();
        self.state.composer_mut().replace_from_editor(restored);
        self.refresh_runtime_input_projection()?;
        Ok(true)
    }

    /// Ask the runtime to make one explicit idle decision. It owns control
    /// precedence, accepted input batching, and authorized goal continuation;
    /// the terminal only owns task polling and presentation.
    pub(super) fn start_runtime_idle_drive(&mut self) {
        if self.durable_task.is_some() || self.agent_is_active() {
            return;
        }
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            return;
        };
        if let Err(error) = self.ensure_execution_authority() {
            self.state.notice(error.to_string());
            return;
        }
        self.spawn_durable_input_drive(harness, IdleAuthorization::AllowExtensionContinuation);
    }

    fn redraw(&mut self, terminal: &mut TerminalGuard) -> Result<(), AppError> {
        let (width, height) = terminal.size()?;
        let size = Size { width, height };
        if self.rendered_projection_generation != self.state.projection_generation() {
            self.committed_entries = 0;
            self.committed_entry_ids.clear();
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

        let stable = self.committable_stable_prefix();
        if stable > self.committed_entries {
            let lines = render::committed_lines(&self.state, self.committed_entries, stable, width);
            terminal
                .renderer_mut()?
                .commit(&lines)
                .map_err(TerminalError::Io)?;
            for index in self.committed_entries..stable {
                if let Some(entry_id) = self.state.transcript_entry_id(index) {
                    self.committed_entry_ids.push(entry_id.clone());
                }
            }
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

    /// Do not permanently write a canonical row until its durable `EntryId`
    /// is available. Local welcome and notice rows remain safe to commit
    /// without an identity; every other stable semantic row waits for the
    /// next durable binding or atomic snapshot.
    fn committable_stable_prefix(&self) -> usize {
        let stable = render::stable_prefix(self.state.transcript());
        for index in self.committed_entries..stable {
            if self.state.transcript_entry_id(index).is_none()
                && requires_durable_entry_id(&self.state.transcript()[index])
            {
                return index;
            }
        }
        stable
    }

    /// Poll one runtime-owned idle decision through the terminal's structured
    /// task boundary. The runtime, not this host, decides whether controls,
    /// accepted inputs, or an explicitly authorized extension continuation
    /// run next.
    pub(super) fn spawn_durable_input_drive(
        &mut self,
        harness: Arc<super::durable::HostHarness>,
        authorization: IdleAuthorization,
    ) {
        let mut drive = Box::pin(async move {
            harness
                .drive_next_input(authorization)
                .await
                .map(RootTaskOutcome::Drive)
        });
        // Deciding that nothing is eligible is synchronous: the runtime
        // reduces its durable queue, applies settled controls, and evaluates
        // bounded idle hooks before any await. Poll once so an immediately
        // idle answer never holds a task, which would otherwise turn the
        // user's next Ctrl-C into an abort of work that does not exist.
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let ready = match std::future::Future::poll(drive.as_mut(), &mut context) {
            std::task::Poll::Ready(Ok(RootTaskOutcome::Drive(IdleDriveOutcome::Idle))) => {
                self.state.status = UiStatus::Idle;
                return;
            }
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        let (sender, receiver) = sync_channel(1);
        let task = smol::spawn(async move {
            let result = match ready {
                Some(result) => result,
                None => drive.await,
            };
            let _ = sender.send(result);
        });
        self.durable_task = Some(OwnedRootTask::new(receiver, task));
        self.state.status = UiStatus::Active;
    }

    /// Explicitly continue the root recovery plan reported by a previously
    /// opened durable session. Opening itself remains read-only and never
    /// calls this path implicitly.
    pub(super) fn continue_recovery(&mut self) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state
                .notice("continuation requires an idle durable harness");
            return Ok(());
        }
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            self.state
                .notice("open a durable session before continuing recovery");
            return Ok(());
        };
        let report = harness.recovery_report()?;
        if !report
            .lanes
            .iter()
            .any(|lane| lane.lane_id == tea_session::LaneId::main())
        {
            self.state
                .notice("the root session has no interrupted operation");
            return Ok(());
        }
        if let Err(error) = self.ensure_execution_authority() {
            self.state.notice(error.to_string());
            return Ok(());
        }
        self.spawn_durable_recovery(harness);
        self.state.notice("continuing durable recovery");
        Ok(())
    }

    /// Fork a completed root turn from its recorded checkpoint without
    /// starting work in the new lane. The supervisor validates the historical
    /// harness boundary and installs fresh inert lane services atomically.
    pub(super) fn fork_settled_turn(&mut self, arguments: &str) -> Result<(), AppError> {
        if self.agent_is_active() {
            self.state
                .notice("forking requires an idle durable harness");
            return Ok(());
        }
        self.refresh_runtime_input_projection()?;
        if !self.state.queued_input_ids().is_empty() {
            self.state
                .notice("withdraw queued inputs before creating a fork");
            return Ok(());
        }
        let mut words = arguments.split_whitespace();
        let Some(checkpoint_text) = words.next() else {
            self.state.notice("usage: /fork <checkpoint-id> [lane-id]");
            return Ok(());
        };
        let lane_text = words.next();
        if words.next().is_some() {
            self.state.notice("usage: /fork <checkpoint-id> [lane-id]");
            return Ok(());
        }
        let checkpoint_id = match TurnCheckpointId::new(checkpoint_text.to_owned()) {
            Ok(checkpoint_id) => checkpoint_id,
            Err(error) => {
                self.state
                    .notice(format!("invalid settled-turn checkpoint ID: {error}"));
                return Ok(());
            }
        };
        let Some(harness) = self.durable_harness.as_ref().cloned() else {
            self.state
                .notice("open a durable session before creating a fork");
            return Ok(());
        };
        let lane_id = match lane_text {
            Some(lane_text) => match LaneId::new(lane_text.to_owned()) {
                Ok(lane_id) => lane_id,
                Err(error) => {
                    self.state.notice(format!("invalid fork lane ID: {error}"));
                    return Ok(());
                }
            },
            None => {
                let snapshot = harness.snapshot()?;
                match LaneId::new(format!("fork-{}", snapshot.next_sequence().0)) {
                    Ok(lane_id) => lane_id,
                    Err(error) => {
                        return Err(AppError::Setup(format!(
                            "could not derive a fresh fork lane ID: {error}"
                        )));
                    }
                }
            }
        };
        match harness.fork_settled_turn(checkpoint_id, lane_id) {
            Ok(fork) => self.state.notice(format!(
                "forked checkpoint {} into lane {}",
                fork.checkpoint_id(),
                fork.lane_id()
            )),
            Err(error) => self.state.notice(error.to_string()),
        }
        Ok(())
    }

    /// Drive the one root recovery plan after explicit user authorization.
    fn spawn_durable_recovery(&mut self, harness: Arc<super::durable::HostHarness>) {
        let (sender, receiver) = sync_channel(1);
        let task = smol::spawn(async move {
            let _ = sender.send(harness.resume().await.map(|_| RootTaskOutcome::Recovery));
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
        let mock_coding_operations = model.provider == mock::PROVIDER_ID;
        let config = super::durable::HostHarnessConfig {
            tea_home: &home,
            workspace: &workspace,
            configuration,
            model,
            provider,
            thinking_level: Some(self.options.thinking_level()),
            compactor: self.compactor.clone(),
            automatic_compaction,
            subagents,
        };
        let harness = if mock_coding_operations {
            super::durable::create_mock_host_harness(config)?
        } else {
            super::durable::create_host_harness(config)?
        };
        let subscription = harness.subscribe_events()?;
        self.restore_observation_snapshot(&subscription.snapshot)?;
        self.durable_subscription = Some(subscription);
        self.state
            .set_extension_commands(harness.extension_host_commands()?);
        self.durable_harness = Some(Arc::clone(&harness));
        self.refresh_runtime_input_projection()?;
        self.refresh_subagent_footer();
        Ok(harness)
    }

    /// Replace the idle terminal's current durable writer with an existing
    /// session selected from the explicit workspace-scoped picker. Reopen is
    /// read-only restoration: recovery remains paused until `/continue` is an
    /// explicit user action.
    pub(super) fn reopen_durable_session(&mut self, id: &str) -> Result<(), AppError> {
        if self.agent_is_active() {
            return Err(AppError::Setup(
                "session changes require an idle durable harness".into(),
            ));
        }
        self.refresh_runtime_input_projection()?;
        if !self.state.queued_input_ids().is_empty() {
            return Err(AppError::Setup(
                "withdraw queued inputs before changing sessions".into(),
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
        let mock_coding_operations = model.provider == mock::PROVIDER_ID;
        let input = super::durable::HostHarnessReopen {
            tea_home: &home,
            workspace: &workspace,
            session_id: id,
            configuration,
            model,
            provider,
            compactor: self.compactor.clone(),
            automatic_compaction,
            subagents,
        };
        let harness = if mock_coding_operations {
            super::durable::reopen_mock_host_harness(input)?
        } else {
            super::durable::reopen_host_harness(input)?
        };
        self.state.set_thinking_level(harness.thinking_level()?);
        let subscription = harness.subscribe_events()?;
        self.restore_observation_snapshot(&subscription.snapshot)?;
        self.durable_subscription = Some(subscription);
        self.state
            .set_extension_commands(harness.extension_host_commands()?);
        let recovery = harness.recovery_report()?;
        self.durable_harness = Some(Arc::clone(&harness));
        self.refresh_runtime_input_projection()?;
        self.refresh_subagent_footer();
        self.state.close_surface();
        self.state.status = UiStatus::Idle;
        if recovery.lanes.is_empty() {
            self.state.notice(format!("opened durable session {id}"));
        } else {
            self.state.notice(format!(
                "opened durable session {id}; {} recovery lane(s) require /continue",
                recovery.lanes.len()
            ));
        }
        Ok(())
    }
}

/// Only durable semantic rows need an identity before native scrollback
/// commits them. Local terminal affordances intentionally remain outside the
/// session branch and can be rendered immediately.
fn requires_durable_entry_id(entry: &TranscriptEntry) -> bool {
    match entry {
        TranscriptEntry::Welcome { .. } | TranscriptEntry::Notice { .. } => false,
        TranscriptEntry::User { .. } | TranscriptEntry::Error { .. } => true,
        TranscriptEntry::Assistant { streaming, .. } => !streaming,
        TranscriptEntry::Tool(tool) => {
            matches!(tool.state, ToolState::Completed | ToolState::Failed)
        }
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
pub(super) fn resolve_tea_home(override_path: Option<&Path>) -> Result<PathBuf, AppError> {
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
