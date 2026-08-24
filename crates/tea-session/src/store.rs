use crate::reduction::{reduce_lane_ref, reduce_lane_ref_with_append};
use crate::{
    Corruption, EntryHeader, EntryId, EpochId, LaneId, LaneMutation, LaneRecord, OperationId,
    OperationKind, ProviderRequestId, ProvisionedEntry, SESSION_FORMAT_VERSION, Sequence,
    SessionEntry, SessionFact, SessionHeader, SessionMutation, SessionSnapshot, StepId, StepKind,
    StoredEntry, StoredFact, StoredLaneMutation, StoredMutation, StoredRecord,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Explicit time source used for durable commit timestamps.
///
/// The clock is supplied by the host so fixture and replay tests can use a
/// deterministic value without altering the session wire contract.
pub trait SessionClock: fmt::Debug + Send + Sync {
    /// Return milliseconds since the Unix epoch for one accepted commit.
    fn now_ms(&self) -> u64;
}

/// Production clock used by the convenience session constructors.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSessionClock;

impl SessionClock for SystemSessionClock {
    fn now_ms(&self) -> u64 {
        system_time_ms()
    }
}

/// Failures at the durable-session boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionError {
    /// The session header names a discarded on-disk format. Tea deliberately
    /// does not inspect, import, or repair records from unsupported formats.
    UnsupportedFormat {
        /// Session file whose header was inspected.
        path: String,
        /// Header version when it was safely readable.
        observed_version: Option<u64>,
    },
    /// Caller-provided content violates the durable session contract.
    InvalidInput { message: String },
    /// An existing log prefix cannot be reduced safely.
    Corruption(Corruption),
    /// The store has faulted after an incomplete or failed write and must be reopened.
    Faulted { message: String },
    /// The log ends in an uncommitted unterminated tail that requires an
    /// explicit repair operation before a writer can open it.
    RecoveryRequired { path: String, offset: u64 },
    /// An I/O operation failed at a named path.
    Io { path: String, message: String },
    /// An append-stage I/O operation failed after the durable prefix may have
    /// changed. The writer is faulted and must be reopened before another
    /// mutation or dependent effect can proceed.
    IndeterminateWrite { path: String, message: String },
    /// Another active writer owns the session.
    WriterBusy { path: String },
    /// A persisted format line is invalid.
    Format {
        path: String,
        line: usize,
        /// Offset of the first byte in the offending line.
        offset: u64,
        /// Decoded envelope sequence when the failing line reached that stage.
        sequence: Option<Sequence>,
        /// Decoded top-level mutation kind when the failing line reached that stage.
        mutation_kind: Option<String>,
        message: String,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat {
                path,
                observed_version,
            } => match observed_version {
                Some(version) => write!(
                    formatter,
                    "unsupported session format at {path}: observed version {version}; current build supports only session format 1; no automatic migration is available"
                ),
                None => write!(
                    formatter,
                    "unsupported session format at {path}: current build supports only session format 1; no automatic migration is available"
                ),
            },
            Self::InvalidInput { message } => write!(formatter, "invalid session input: {message}"),
            Self::Corruption(error) => write!(formatter, "session corruption: {error}"),
            Self::Faulted { message } => write!(formatter, "session store is faulted: {message}"),
            Self::RecoveryRequired { path, offset } => write!(
                formatter,
                "session recovery is required at {path}: uncommitted tail begins at byte {offset}; run the explicit torn-tail repair operation"
            ),
            Self::Io { path, message } => {
                write!(formatter, "session I/O failed at {path}: {message}")
            }
            Self::IndeterminateWrite { path, message } => write!(
                formatter,
                "session append outcome is indeterminate at {path}: {message}; close and reopen before continuing"
            ),
            Self::WriterBusy { path } => {
                write!(formatter, "session already has an active writer: {path}")
            }
            Self::Format {
                path,
                line,
                offset,
                sequence,
                mutation_kind,
                message,
            } => {
                write!(
                    formatter,
                    "invalid session format at {path} line {line} at byte {offset}"
                )?;
                if let Some(sequence) = sequence {
                    write!(formatter, " sequence {}", sequence.0)?;
                }
                if let Some(mutation_kind) = mutation_kind {
                    write!(formatter, " mutation {mutation_kind}")?;
                }
                write!(formatter, ": {message}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

impl From<Corruption> for SessionError {
    fn from(value: Corruption) -> Self {
        Self::Corruption(value)
    }
}

/// Read-only atomic snapshot access to a durable session.
pub trait SessionReader {
    /// Clone one coherent durable snapshot. Live events are intentionally not
    /// replayed through this interface.
    fn snapshot(&self) -> Result<SessionSnapshot, SessionError>;
}

/// The narrow mutation interface that allocates sequences and timestamps
/// inside each successful commit.
pub trait SessionWriter: SessionReader {
    /// Append an entry to the current leaf of the named lane.
    fn append_entry(
        &mut self,
        lane_id: &LaneId,
        entry: ProvisionedEntry,
    ) -> Result<StoredEntry, SessionError>;

    /// Append one immutable operation record.
    fn append_record(&mut self, record: LaneRecord) -> Result<StoredRecord, SessionError>;

    /// Append a lane topology fact.
    fn append_lane_mutation(
        &mut self,
        mutation: LaneMutation,
    ) -> Result<StoredLaneMutation, SessionError>;

    /// Append a session-wide non-semantic fact.
    fn append_fact(&mut self, fact: SessionFact) -> Result<StoredFact, SessionError>;
}

/// In-memory reference implementation used for storage-conformance fixtures.
///
/// It uses precisely the same append/reduce validation contract as the JSONL
/// implementation. A host can therefore run backend-neutral recovery prefixes
/// without a filesystem or provider.
#[derive(Clone, Debug)]
pub struct MemorySession {
    snapshot: SessionSnapshot,
    append_index: SessionAppendIndex,
    fault: Option<String>,
    clock: Arc<dyn SessionClock>,
}

/// Disposable indexes used only to prepare the next append against an already
/// validated snapshot.
///
/// The index has two deliberately narrow jobs: it derives entry parents and
/// validates the self-contained lifecycle records that account for ordinary
/// operation throughput. Records outside that subset still take the pure
/// full-snapshot reducer path before they commit. This cache is never
/// authoritative for reopening or recovery; the reducer remains the durable
/// session contract.
#[derive(Clone, Debug)]
pub(crate) struct SessionAppendIndex {
    entry_ids: BTreeSet<EntryId>,
    lane_leaves: BTreeMap<LaneId, Option<EntryId>>,
    provisioned_entries: BTreeMap<EntryId, ProvisionedEntry>,
    operations: BTreeMap<OperationId, IndexedOperation>,
    active_operations: BTreeMap<LaneId, OperationId>,
    epochs: BTreeMap<EpochId, OperationId>,
    step_attempts: BTreeMap<(OperationId, EpochId, StepKind), (StepId, u32)>,
    provider_starts: BTreeMap<ProviderRequestId, OperationId>,
    provider_settled: BTreeSet<ProviderRequestId>,
    agent_lanes: BTreeSet<LaneId>,
    incremental_validation_ready: bool,
}

#[derive(Clone, Debug)]
struct IndexedOperation {
    lane_id: LaneId,
    finished: bool,
    epoch_ids: Vec<EpochId>,
    open_epochs: BTreeSet<EpochId>,
}

impl SessionAppendIndex {
    pub(crate) fn empty(header: &SessionHeader) -> Self {
        Self {
            entry_ids: BTreeSet::new(),
            lane_leaves: BTreeMap::from([(header.initial_lane.clone(), None)]),
            provisioned_entries: BTreeMap::new(),
            operations: BTreeMap::new(),
            active_operations: BTreeMap::new(),
            epochs: BTreeMap::new(),
            step_attempts: BTreeMap::new(),
            provider_starts: BTreeMap::new(),
            provider_settled: BTreeSet::new(),
            agent_lanes: BTreeSet::new(),
            incremental_validation_ready: true,
        }
    }

    pub(crate) fn contains_entry(&self, entry_id: &EntryId) -> bool {
        self.entry_ids.contains(entry_id)
    }

    pub(crate) fn lane_leaf(&self, lane_id: &LaneId) -> Result<Option<EntryId>, Corruption> {
        self.lane_leaves
            .get(lane_id)
            .cloned()
            .ok_or_else(|| Corruption::new(format!("unknown lane {lane_id}")))
    }

    /// Validate whether an append can skip full reducer replay.
    ///
    /// Callers invoke this before a durable write and call [`Self::advance`]
    /// only after that write commits. A failed validation is a normal append
    /// rejection, not an index mutation.
    pub(crate) fn is_locally_validated_mutation(
        &self,
        mutation: &StoredMutation,
    ) -> Result<bool, Corruption> {
        if !self.incremental_validation_ready {
            return Ok(false);
        }
        match &mutation.mutation {
            SessionMutation::Entry(entry)
                if matches!(&entry.body, SessionEntry::UserMessage(_)) =>
            {
                if let Some(provisioned) = self.provisioned_entries.get(&entry.header.id)
                    && provisioned.body != entry.body
                {
                    return Err(Corruption::new(format!(
                        "provisioned entry {} materialized with different content",
                        entry.header.id
                    )));
                }
                Ok(true)
            }
            // Every operation on an agent-bound lane carries graph-only
            // binding checks (the durable spawn, operation kind, harness/profile,
            // and assignment identity), so it must pass through the whole-prefix
            // graph reducer before it is committed. A claimed subagent operation
            // also takes that path so an unbound or wrong lane cannot bypass it.
            SessionMutation::Record(stored)
                if matches!(
                    &stored.record,
                    LaneRecord::OperationStarted(record)
                        if matches!(&record.kind, OperationKind::Subagent { .. })
                            || self.agent_lanes.contains(&record.lane_id)
                ) =>
            {
                Ok(false)
            }
            SessionMutation::Record(stored) => self.validate_lifecycle_record(&stored.record),
            SessionMutation::Entry(_) | SessionMutation::Lane(_) | SessionMutation::Fact(_) => {
                Ok(false)
            }
        }
    }

    /// Advance this disposable index after a committed mutation.
    pub(crate) fn advance(&mut self, mutation: &StoredMutation) {
        if !self.incremental_validation_ready {
            return;
        }
        if self.is_locally_validated_mutation(mutation).is_err() {
            // Decoding builds the index before the authoritative whole-prefix
            // reducer runs. Do not let an invalid historical record make this
            // optimization authoritative or panic while that reducer prepares
            // its diagnostic.
            self.incremental_validation_ready = false;
            return;
        }
        match &mutation.mutation {
            SessionMutation::Entry(entry) => {
                self.entry_ids.insert(entry.header.id.clone());
                self.lane_leaves
                    .insert(entry.lane_id.clone(), Some(entry.header.id.clone()));
            }
            SessionMutation::Lane(stored) => {
                let LaneMutation::Created {
                    lane_id,
                    base_leaf_id,
                } = &stored.mutation;
                self.lane_leaves
                    .insert(lane_id.clone(), base_leaf_id.clone());
            }
            SessionMutation::Record(stored) => self.advance_lifecycle_record(&stored.record),
            SessionMutation::Fact(stored) => {
                if let SessionFact::AgentSpawned(spawn) = &stored.fact {
                    self.agent_lanes.insert(spawn.lane_id.clone());
                }
            }
        }
    }

    fn validate_lifecycle_record(&self, record: &LaneRecord) -> Result<bool, Corruption> {
        match record {
            LaneRecord::OperationStarted(record) => {
                if self.operations.contains_key(&record.id) {
                    return Err(Corruption::new(format!(
                        "duplicate operation ID {}",
                        record.id
                    )));
                }
                let Some(current_leaf) = self.lane_leaves.get(&record.lane_id) else {
                    return Err(Corruption::new(format!(
                        "operation {} targets unknown lane {}",
                        record.id, record.lane_id
                    )));
                };
                if &record.source_leaf_id != current_leaf {
                    return Err(Corruption::new(format!(
                        "operation {} accepted against stale lane leaf",
                        record.id
                    )));
                }
                if self.active_operations.contains_key(&record.lane_id) {
                    return Err(Corruption::new(format!(
                        "lane {} has more than one open operation",
                        record.lane_id
                    )));
                }
                let mut seen_inputs = BTreeSet::new();
                for provisioned in &record.original_input {
                    if self.entry_ids.contains(&provisioned.id) {
                        return Err(Corruption::new(format!(
                            "operation {} provisions an already materialized entry {}",
                            record.id, provisioned.id
                        )));
                    }
                    if self.provisioned_entries.contains_key(&provisioned.id)
                        || !seen_inputs.insert(provisioned.id.clone())
                    {
                        return Err(Corruption::new(format!(
                            "provisioned entry {} was accepted more than once",
                            provisioned.id
                        )));
                    }
                }
                Ok(true)
            }
            LaneRecord::OperationFinished(record) => {
                let operation = self.open_operation(&record.operation_id)?;
                if !operation.open_epochs.is_empty() {
                    return Err(Corruption::new(format!(
                        "operation {} finished with an open epoch",
                        record.operation_id
                    )));
                }
                match self.active_operations.get(&operation.lane_id) {
                    Some(current) if current == &record.operation_id => Ok(true),
                    _ => Err(Corruption::new(format!(
                        "operation {} was not active on its lane at finish",
                        record.operation_id
                    ))),
                }
            }
            LaneRecord::EpochStarted(record) => {
                let operation = self.open_operation(&record.operation_id)?;
                if !operation.open_epochs.is_empty() {
                    return Err(Corruption::new(format!(
                        "operation {} has more than one open epoch",
                        record.operation_id
                    )));
                }
                if record.epoch_index != operation.epoch_ids.len() as u32 {
                    return Err(Corruption::new(format!(
                        "epoch {} has non-consecutive index {}",
                        record.id, record.epoch_index
                    )));
                }
                if self.epochs.contains_key(&record.id) {
                    return Err(Corruption::new(format!("duplicate epoch ID {}", record.id)));
                }
                Ok(true)
            }
            LaneRecord::EpochFinished(record) => {
                let operation = self.open_operation(&record.operation_id)?;
                if self.epochs.get(&record.epoch_id) != Some(&record.operation_id)
                    || !operation.open_epochs.contains(&record.epoch_id)
                {
                    return Err(Corruption::new(format!(
                        "epoch {} is not open for operation {}",
                        record.epoch_id, record.operation_id
                    )));
                }
                Ok(true)
            }
            LaneRecord::StepAttempted(record) => {
                let operation = self.open_operation(&record.operation_id)?;
                if self.epochs.get(&record.epoch_id) != Some(&record.operation_id)
                    || !operation.open_epochs.contains(&record.epoch_id)
                {
                    return Err(Corruption::new(format!(
                        "step {} was recorded after its epoch settled",
                        record.id
                    )));
                }
                let key = (
                    record.operation_id.clone(),
                    record.epoch_id.clone(),
                    record.kind,
                );
                match self.step_attempts.get(&key) {
                    None if record.attempt == 1 => Ok(true),
                    Some((previous_id, previous_attempt))
                        if record.attempt == previous_attempt.saturating_add(1)
                            && &record.id != previous_id =>
                    {
                        Ok(true)
                    }
                    Some((previous_id, _)) if &record.id == previous_id => Err(Corruption::new(
                        format!("step ID {} was attempted more than once", record.id),
                    )),
                    _ => Err(Corruption::new(format!(
                        "step {} has non-consecutive attempt {}",
                        record.id, record.attempt
                    ))),
                }
            }
            LaneRecord::ProviderRequestStarted(record) => {
                let operation = self.open_operation(&record.operation_id)?;
                if self.epochs.get(&record.epoch_id) != Some(&record.operation_id)
                    || !operation.open_epochs.contains(&record.epoch_id)
                {
                    return Err(Corruption::new(format!(
                        "provider request {} refers to a closed or invalid epoch",
                        record.request_id
                    )));
                }
                if self.provider_starts.contains_key(&record.request_id) {
                    return Err(Corruption::new(format!(
                        "duplicate provider request ID {}",
                        record.request_id
                    )));
                }
                Ok(true)
            }
            LaneRecord::ProviderRequestSettled(record) => {
                let _ = self.open_operation(&record.operation_id)?;
                if self.provider_starts.get(&record.request_id) != Some(&record.operation_id)
                    || self.provider_settled.contains(&record.request_id)
                {
                    return Err(Corruption::new(format!(
                        "invalid duplicate or cross-operation provider settlement {}",
                        record.request_id
                    )));
                }
                Ok(true)
            }
            LaneRecord::Usage(record) => {
                let _ = self.open_operation(&record.operation_id)?;
                Ok(true)
            }
            LaneRecord::AbortRequested(_)
            | LaneRecord::ToolStarted(_)
            | LaneRecord::QueueEnqueued(_)
            | LaneRecord::QueueCancelled(_)
            | LaneRecord::WriteDeferred(_)
            | LaneRecord::HarnessActivationRequested(_) => Ok(false),
        }
    }

    fn advance_lifecycle_record(&mut self, record: &LaneRecord) {
        match record {
            LaneRecord::OperationStarted(record) => {
                for provisioned in &record.original_input {
                    self.provisioned_entries
                        .insert(provisioned.id.clone(), provisioned.clone());
                }
                self.active_operations
                    .insert(record.lane_id.clone(), record.id.clone());
                self.operations.insert(
                    record.id.clone(),
                    IndexedOperation {
                        lane_id: record.lane_id.clone(),
                        finished: false,
                        epoch_ids: Vec::new(),
                        open_epochs: BTreeSet::new(),
                    },
                );
            }
            LaneRecord::OperationFinished(record) => {
                let operation = self
                    .operations
                    .get_mut(&record.operation_id)
                    .expect("validated operation finish has a known operation");
                operation.finished = true;
                self.active_operations.remove(&operation.lane_id);
            }
            LaneRecord::EpochStarted(record) => {
                self.epochs
                    .insert(record.id.clone(), record.operation_id.clone());
                let operation = self
                    .operations
                    .get_mut(&record.operation_id)
                    .expect("validated epoch start has a known operation");
                operation.epoch_ids.push(record.id.clone());
                operation.open_epochs.insert(record.id.clone());
            }
            LaneRecord::EpochFinished(record) => {
                self.operations
                    .get_mut(&record.operation_id)
                    .expect("validated epoch finish has a known operation")
                    .open_epochs
                    .remove(&record.epoch_id);
            }
            LaneRecord::StepAttempted(record) => {
                self.step_attempts.insert(
                    (
                        record.operation_id.clone(),
                        record.epoch_id.clone(),
                        record.kind,
                    ),
                    (record.id.clone(), record.attempt),
                );
            }
            LaneRecord::ProviderRequestStarted(record) => {
                self.provider_starts
                    .insert(record.request_id.clone(), record.operation_id.clone());
            }
            LaneRecord::ProviderRequestSettled(record) => {
                self.provider_settled.insert(record.request_id.clone());
            }
            LaneRecord::Usage(_) => {}
            LaneRecord::AbortRequested(_)
            | LaneRecord::ToolStarted(_)
            | LaneRecord::QueueEnqueued(_)
            | LaneRecord::QueueCancelled(_)
            | LaneRecord::WriteDeferred(_)
            | LaneRecord::HarnessActivationRequested(_) => {}
        }
    }

    fn open_operation(&self, id: &OperationId) -> Result<&IndexedOperation, Corruption> {
        let Some(operation) = self.operations.get(id) else {
            return Err(Corruption::new(format!(
                "record refers to unknown operation {id}"
            )));
        };
        if operation.finished {
            return Err(Corruption::new(format!(
                "record follows terminal operation {id}"
            )));
        }
        Ok(operation)
    }
}

impl MemorySession {
    /// Create an empty v1 session.
    pub fn create(header: SessionHeader) -> Result<Self, SessionError> {
        Self::create_with_clock(header, Arc::new(SystemSessionClock))
    }

    /// Create an empty v1 session with an explicit commit clock.
    pub fn create_with_clock(
        mut header: SessionHeader,
        clock: Arc<dyn SessionClock>,
    ) -> Result<Self, SessionError> {
        if header.kind != "session" || header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::InvalidInput {
                message: "new sessions require a v1 session header".into(),
            });
        }
        if header.workspace.contains('\0') {
            return Err(SessionError::InvalidInput {
                message: "workspace identity contains a NUL byte".into(),
            });
        }
        crate::jsonl::seal_header(&mut header)?;
        let append_index = SessionAppendIndex::empty(&header);
        Ok(Self {
            snapshot: SessionSnapshot::empty(header),
            append_index,
            fault: None,
            clock,
        })
    }

    /// Return one atomic snapshot of the current in-memory durable prefix.
    pub fn snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        Ok(self.snapshot.clone())
    }

    /// Mark the reference backend faulted for a test that needs the same
    /// fail-closed behavior as a failed persistent write.
    pub fn fault_for_test(&mut self, message: impl Into<String>) {
        self.fault = Some(message.into());
    }

    fn ensure_writable(&self) -> Result<(), SessionError> {
        match &self.fault {
            Some(message) => Err(SessionError::Faulted {
                message: message.clone(),
            }),
            None => Ok(()),
        }
    }

    fn next_envelope(&self) -> (Sequence, u64) {
        (self.snapshot.next_sequence(), self.clock.now_ms())
    }

    fn commit_mutation(&mut self, mutation: StoredMutation) -> Result<(), SessionError> {
        if self.append_index.is_locally_validated_mutation(&mutation)? {
            self.append_index.advance(&mutation);
            self.snapshot.push_mutation(mutation);
            return Ok(());
        }
        validate_snapshot_append(&self.snapshot, &mutation)?;
        self.append_index.advance(&mutation);
        self.snapshot.push_mutation(mutation);
        Ok(())
    }
}

impl SessionReader for MemorySession {
    fn snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        Self::snapshot(self)
    }
}

