//! Standard coding tools assembled behind an explicit workspace authority.

pub(crate) mod arguments;
pub(crate) mod bash;
pub(crate) mod contract;
pub(crate) mod edit;
pub(crate) mod factory;
pub(crate) mod find;
pub(crate) mod grep;
pub(crate) mod local_operations;
pub(crate) mod ls;
pub(crate) mod read;
pub(crate) mod schemas;
pub(crate) mod search;
#[cfg(test)]
mod tests;
pub(crate) mod workspace;
pub(crate) mod write;

pub use contract::{
    CodingOperations, CommandEnvironment, CommandOutput, DirectoryEntry, EntryMetadata, GrepMatch,
    GrepOptions, OperationError, OperationFuture,
};
pub use factory::DefaultCodingTools;
pub use local_operations::LocalCodingOperations;
pub use workspace::WorkspaceRoot;
