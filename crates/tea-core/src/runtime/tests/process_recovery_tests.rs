use super::*;
use crate::error::HookError;
use crate::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookFuture, HookSet, NoHooks};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CHILD_CASE: &str = "TEA_PROCESS_RECOVERY_CHILD_CASE";
const CHILD_SESSION_DIRECTORY: &str = "TEA_PROCESS_RECOVERY_SESSION_DIRECTORY";
const CHILD_COUNTER_PATH: &str = "TEA_PROCESS_RECOVERY_COUNTER_PATH";
const READY_PREFIX: &str = "tea-process-recovery-ready:";
const FINAL_HOOK_IDENTITY: &str = "tea-process-recovery-final-hook-v1";

static NEXT_DIRECTORY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashPoint {
    UnsafeToolIntent,
    ProviderFinal,
}

impl CrashPoint {
    const fn name(self) -> &'static str {
        match self {
            Self::UnsafeToolIntent => "unsafe-tool-intent",
            Self::ProviderFinal => "provider-final",
        }
    }

    const fn test_name(self) -> &'static str {
        match self {
            Self::UnsafeToolIntent => {
                "runtime::tests::process_recovery_tests::sigkill_after_durable_unsafe_tool_intent_requires_reconciliation"
            }
            Self::ProviderFinal => {
                "runtime::tests::process_recovery_tests::sigkill_after_committed_provider_final_settles_without_replay"
            }
        }
    }

    fn from_name(value: &str) -> Self {
        match value {
            "unsafe-tool-intent" => Self::UnsafeToolIntent,
            "provider-final" => Self::ProviderFinal,
            _ => panic!("unknown process recovery crash point {value:?}"),
        }
    }
}

struct ProcessFixture {
    root: PathBuf,
    session_directory: PathBuf,
    counter_path: PathBuf,
}

impl ProcessFixture {
    fn create(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "tea-core-process-recovery-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after Unix epoch")
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        fs::create_dir_all(&root).expect("process recovery fixture directory creates");
        Self {
            session_directory: root.join("session"),
            counter_path: root.join("provider-calls"),
            root,
        }
    }
}

impl Drop for ProcessFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy)]
enum ProviderScript {
    UnsafeTool,
    FinalResponse,
}

struct PersistedCounterProvider {
    counter_path: PathBuf,
    script: ProviderScript,
}

impl ModelProvider for PersistedCounterProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        record_provider_call(&self.counter_path);
        let events = match self.script {
            ProviderScript::UnsafeTool => vec![
                ModelStreamEvent::ToolCall(AgentToolCall {
                    id: ToolCallId::new("process-recovery-unsafe-call")
                        .expect("fixture tool call ID"),
                    name: "record".into(),
                    arguments: SerializedJson::new("{}"),
                }),
                ModelStreamEvent::End(StopReason::ToolUse),
            ],
            ProviderScript::FinalResponse => vec![
                ModelStreamEvent::TextDelta("durably committed final response".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        };
        Box::pin(std::future::ready(
            Ok(Box::new(ModelStream { events }) as _),
        ))
    }
}

struct BlockingUnsafeTool;

impl AgentTool for BlockingUnsafeTool {
    fn name(&self) -> &str {
        "record"
    }

    fn description(&self) -> &str {
        "records one durable tool intent"
    }

    fn schema(&self) -> &JsonValue {
        RecordingTool.schema()
    }

    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        announce_ready(CrashPoint::UnsafeToolIntent);
        Box::pin(std::future::pending())
    }
}

struct BlockAfterProviderFinal;

impl HookSet for BlockAfterProviderFinal {
    fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        NoHooks.before_tool_call(call)
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        NoHooks.after_tool_call(call, result)
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        NoHooks.transform_context(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        NoHooks.convert_to_llm(context)
    }

    fn should_stop_after_turn_async<'a>(
        &'a self,
        _context: &'a ContextEnvelope,
        _cancellation: CancellationToken,
    ) -> HookFuture<'a, bool> {
        announce_ready(CrashPoint::ProviderFinal);
        Box::pin(std::future::pending())
    }
}

