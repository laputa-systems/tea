//! Trusted coding host-operation substrate.
//!
//! This module contains no model-facing tool definitions. The revisioned Luau
//! coding bundle owns that surface; Rust retains only operations, transactions,
//! workspace authority, and optimized workspace search.

pub(crate) mod contract;
pub(crate) mod local_operations;
pub(crate) mod search;
pub(crate) mod workspace;

pub use contract::{
    CodingOperations, CommandEnvironment, CommandOutput, ConditionalFileCreate,
    ConditionalFileEdit, EditTransaction, EditTransactionOutcome, EntryMetadata, FileSnapshot,
    OperationError, OperationFuture, SearchResult, SearchTruncation,
};
pub use local_operations::LocalCodingOperations;
pub use workspace::WorkspaceRoot;
