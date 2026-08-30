//! Trusted coding host-operation substrate.
//!
//! This module contains no model-facing tool definitions. The revisioned Luau
//! coding builtins own that surface; Rust retains only operations, transactions,
//! workspace authority, and optimized workspace search.

pub(crate) mod contract;
pub(crate) mod local_operations;
mod process;
pub(crate) mod search;
pub(crate) mod workspace;

pub use contract::{
    CodingOperations, CommandEnvironment, CommandOutput, CommandTermination, ConditionalFileCreate,
    ConditionalFileEdit, EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot,
    OperationError, OperationFuture, SearchResult, SearchTruncation,
};
pub use local_operations::LocalCodingOperations;
pub use process::run_local_command;
pub use workspace::WorkspaceRoot;
