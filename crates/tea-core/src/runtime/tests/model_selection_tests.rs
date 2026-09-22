use super::*;
use crate::harness::extension::NoExtensions;
use crate::scheduler::{CancellationToken, ModelFuture, ModelProvider, ModelRequest};
use crate::state::ModelDescriptor;
use crate::tool::ToolRegistry;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tea_session::{
    ArtifactError, ArtifactId, ArtifactStore, EntryId, EpochId, EpochStartedRecord,
    HarnessRevisionChangedEntry, LaneId, LaneMutation, LaneRecord, MemoryArtifactStore,
    MemorySession, ModelChangedEntry, OperationId, OperationKind, ProvisionedEntry,
    SessionEntry, SessionError, SessionHeader, SessionId, SessionReader, SessionWriter,
    StoredCommit,
};

#[derive(Default)]
struct ModelSelectionProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ModelProvider for ModelSelectionProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("model-selection provider request mutex")
            .push(request);
        Box::pin(std::future::ready(Ok(Box::new(completion_stream()) as _)))
    }
}

#[derive(Clone)]
struct CountingSession {
    inner: MemorySession,
    commits: Arc<AtomicUsize>,
}

impl CountingSession {
    fn new(session_id: &str) -> (Self, Arc<AtomicUsize>) {
        let commits = Arc::new(AtomicUsize::new(0));
        let session = MemorySession::create(SessionHeader::new(
            SessionId::new(session_id).expect("fixture session ID"),
            "model-selection-workspace",
            fixture_metadata(),
        ))
        .expect("fixture session creates");
        (
            Self {
                inner: session,
                commits: Arc::clone(&commits),
            },
            commits,
        )
    }
}

impl SessionReader for CountingSession {
    fn snapshot(&self) -> Result<tea_session::SessionSnapshot, SessionError> {
        self.inner.snapshot()
    }
}

impl SessionWriter for CountingSession {
    fn commit(&mut self, commit: tea_session::SessionCommit) -> Result<StoredCommit, SessionError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.inner.commit(commit)
    }
}

struct MissingCatalogStore {
    gets: AtomicUsize,
}

impl ArtifactStore for MissingCatalogStore {
    fn put(
        &self,
        _bytes: &[u8],
        _media_type: &str,
    ) -> Result<tea_session::ArtifactDescriptor, ArtifactError> {
        panic!("model selection must reject before persisting a catalog")
    }

    fn get(&self, artifact_id: ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        Err(ArtifactError::NotFound { artifact_id })
    }
}

fn descriptor(provider: &str, model: &str, revision: Option<&str>) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        revision: revision.map(str::to_owned),
    }
}

fn append_model_selection<S: SessionWriter>(
    session: &mut S,
    lane_id: &LaneId,
    entry_id: &str,
    selected: &ModelDescriptor,
) {
    session
        .append_entry(
            lane_id,
            ProvisionedEntry {
                id: EntryId::new(entry_id).expect("fixture model entry ID"),
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: selected.provider.clone(),
                    model: selected.model.clone(),
                    revision: selected.revision.clone(),
                }),
            },
        )
        .expect("fixture model selection commits");
}

#[test]
fn create_rejects_durable_model_selection_mismatch_before_side_effects() {
    let selected = descriptor("fixture-provider", "durable-model", Some("durable-r1"));
    let variants = [
        (
            "provider",
            Some(selected.clone()),
            Some(descriptor("other-provider", "durable-model", Some("durable-r1"))),
        ),
        (
            "model",
            Some(selected.clone()),
            Some(descriptor("fixture-provider", "other-model", Some("durable-r1"))),
        ),
        (
            "revision",
            Some(selected.clone()),
            Some(descriptor("fixture-provider", "durable-model", Some("other-r1"))),
        ),
        ("missing", Some(selected.clone()), None),
        ("unexpected", None, Some(selected.clone())),
    ];

    for (label, durable, supplied) in variants {
        let store = Arc::new(MemoryArtifactStore::default());
        let provider = Arc::new(ModelSelectionProvider::default());
        let (manager, identity, base_services) = fixture_manager(provider.clone(), store.clone());
        let (mut session, commits) = CountingSession::new(&format!("model-selection-{label}"));
        append_initial_revision(&mut session, &identity);
        if let Some(durable) = &durable {
            append_model_selection(
                &mut session,
                &LaneId::main(),
                &format!("model-selection-{label}-entry"),
                durable,
            );
        }
        let commits_before_create = commits.load(Ordering::SeqCst);
        let artifacts_before_create = store
            .inventory()
            .expect("memory artifact inventory reads")
            .len();
        let services = supplied
            .map(|model| base_services.clone().model(model))
            .unwrap_or(base_services);

        let result = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: services,
            artifacts: store.clone(),
            rollover_budget: 1,
            subagents: None,
        });
        let error = match result {
            Ok(_) => panic!("{label} runtime descriptor must not bind durable selection"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("model selection"),
            "{label} rejection identifies the durable model boundary: {error}"
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            commits_before_create,
            "{label} mismatch cannot append a harness catalog or any session mutation"
        );
        assert_eq!(
            store
                .inventory()
                .expect("memory artifact inventory reads")
                .len(),
            artifacts_before_create,
            "{label} mismatch cannot persist a harness catalog artifact"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "{label} mismatch cannot reach the provider"
        );
    }
}

