use crate::{
    Corruption, EntryHeader, LaneId, LaneMutation, LaneRecord, ProvisionedEntry,
    SESSION_FORMAT_VERSION, Sequence, SessionFact, SessionHeader, SessionSnapshot, StoredEntry,
    StoredFact, StoredLaneMutation, StoredRecord, reduce_lane,
};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Failures at the durable-session boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionError {
    /// Caller-provided content violates the durable session contract.
    InvalidInput { message: String },
    /// An existing log prefix cannot be reduced safely.
    Corruption(Corruption),
    /// The store has faulted after an incomplete or failed write and must be reopened.
    Faulted { message: String },
    /// An I/O operation failed at a named path.
    Io { path: String, message: String },
    /// Another active writer owns the session.
    WriterBusy { path: String },
    /// A persisted format line is invalid.
    Format {
        path: String,
        line: usize,
        message: String,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput { message } => write!(formatter, "invalid session input: {message}"),
            Self::Corruption(error) => write!(formatter, "session corruption: {error}"),
            Self::Faulted { message } => write!(formatter, "session store is faulted: {message}"),
            Self::Io { path, message } => {
                write!(formatter, "session I/O failed at {path}: {message}")
            }
            Self::WriterBusy { path } => {
                write!(formatter, "session already has an active writer: {path}")
            }
            Self::Format {
                path,
                line,
                message,
            } => write!(
                formatter,
                "invalid session format at {path} line {line}: {message}"
            ),
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
    fault: Option<String>,
}

impl MemorySession {
    /// Create an empty v1 session.
    pub fn create(header: SessionHeader) -> Result<Self, SessionError> {
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
        Ok(Self {
            snapshot: SessionSnapshot::empty(header),
            fault: None,
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
        (self.snapshot.next_sequence(), commit_time_ms())
    }

    fn commit_candidate(&mut self, candidate: SessionSnapshot) -> Result<(), SessionError> {
        if let Err(error) = validate_snapshot(&candidate) {
            return Err(SessionError::Corruption(error));
        }
        self.snapshot = candidate;
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
        if self
            .snapshot
            .entries()
            .iter()
            .any(|stored| stored.header.id == entry.id)
        {
            return Err(SessionError::InvalidInput {
                message: format!("entry ID {} already materialized", entry.id),
            });
        }
        let reduction = reduce_lane(self.snapshot.clone(), lane_id.clone())?;
        let (seq, timestamp_ms) = self.next_envelope();
        let stored = StoredEntry {
            lane_id: lane_id.clone(),
            header: EntryHeader {
                id: entry.id,
                parent_id: reduction.lane_state.leaf_id,
                seq,
                timestamp_ms,
            },
            body: entry.body,
        };
        let mut candidate = self.snapshot.clone();
        candidate.push_entry(stored.clone());
        self.commit_candidate(candidate)?;
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
        let mut candidate = self.snapshot.clone();
        candidate.push_record(stored.clone());
        self.commit_candidate(candidate)?;
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
        let mut candidate = self.snapshot.clone();
        candidate.push_lane_mutation(stored.clone());
        self.commit_candidate(candidate)?;
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
        let mut candidate = self.snapshot.clone();
        candidate.push_fact(stored.clone());
        self.commit_candidate(candidate)?;
        Ok(stored)
    }
}

pub(crate) fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), Corruption> {
    let mut lanes = vec![snapshot.header().initial_lane.clone()];
    for stored in snapshot.lane_mutations() {
        let crate::LaneMutation::Created { lane_id, .. } = &stored.mutation;
        if !lanes.contains(lane_id) {
            lanes.push(lane_id.clone());
        }
    }
    for lane in lanes {
        let _ = reduce_lane(snapshot.clone(), lane)?;
    }
    Ok(())
}

pub(crate) fn commit_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
