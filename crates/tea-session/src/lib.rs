//! Durable, executor-agnostic session primitives for Tea.
//!
//! This crate owns semantic session state, operation facts, and immutable
//! artifacts. It deliberately has no provider, tool, executor, or Luau VM
//! dependency. `tea-harness` will drive those effects above this boundary.

mod artifact;
mod gc;
mod ids;
mod jsonl;
mod model;
mod reduction;
mod store;
mod verification;

pub use artifact::{
    ArtifactDescriptor, ArtifactError, ArtifactInventoryItem, ArtifactMatch, ArtifactPage,
    ArtifactPolicy, ArtifactStore, FileArtifactStore, MemoryArtifactStore,
};
pub use gc::{
    ArtifactGcPlan, ArtifactGcReport, ArtifactQuota, ArtifactQuotaStatus, apply_artifact_gc,
    plan_artifact_gc, session_artifact_roots,
};
pub use ids::{
    ArtifactId, ArtifactPolicyId, CanonicalHashWriter, CoreRunId, Digest, DigestError, EntryId,
    EpochId, ExperimentId, FailureSignatureId, HarnessCandidateId, HarnessRevisionId,
    HarnessSnapshotId, HarnessTreeId, IdError, IdGenerator, LaneId, ModelHarnessProfileId,
    NormalizedPath, NormalizedPathError, OperationId, ProviderRequestId, RecordId, Sequence,
    SessionId, StableHookId, StepId, ToolInvocationId,
};
pub use jsonl::{DurabilityMode, JsonlSession, SessionExport, SessionExportError};
pub use model::*;
pub use reduction::{Corruption, LaneReduction, RecoveryPlan, reduce_lane};
pub use store::{MemorySession, SessionError, SessionReader, SessionWriter};
pub use tea_protocol::JsonValue;
pub use verification::{SessionVerification, SessionVerificationError, verify_session};

#[cfg(test)]
mod tests;
