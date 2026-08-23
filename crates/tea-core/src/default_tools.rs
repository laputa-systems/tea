//! Batteries-included, explicit coding tools for the pinned Pi profile.
//!
//! The implementation is organized under `tools/`; this module remains the
//! public home for the default coding-tool contract.

pub use crate::tools::{
    CodingOperations, CommandEnvironment, CommandOutput, DefaultCodingTools, DirectoryEntry,
    EntryMetadata, GrepMatch, GrepOptions, LocalCodingOperations, OperationError, OperationFuture,
    WorkspaceRoot,
};