#[cfg(unix)]
#[test]
fn sigkill_after_durable_unsafe_tool_intent_requires_reconciliation() {
    if let Some(point) = child_crash_point() {
        assert_eq!(point, CrashPoint::UnsafeToolIntent);
        drive_child(point);
        return;
    }

    let fixture = ProcessFixture::create("unsafe-tool");
    let mut child = spawn_child(CrashPoint::UnsafeToolIntent, &fixture);
    wait_for_ready(&mut child, CrashPoint::UnsafeToolIntent);
    assert_eq!(provider_calls(&fixture.counter_path), 1);
    kill_child(&mut child);

    let snapshot = read_snapshot(&fixture.session_directory);
    assert!(snapshot.records().iter().any(|stored| {
        matches!(
            &stored.record,
            LaneRecord::ToolStarted(record)
                if record.tool_call_id == "process-recovery-unsafe-call"
                    && record.replay_policy_at_start == ToolReplayPolicy::Never
        )
    }));
    assert!(snapshot.entries().iter().all(|entry| {
        !matches!(
            &entry.body,
            SessionEntry::ToolResult(result)
                if result.tool_call_id == "process-recovery-unsafe-call"
        )
    }));

    let reopened = reopen_runtime(&fixture, CrashPoint::UnsafeToolIntent);
    assert_eq!(
        provider_calls(&fixture.counter_path),
        1,
        "reopen must not infer"
    );
    let before = reopened.snapshot().expect("reopened prefix snapshots");
    assert!(matches!(
        smol::block_on(reopened.resume()),
        Err(HarnessError::RecoveryRequired { .. })
    ));
    assert_eq!(
        provider_calls(&fixture.counter_path),
        1,
        "blocked continue must not infer"
    );
    assert_eq!(
        reopened.snapshot().expect("blocked prefix snapshots"),
        before,
        "blocked continue must not mutate the unsafe durable prefix"
    );
}

#[cfg(unix)]
#[test]
fn sigkill_after_committed_provider_final_settles_without_replay() {
    if let Some(point) = child_crash_point() {
        assert_eq!(point, CrashPoint::ProviderFinal);
        drive_child(point);
        return;
    }

    let fixture = ProcessFixture::create("provider-final");
    let mut child = spawn_child(CrashPoint::ProviderFinal, &fixture);
    wait_for_ready(&mut child, CrashPoint::ProviderFinal);
    assert_eq!(provider_calls(&fixture.counter_path), 1);
    kill_child(&mut child);

    let snapshot = read_snapshot(&fixture.session_directory);
    assert!(snapshot.records().iter().any(|stored| {
        matches!(
            &stored.record,
            LaneRecord::ProviderRequestSettled(record)
                if record.classification == tea_session::ProviderSettlementClassification::Completed
        )
    }));
    assert!(snapshot.entries().iter().any(|entry| {
        matches!(
            &entry.body,
            SessionEntry::AssistantMessage(message)
                if message.content == "durably committed final response" && message.stop_reason.as_deref() == Some("stop")
        )
    }));
    assert!(
        snapshot
            .records()
            .iter()
            .all(|stored| { !matches!(&stored.record, LaneRecord::OperationFinished(_)) })
    );

    let reopened = reopen_runtime(&fixture, CrashPoint::ProviderFinal);
    assert_eq!(
        provider_calls(&fixture.counter_path),
        1,
        "reopen must not infer"
    );
    assert!(
        smol::block_on(reopened.resume())
            .expect("committed final answer settles on explicit continuation")
            .is_completed()
    );
    assert_eq!(
        provider_calls(&fixture.counter_path),
        1,
        "settling a committed final answer must not issue a second provider request"
    );
    assert!(reopened.snapshot().expect("settled prefix snapshots").records().iter().any(|stored| {
        matches!(
            &stored.record,
            LaneRecord::OperationFinished(record) if record.outcome == OperationOutcome::Completed
        )
    }));
}

fn child_crash_point() -> Option<CrashPoint> {
    std::env::var(CHILD_CASE)
        .ok()
        .as_deref()
        .map(CrashPoint::from_name)
}

fn spawn_child(point: CrashPoint, fixture: &ProcessFixture) -> std::process::Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", point.test_name(), "--nocapture"])
        .env(CHILD_CASE, point.name())
        .env(CHILD_SESSION_DIRECTORY, &fixture.session_directory)
        .env(CHILD_COUNTER_PATH, &fixture.counter_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("crash fixture child starts")
}

fn wait_for_ready(child: &mut std::process::Child, point: CrashPoint) {
    let stdout = child
        .stdout
        .take()
        .expect("crash fixture child pipes stdout");
    let mut output = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        let read = output
            .read_line(&mut line)
            .expect("crash fixture child readiness reads");
        assert_ne!(read, 0, "crash fixture child exited before becoming ready");
        if line.contains(&format!("{READY_PREFIX}{}", point.name())) {
            return;
        }
    }
}

fn kill_child(child: &mut std::process::Child) {
    child.kill().expect("SIGKILL reaches crash fixture child");
    let status = child.wait().expect("crash fixture child is reaped");
    assert!(
        !status.success(),
        "crash fixture child must not exit cleanly"
    );
}

