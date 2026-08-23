//! Read-only verification of a durable session prefix and its immutable objects.
//!
//! Verification is deliberately independent from JSONL opening. Opening proves
//! the log can be decoded and reduced; this module additionally checks that
//! every artifact reachable from that immutable prefix still exists and hashes
//! to the identity recorded by the session. Callers provide transitive roots
//! (for example harness source blobs named by a retained catalog) because this
//! crate must not take ownership of another state plane's manifest format.

use crate::{
    ArtifactError, ArtifactId, ArtifactInventoryItem, ArtifactStore, Corruption, PayloadRef,
    SessionEntry, SessionFact, SessionSnapshot, session_artifact_roots,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Content-free result of verifying one immutable session prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionVerification {
    /// Verified durable session identity.
    pub session_id: crate::SessionId,
    /// Last committed mutation sequence in the verified prefix.
    pub last_sequence: crate::Sequence,
    /// Authenticated digest naming the verified committed prefix.
    pub last_digest: crate::Digest,
    /// Every direct or caller-supplied immutable artifact root checked.
    pub artifact_roots: BTreeSet<ArtifactId>,
    /// Number of checked artifact objects.
    pub artifact_count: usize,
    /// Total exact bytes of checked artifact objects.
    pub artifact_bytes: u64,
    /// Finalized objects outside the verified root set. These are not
    /// corruption; a separate reviewed GC pass may remove them.
    pub orphaned_artifacts: Vec<ArtifactInventoryItem>,
}

/// Failure while checking a recovered durable prefix.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionVerificationError {
    /// The JSONL-derived semantic/WAL prefix violates a reducer invariant.
    Corruption(Corruption),
    /// A referenced immutable object is missing, unsafe, or has invalid bytes.
    Artifact(ArtifactError),
    /// An explicit byte length in the durable prefix disagrees with its object.
    LengthMismatch {
        artifact_id: ArtifactId,
        expected: u64,
        actual: u64,
    },
    /// Two durable references claim incompatible lengths for one immutable ID.
    ConflictingLength {
        artifact_id: ArtifactId,
        first: u64,
        second: u64,
    },
}

impl fmt::Display for SessionVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corruption(error) => write!(formatter, "session corruption: {error}"),
            Self::Artifact(error) => {
                write!(formatter, "session artifact verification failed: {error}")
            }
            Self::LengthMismatch {
                artifact_id,
                expected,
                actual,
            } => write!(
                formatter,
                "artifact {artifact_id} has {actual} bytes but durable metadata requires {expected}"
            ),
            Self::ConflictingLength {
                artifact_id,
                first,
                second,
            } => write!(
                formatter,
                "artifact {artifact_id} has conflicting durable lengths {first} and {second}"
            ),
        }
    }
}

impl std::error::Error for SessionVerificationError {}

impl From<Corruption> for SessionVerificationError {
    fn from(value: Corruption) -> Self {
        Self::Corruption(value)
    }
}

impl From<ArtifactError> for SessionVerificationError {
    fn from(value: ArtifactError) -> Self {
        Self::Artifact(value)
    }
}

/// Recompute the session reducer and verify every reachable immutable object.
///
/// `additional_roots` is for transitive state that the session deliberately
/// references only by a manifest, such as immutable harness source objects.
/// The function never mutates the session or artifact store.
pub fn verify_session(
    snapshot: &SessionSnapshot,
    artifacts: &dyn ArtifactStore,
    additional_roots: impl IntoIterator<Item = ArtifactId>,
) -> Result<SessionVerification, SessionVerificationError> {
    crate::store::validate_snapshot(snapshot)?;

    let expected_lengths = expected_artifact_lengths(snapshot)?;
    let mut artifact_roots = session_artifact_roots(snapshot);
    artifact_roots.extend(additional_roots);

    let mut artifact_bytes = 0_u64;
    for artifact_id in &artifact_roots {
        let actual = artifacts.verify_object(*artifact_id)?;
        if let Some(expected) = expected_lengths.get(artifact_id)
            && *expected != actual
        {
            return Err(SessionVerificationError::LengthMismatch {
                artifact_id: *artifact_id,
                expected: *expected,
                actual,
            });
        }
        artifact_bytes = artifact_bytes.saturating_add(actual);
    }
    let orphaned_artifacts = artifacts
        .inventory()?
        .into_iter()
        .filter(|item| !artifact_roots.contains(&item.artifact_id))
        .collect();

    Ok(SessionVerification {
        session_id: snapshot.header().session_id.clone(),
        last_sequence: snapshot.last_sequence(),
        last_digest: snapshot.last_digest(),
        artifact_count: artifact_roots.len(),
        artifact_roots,
        artifact_bytes,
        orphaned_artifacts,
    })
}

fn expected_artifact_lengths(
    snapshot: &SessionSnapshot,
) -> Result<BTreeMap<ArtifactId, u64>, SessionVerificationError> {
    let mut expected = BTreeMap::new();
    for entry in snapshot.entries() {
        match &entry.body {
            SessionEntry::ToolResult(entry) => {
                record_payload_length(&mut expected, &entry.full_result)?;
            }
            SessionEntry::PluginMemory(entry) => {
                record_payload_length(&mut expected, &entry.content)?;
            }
            SessionEntry::Custom(entry) => {
                record_payload_length(&mut expected, &entry.payload)?;
            }
            SessionEntry::UserMessage(_)
            | SessionEntry::AssistantMessage(_)
            | SessionEntry::Compaction(_)
            | SessionEntry::BranchSummary(_)
            | SessionEntry::ModelChanged(_)
            | SessionEntry::ThinkingChanged(_)
            | SessionEntry::ToolActivationChanged(_)
            | SessionEntry::HarnessRevisionChanged(_) => {}
        }
    }
    for fact in snapshot.facts() {
        match &fact.fact {
            SessionFact::HarnessCatalog(catalog) => {
                record_length(&mut expected, catalog.artifact_id, catalog.byte_len)?;
            }
            SessionFact::ToolSchemaDeviation(deviation) => {
                record_payload_length(&mut expected, &deviation.raw_arguments)?;
            }
            SessionFact::TraceArtifact(trace) => {
                record_length(&mut expected, trace.artifact_id, trace.byte_len)?;
            }
            SessionFact::Custom { .. } => {}
        }
    }
    Ok(expected)
}

fn record_payload_length(
    expected: &mut BTreeMap<ArtifactId, u64>,
    payload: &PayloadRef,
) -> Result<(), SessionVerificationError> {
    if let PayloadRef::Artifact {
        artifact_id,
        byte_len,
        ..
    } = payload
    {
        record_length(expected, *artifact_id, *byte_len)?;
    }
    Ok(())
}

fn record_length(
    expected: &mut BTreeMap<ArtifactId, u64>,
    artifact_id: ArtifactId,
    byte_len: u64,
) -> Result<(), SessionVerificationError> {
    match expected.insert(artifact_id, byte_len) {
        Some(first) if first != byte_len => Err(SessionVerificationError::ConflictingLength {
            artifact_id,
            first,
            second: byte_len,
        }),
        _ => Ok(()),
    }
}
