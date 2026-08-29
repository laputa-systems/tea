//! Provider-independent coding workspace/process authority.
//!
//! `CodingHost` requires explicit workspace and operation authority; constructing
//! it never discovers a provider, credential, session, or current directory.

pub mod capabilities;
pub mod tools;

pub use capabilities::{
    CodingHost, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
    WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
};
pub use tools::{
    CodingOperations, CommandEnvironment, CommandOutput, ConditionalFileCreate,
    ConditionalFileEdit, EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot,
    LocalCodingOperations, OperationError, OperationFuture, WorkspaceRoot,
};
