//! Provider-independent coding workspace/process authority.
//!
//! `CodingHost` requires explicit workspace and operation authority; constructing
//! it never discovers a provider, credential, session, or current directory.

pub mod capabilities;
pub mod host;

pub use capabilities::{
    CodingHost, DEFAULT_PROCESS_TIMEOUT, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
    WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
};
pub use host::{
    CodingOperations, CommandEnvironment, CommandOutput, CommandTermination, ConditionalFileCreate,
    ConditionalFileEdit, EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot,
    LocalCodingOperations, OperationError, OperationFuture, SearchResult, SearchTruncation,
    WorkspaceRoot, run_local_command,
};
