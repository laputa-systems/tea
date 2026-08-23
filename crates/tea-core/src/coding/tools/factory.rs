//! Factory and registry facade for the pinned standard coding tools.

use super::bash::BashTool;
use super::contract::{CodingOperations, CommandEnvironment, OperationError};
use super::edit::EditTool;
use super::find::FindTool;
use super::grep::GrepTool;
use super::local_operations::LocalCodingOperations;
use super::ls::LsTool;
use super::multiedit::MultiEditTool;
use super::read::ReadTool;
use super::workspace::WorkspaceRoot;
use super::write::WriteTool;
use crate::tool::{AgentTool, ToolRegistry};
use std::path::Path;
use std::sync::Arc;

/// The explicit batteries-included standard tool set.
#[derive(Clone)]
pub struct DefaultCodingTools {
    workspace: WorkspaceRoot,
    operations: Arc<dyn CodingOperations>,
    environment: CommandEnvironment,
}

impl std::fmt::Debug for DefaultCodingTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DefaultCodingTools")
            .field("workspace", &self.workspace)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl DefaultCodingTools {
    /// Construct the local standard tools for one existing workspace directory.
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, OperationError> {
        Self::with_operations(workspace, Arc::new(LocalCodingOperations))
    }

    /// Construct standard tools over caller-owned operations.
    pub fn with_operations(
        workspace: impl AsRef<Path>,
        operations: Arc<dyn CodingOperations>,
    ) -> Result<Self, OperationError> {
        Ok(Self {
            workspace: WorkspaceRoot::new(workspace)?,
            operations,
            environment: CommandEnvironment::empty(),
        })
    }

    /// Replace the explicit shell environment policy.
    pub fn with_environment(mut self, environment: CommandEnvironment) -> Self {
        self.environment = environment;
        self
    }

    /// Borrow the canonical workspace authority.
    pub fn workspace(&self) -> &WorkspaceRoot {
        &self.workspace
    }

    /// Return the default active coding tools in captured order.
    pub fn coding_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![self.read(), self.bash(), self.edit(), self.write()]
    }

    /// Return every pinned standard factory in captured order.
    pub fn all_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![
            self.read(),
            self.bash(),
            self.edit(),
            self.write(),
            self.grep(),
            self.find(),
            self.ls(),
        ]
    }

    /// Build a registry containing the active default coding tools.
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        for tool in self.coding_tools() {
            registry.insert(tool);
        }
        registry
    }

    /// Construct the read capability.
    pub fn read(&self) -> Arc<dyn AgentTool> {
        Arc::new(ReadTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
    /// Construct the bash capability.
    pub fn bash(&self) -> Arc<dyn AgentTool> {
        Arc::new(BashTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
            self.environment.clone(),
        ))
    }
    /// Construct the edit capability.
    pub fn edit(&self) -> Arc<dyn AgentTool> {
        Arc::new(EditTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
    /// Construct the write capability.
    pub fn write(&self) -> Arc<dyn AgentTool> {
        Arc::new(WriteTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
    /// Construct the grep capability.
    pub fn grep(&self) -> Arc<dyn AgentTool> {
        Arc::new(GrepTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
    /// Construct the find capability.
    pub fn find(&self) -> Arc<dyn AgentTool> {
        Arc::new(FindTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
    /// Construct the ls capability.
    pub fn ls(&self) -> Arc<dyn AgentTool> {
        Arc::new(LsTool::new(
            self.workspace.clone(),
            Arc::clone(&self.operations),
        ))
    }
}

/// Tea's explicit v2 default composition.
///
/// This is deliberately separate from [`DefaultCodingTools`]. The latter is
/// the executable side of the immutable Pi capture and must retain Pi's
/// single-file `edit` schema. V2 keeps the model-facing `edit` name but pairs
/// its `files[]` transaction schema with `TeaDefaultCodingProfileV2`.
#[derive(Clone, Debug)]
pub struct TeaCodingToolsV2 {
    legacy: DefaultCodingTools,
}

impl TeaCodingToolsV2 {
    /// Construct the v2 tools over the local filesystem host.
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, OperationError> {
        Ok(Self {
            legacy: DefaultCodingTools::new(workspace)?,
        })
    }

    /// Construct the v2 tools over caller-owned operations.
    pub fn with_operations(
        workspace: impl AsRef<Path>,
        operations: Arc<dyn CodingOperations>,
    ) -> Result<Self, OperationError> {
        Ok(Self {
            legacy: DefaultCodingTools::with_operations(workspace, operations)?,
        })
    }

    /// Replace the explicit shell environment policy.
    pub fn with_environment(mut self, environment: CommandEnvironment) -> Self {
        self.legacy = self.legacy.with_environment(environment);
        self
    }

    /// Borrow the canonical workspace authority.
    pub fn workspace(&self) -> &WorkspaceRoot {
        self.legacy.workspace()
    }

    /// Return the active Tea v2 tools in profile order.
    pub fn coding_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![self.read(), self.bash(), self.edit(), self.write()]
    }

    /// Build a registry for [`crate::coding::TeaDefaultCodingProfileV2`].
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        for tool in self.coding_tools() {
            registry.insert(tool);
        }
        registry
    }

    /// Construct Tea v2's digest-aware read capability.
    pub fn read(&self) -> Arc<dyn AgentTool> {
        Arc::new(ReadTool::tea_v2(
            self.legacy.workspace.clone(),
            Arc::clone(&self.legacy.operations),
        ))
    }

    /// Construct the unchanged explicit shell capability.
    pub fn bash(&self) -> Arc<dyn AgentTool> {
        self.legacy.bash()
    }

    /// Construct Tea v2's exclusive multi-file transactional edit capability.
    pub fn edit(&self) -> Arc<dyn AgentTool> {
        Arc::new(MultiEditTool::new(
            self.legacy.workspace.clone(),
            Arc::clone(&self.legacy.operations),
        ))
    }

    /// Construct the unchanged explicit write capability.
    pub fn write(&self) -> Arc<dyn AgentTool> {
        self.legacy.write()
    }
}
