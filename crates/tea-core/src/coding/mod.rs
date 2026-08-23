//! Provider-independent coding profile composition and standard tools.
//!
//! The profile capture and its factories require explicit workspace and operation authority;
//! constructing them never discovers a provider, credential, session, or current directory.

pub mod profile;
pub mod tools;

pub use profile::{PiDefaultCodingProfile, ProfileSpec, TeaDefaultCodingProfileV2};
pub use tools::{
    CodingOperations, CommandEnvironment, CommandOutput, ConditionalFileEdit, DefaultCodingTools,
    DirectoryEntry, EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot,
    GrepMatch, GrepOptions, LocalCodingOperations, OperationError, OperationFuture,
    TeaCodingToolsV2, WorkspaceRoot,
};