fn drive_child(point: CrashPoint) {
    let session_directory = PathBuf::from(
        std::env::var(CHILD_SESSION_DIRECTORY).expect("child session directory is configured"),
    );
    let counter_path = PathBuf::from(
        std::env::var(CHILD_COUNTER_PATH).expect("child provider counter is configured"),
    );
    let session = JsonlSession::create(
        &session_directory,
        SessionHeader::new(
            SessionId::new(format!("process-recovery-{}", point.name()))
                .expect("fixture session ID"),
            "synthetic-process-recovery-workspace",
            fixture_metadata(),
        ),
        DurabilityMode::Strict,
    )
    .expect("child JSONL session creates");
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(
        session
            .artifact_store()
            .expect("child JSONL artifact store opens"),
    );
    let (resolver, identity) = child_harness(point, &counter_path, Arc::clone(&artifacts));
    let services = process_services(point, &counter_path, true);
    let mut session = session;
    append_initial_revision(&mut session, &identity);
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver,
        root_identity: identity,
        root_services: services,
        artifacts,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("child supervisor creates");
    let _ = smol::block_on(runtime.run_root_prompt("crash after one durable boundary"));
    panic!("crash fixture child passed its durable crash boundary");
}

fn reopen_runtime(
    fixture: &ProcessFixture,
    point: CrashPoint,
) -> Arc<SessionSupervisor<JsonlSession>> {
    let session = JsonlSession::open(&fixture.session_directory, DurabilityMode::Strict)
        .expect("crashed child JSONL session reopens");
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(
        session
            .artifact_store()
            .expect("reopened JSONL artifact store opens"),
    );
    let repository =
        HarnessRepository::with_extension_engine(Arc::clone(&artifacts), Arc::new(NoExtensions));
    let resolver = Arc::new(HarnessResolver::new(repository, Default::default()));
    let root_services = process_services(point, &fixture.counter_path, false);
    SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session,
        resolver,
        root_services,
        lane_services: BTreeMap::new(),
        artifacts,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("supervisor reconstructs the durable catalog")
}

fn child_harness(
    point: CrashPoint,
    counter_path: &Path,
    artifacts: Arc<dyn ArtifactStore>,
) -> (Arc<HarnessResolver>, HarnessIdentity) {
    let services = process_services(point, counter_path, true);
    let mut repository =
        HarnessRepository::with_extension_engine(artifacts, Arc::new(NoExtensions));
    let snapshot = repository
        .stage_snapshot(snapshot_spec(services.runtime_policy_identities()))
        .expect("process fixture snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("process fixture revision stages");
    let identity = HarnessIdentity::new(
        revision.revision_id,
        snapshot.id,
        snapshot.spec.model_harness_profile,
    );
    (
        Arc::new(HarnessResolver::new(repository, Default::default())),
        identity,
    )
}

fn process_services(
    point: CrashPoint,
    counter_path: &Path,
    block_after_provider_final: bool,
) -> RuntimeServices {
    let provider = Arc::new(PersistedCounterProvider {
        counter_path: counter_path.to_path_buf(),
        script: match point {
            CrashPoint::UnsafeToolIntent => ProviderScript::UnsafeTool,
            CrashPoint::ProviderFinal => ProviderScript::FinalResponse,
        },
    });
    let mut tools = ToolRegistry::default();
    match point {
        CrashPoint::UnsafeToolIntent => tools.insert(Arc::new(BlockingUnsafeTool)),
        CrashPoint::ProviderFinal => tools.insert(Arc::new(RecordingTool)),
    };
    
    match point {
        CrashPoint::UnsafeToolIntent => RuntimeServices::new(provider, tools),
        CrashPoint::ProviderFinal => {
            let hooks: Arc<dyn HookSet> = if block_after_provider_final {
                Arc::new(BlockAfterProviderFinal)
            } else {
                Arc::new(NoHooks)
            };
            RuntimeServices::new(provider, tools)
                .hooks_with_identity(hooks, Digest::from_bytes(FINAL_HOOK_IDENTITY))
        }
    }
}

fn read_snapshot(directory: &Path) -> tea_session::SessionSnapshot {
    let session = JsonlSession::open(directory, DurabilityMode::Strict)
        .expect("crashed child JSONL session opens for inspection");
    session.snapshot().expect("crashed child prefix snapshots")
}

fn announce_ready(point: CrashPoint) {
    println!("{READY_PREFIX}{}", point.name());
    std::io::stdout()
        .flush()
        .expect("crash fixture readiness flushes");
}

fn record_provider_call(path: &Path) {
    let count = provider_calls(path).saturating_add(1);
    fs::write(path, count.to_string()).expect("process fixture provider count persists");
}

fn provider_calls(path: &Path) -> usize {
    match fs::read_to_string(path) {
        Ok(value) => value
            .trim()
            .parse()
            .expect("process fixture provider count is an integer"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("process fixture provider count reads: {error}"),
    }
}
