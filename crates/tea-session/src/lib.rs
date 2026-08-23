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
    apply_artifact_gc, plan_artifact_gc, session_artifact_roots, ArtifactGcPlan, ArtifactGcReport,
    ArtifactQuota, ArtifactQuotaStatus,
};
pub use jsonl::{DurabilityMode, JsonlSession, SessionExport, SessionExportError};
pub use ids::{
    ArtifactId, ArtifactPolicyId, CanonicalHashWriter, CoreRunId, Digest, DigestError, EntryId, EpochId, ExperimentId,
    FailureSignatureId, HarnessCandidateId, HarnessRevisionId, HarnessSnapshotId, HarnessTreeId,
    IdError, IdGenerator, LaneId, ModelHarnessProfileId, NormalizedPath, NormalizedPathError, OperationId, ProviderRequestId,
    RecordId, Sequence, SessionId, StableHookId, StepId, ToolInvocationId,
};
pub use model::*;
pub use reduction::{reduce_lane, Corruption, LaneReduction, RecoveryPlan};
pub use store::{MemorySession, SessionError, SessionReader, SessionWriter};
pub use verification::{verify_session, SessionVerification, SessionVerificationError};
pub use tea_protocol::JsonValue;

#[cfg(test)]
mod tests;