#[test]
fn matching_durable_model_selection_is_accepted_and_reaches_that_provider_request() {
    smol::block_on(async {
        let selected = descriptor("fixture-provider", "durable-model", Some("durable-r1"));
        let store = Arc::new(MemoryArtifactStore::default());
        let provider = Arc::new(ModelSelectionProvider::default());
        let (manager, identity, base_services) = fixture_manager(provider.clone(), store.clone());
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("matching-model-selection").expect("fixture session ID"),
            "model-selection-workspace",
            fixture_metadata(),
        ))
        .expect("fixture session creates");
        append_initial_revision(&mut session, &identity);
        append_model_selection(
            &mut session,
            &LaneId::main(),
            "matching-model-selection-entry",
            &selected,
        );

        let supervisor = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: base_services.model(selected.clone()),
            artifacts: store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("matching model selection creates a supervisor");
        supervisor
            .run_root_prompt("exercise the selected model")
            .await
            .expect("matching model selection drives a root operation");

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let requests = provider
            .requests
            .lock()
            .expect("model-selection provider request mutex");
        assert_eq!(
            requests[0].model.as_ref(),
            Some(&selected),
            "the actual provider request carries the durable selected descriptor"
        );
    });
}

#[test]
fn reopen_rejects_mismatched_model_before_restoring_the_catalog() {
    let selected = descriptor("fixture-provider", "durable-model", Some("durable-r1"));
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = Arc::new(ModelSelectionProvider::default());
    let (manager, identity, base_services) = fixture_manager(provider.clone(), store.clone());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("reopen-model-selection").expect("fixture session ID"),
        "model-selection-workspace",
        fixture_metadata(),
    ))
    .expect("fixture session creates");
    append_initial_revision(&mut session, &identity);
    append_model_selection(
        &mut session,
        &LaneId::main(),
        "reopen-model-selection-entry",
        &selected,
    );
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity: identity,
        root_services: base_services.model(selected),
        artifacts: store,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("matching model selection persists catalog");
    let session = runtime
        .clone_session_for_test()
        .expect("persisted session clones for reopen");
    drop(runtime);

    let missing_catalog_store = Arc::new(MissingCatalogStore {
        gets: AtomicUsize::new(0),
    });
    let empty_repository = HarnessRepository::with_extension_engine(
        missing_catalog_store.clone(),
        Arc::new(NoExtensions),
    );
    let resolver = Arc::new(HarnessResolver::new(empty_repository, Default::default()));
    let mismatched_provider = Arc::new(ModelSelectionProvider::default());
    let error = match SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session,
        resolver,
        root_services: RuntimeServices::new(mismatched_provider.clone(), ToolRegistry::default())
            .model(descriptor("fixture-provider", "other-model", Some("durable-r1"))),
        lane_services: BTreeMap::new(),
        artifacts: missing_catalog_store.clone(),
        rollover_budget: 1,
        subagents: None,
    }) {
        Ok(_) => panic!("mismatched reopened service cannot bind durable selection"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("model selection"));
    assert_eq!(
        missing_catalog_store.gets.load(Ordering::SeqCst),
        0,
        "reopen rejects the service before loading or mutating catalog state"
    );
    assert_eq!(mismatched_provider.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn register_lane_rejects_mismatched_durable_model_before_installing_services() {
    let root_model = descriptor("fixture-provider", "root-model", Some("root-r1"));
    let lane_model = descriptor("fixture-provider", "lane-model", Some("lane-r1"));
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = Arc::new(ModelSelectionProvider::default());
    let (manager, identity, base_services) = fixture_manager(provider.clone(), store.clone());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("lane-model-selection").expect("fixture session ID"),
        "model-selection-workspace",
        fixture_metadata(),
    ))
    .expect("fixture session creates");
    append_initial_revision(&mut session, &identity);
    append_model_selection(
        &mut session,
        &LaneId::main(),
        "lane-model-selection-root",
        &root_model,
    );
    let lane_id = LaneId::new("model-selection-lane").expect("fixture lane ID");
    session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: lane_id.clone(),
            base_leaf_id: Some(
                EntryId::new("lane-model-selection-root").expect("fixture model entry ID"),
            ),
        })
        .expect("fixture lane creates");
    session
        .append_entry(
            &lane_id,
            ProvisionedEntry {
                id: EntryId::new("lane-model-selection-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: identity.revision_id().clone(),
                    snapshot_id: identity.snapshot_id().clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("fixture lane revision commits");
    append_model_selection(
        &mut session,
        &lane_id,
        "lane-model-selection-lane",
        &lane_model,
    );

    let supervisor = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity: identity,
        root_services: base_services.clone().model(root_model),
        artifacts: store,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("root model selection creates supervisor");
    let mismatch = base_services.clone().model(descriptor(
        "fixture-provider",
        "other-lane-model",
        Some("lane-r1"),
    ));
    let error = supervisor
        .register_lane(lane_id.clone(), mismatch)
        .expect_err("lane services must match durable lane model selection");
    assert!(error.to_string().contains("model selection"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    supervisor
        .register_lane(lane_id, base_services.model(lane_model))
        .expect("rejected lane was not installed before validation");
}

#[test]
fn resume_validates_model_selection_at_the_epoch_source_leaf() {
    smol::block_on(async {
        let historical = descriptor("fixture-provider", "historical-model", Some("historical-r1"));
        let current = descriptor("fixture-provider", "current-model", Some("current-r1"));
        let store = Arc::new(MemoryArtifactStore::default());
        let provider = Arc::new(ModelSelectionProvider::default());
        let (manager, identity, base_services) = fixture_manager(provider.clone(), store.clone());
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("historical-epoch-model-selection").expect("fixture session ID"),
            "model-selection-workspace",
            fixture_metadata(),
        ))
        .expect("fixture session creates");
        append_initial_revision(&mut session, &identity);
        append_model_selection(
            &mut session,
            &LaneId::main(),
            "historical-epoch-model",
            &historical,
        );
        let operation_id = OperationId::new("historical-epoch-operation")
            .expect("fixture operation ID");
        let input = ProvisionedEntry::user(
            EntryId::new("historical-epoch-input").expect("fixture input entry ID"),
            "resume the historical epoch",
        );
        session
            .append_record(LaneRecord::OperationStarted(tea_session::OperationStartedRecord::new(
                operation_id.clone(),
                LaneId::main(),
                Some(EntryId::new("historical-epoch-model").expect("fixture model entry ID")),
                OperationKind::Run,
                vec![input.clone()],
                identity.revision_id().clone(),
                identity.profile_id().clone(),
            )))
            .expect("fixture operation accepts");
        session
            .append_entry(&LaneId::main(), input)
            .expect("fixture input commits");
        let epoch_id = EpochId::new("historical-epoch").expect("fixture epoch ID");
        session
            .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                id: epoch_id,
                operation_id,
                epoch_index: 0,
                source_leaf_id: Some(
                    EntryId::new("historical-epoch-input").expect("fixture input entry ID"),
                ),
                harness_revision_id: identity.revision_id().clone(),
                harness_snapshot_id: identity.snapshot_id().clone(),
                model_harness_profile: identity.profile_id().clone(),
                core_run_id: tea_session::CoreRunId::new("historical-epoch-core-run")
                    .expect("fixture core run ID"),
                epoch_resume_data: BTreeMap::new(),
            }))
            .expect("fixture epoch starts");
        append_model_selection(
            &mut session,
            &LaneId::main(),
            "current-model-after-epoch-start",
            &current,
        );

        let supervisor = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: base_services.model(current),
            artifacts: store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("current root selection binds at construction");
        let sequence_before_resume = supervisor
            .snapshot()
            .expect("fixture snapshot reads")
            .last_sequence();
        let error = supervisor
            .resume()
            .await
            .expect_err("historical epoch cannot use current swapped descriptor");

        assert!(error.to_string().contains("model selection"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            supervisor
                .snapshot()
                .expect("fixture snapshot reads")
                .last_sequence(),
            sequence_before_resume,
            "historical model mismatch rejects before a resumed provider request or session mutation"
        );
    });
}