impl SessionWriter for MemorySession {
    fn append_entry(
        &mut self,
        lane_id: &LaneId,
        entry: ProvisionedEntry,
    ) -> Result<StoredEntry, SessionError> {
        self.ensure_writable()?;
        if self.append_index.contains_entry(&entry.id) {
            return Err(SessionError::InvalidInput {
                message: format!("entry ID {} already materialized", entry.id),
            });
        }
        let parent_id = self.append_index.lane_leaf(lane_id)?;
        let (seq, timestamp_ms) = self.next_envelope();
        let stored = StoredEntry {
            lane_id: lane_id.clone(),
            header: EntryHeader {
                id: entry.id,
                parent_id,
                seq,
                timestamp_ms,
            },
            body: entry.body,
        };
        let mutation =
            crate::jsonl::seal_mutation(&self.snapshot, SessionMutation::Entry(stored.clone()))?;
        self.commit_mutation(mutation)?;
        Ok(stored)
    }

    fn append_record(&mut self, record: LaneRecord) -> Result<StoredRecord, SessionError> {
        self.ensure_writable()?;
        let (seq, timestamp_ms) = self.next_envelope();
        let stored = StoredRecord {
            seq,
            timestamp_ms,
            record,
        };
        let mutation =
            crate::jsonl::seal_mutation(&self.snapshot, SessionMutation::Record(stored.clone()))?;
        self.commit_mutation(mutation)?;
        Ok(stored)
    }

    fn append_lane_mutation(
        &mut self,
        mutation: LaneMutation,
    ) -> Result<StoredLaneMutation, SessionError> {
        self.ensure_writable()?;
        let (seq, timestamp_ms) = self.next_envelope();
        let stored = StoredLaneMutation {
            seq,
            timestamp_ms,
            mutation,
        };
        let mutation =
            crate::jsonl::seal_mutation(&self.snapshot, SessionMutation::Lane(stored.clone()))?;
        self.commit_mutation(mutation)?;
        Ok(stored)
    }

    fn append_fact(&mut self, fact: SessionFact) -> Result<StoredFact, SessionError> {
        self.ensure_writable()?;
        let (seq, timestamp_ms) = self.next_envelope();
        let stored = StoredFact {
            seq,
            timestamp_ms,
            fact,
        };
        let mutation =
            crate::jsonl::seal_mutation(&self.snapshot, SessionMutation::Fact(stored.clone()))?;
        self.commit_mutation(mutation)?;
        Ok(stored)
    }
}

pub(crate) fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), Corruption> {
    for lane in snapshot_lanes(snapshot) {
        let _ = reduce_lane_ref(snapshot, lane)?;
    }
    let _ = crate::reduce_agent_graph(snapshot)?;
    Ok(())
}

/// Validate one prospective mutation through the same pure reducer without
/// cloning the snapshot or its retained semantic payloads.
pub(crate) fn validate_snapshot_append(
    snapshot: &SessionSnapshot,
    appended: &StoredMutation,
) -> Result<(), Corruption> {
    let mut lanes = snapshot_lanes(snapshot);
    if let SessionMutation::Lane(StoredLaneMutation {
        mutation: LaneMutation::Created { lane_id, .. },
        ..
    }) = &appended.mutation
        && !lanes.contains(lane_id)
    {
        lanes.push(lane_id.clone());
    }
    for lane in lanes {
        let _ = reduce_lane_ref_with_append(snapshot, appended, lane)?;
    }
    let _ = crate::agents::reduce_agent_graph_ref_with_append(snapshot, appended)?;
    Ok(())
}

fn snapshot_lanes(snapshot: &SessionSnapshot) -> Vec<LaneId> {
    let mut lanes = vec![snapshot.header().initial_lane.clone()];
    for stored in snapshot.lane_mutations() {
        let crate::LaneMutation::Created { lane_id, .. } = &stored.mutation;
        if !lanes.contains(lane_id) {
            lanes.push(lane_id.clone());
        }
    }
    lanes
}

fn system_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
