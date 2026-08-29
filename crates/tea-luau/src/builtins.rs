//! Checked-in closed source trees for Tea's bundled extensions.

use std::collections::{BTreeMap, BTreeSet};
use tea_core::harness::extension::{ExtensionLimits, ExtensionSourceTree};

/// Return the exact bundled goal extension source tree.
///
/// The caller supplies the immutable resource limits selected for the harness
/// snapshot. The files themselves are compile-time assets rather than Rust
/// strings synthesized at the terminal composition root.
pub fn goal(limits: ExtensionLimits) -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: "goal".into(),
        files: BTreeMap::from([
            (
                "manifest.json".into(),
                include_str!("../builtins/goal/manifest.json").into(),
            ),
            (
                "init.luau".into(),
                include_str!("../builtins/goal/init.luau").into(),
            ),
            (
                "prompts.luau".into(),
                include_str!("../builtins/goal/prompts.luau").into(),
            ),
        ]),
        expected_capabilities: Some(BTreeSet::from(["extension.state".into()])),
        limits,
    }
}

/// Return the immutable default coding-tool source tree.
///
/// Provider-facing metadata and ordinary tool semantics live in these Luau
/// files. The host independently supplies the four fixed capability grants;
/// changing this source therefore cannot widen workspace or process authority.
pub fn coding(limits: ExtensionLimits) -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: "coding".into(),
        files: BTreeMap::from([
            (
                "manifest.json".into(),
                include_str!("../builtins/coding/manifest.json").into(),
            ),
            (
                "init.luau".into(),
                include_str!("../builtins/coding/init.luau").into(),
            ),
            (
                "prompts.luau".into(),
                include_str!("../builtins/coding/prompts.luau").into(),
            ),
            (
                "tools/read.luau".into(),
                include_str!("../builtins/coding/tools/read.luau").into(),
            ),
            (
                "tools/bash.luau".into(),
                include_str!("../builtins/coding/tools/bash.luau").into(),
            ),
            (
                "tools/edit.luau".into(),
                include_str!("../builtins/coding/tools/edit.luau").into(),
            ),
            (
                "tools/find.luau".into(),
                include_str!("../builtins/coding/tools/find.luau").into(),
            ),
        ]),
        expected_capabilities: Some(BTreeSet::from([
            "tea.process.v1".into(),
            "tea.workspace.mutate.v1".into(),
            "tea.workspace.read.v1".into(),
            "tea.workspace.search.v1".into(),
        ])),
        limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, BundleManifest, BUNDLE_ABI_V2_VERSION};
    use crate::{LuaPolicy, LuauExtensionEngine, PolicyError};
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;
    use tea_core::effect::RunProvenance;
    use tea_core::harness::extension::ExtensionToolLimits;
    use tea_core::harness::extension::{
        ExtensionCapabilityBindings, ExtensionCommandInput, ExtensionEngine, ExtensionIdleInput,
        ExtensionMemoryCollector, ExtensionOperationOutcome, ExtensionStateView,
    };
    use tea_core::hooks::NoHooks;
    use tea_core::state::{SerializedJson, ToolCallId};
    use tea_core::{
        coding::{
            CodingHost, PROCESS_CAPABILITY_V1, WORKSPACE_MUTATE_CAPABILITY_V1,
            WORKSPACE_READ_CAPABILITY_V1, WORKSPACE_SEARCH_CAPABILITY_V1,
        },
        tool::{
            CancellationSettlementMode, ToolCall, ToolContext, ToolExecutionMode, ToolUpdateSink,
        },
    };

    fn policy() -> LuaPolicy {
        let tree = goal(ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        });
        let manifest = BundleManifest::new(BUNDLE_ABI_V2_VERSION, "init.luau", ["extension.state"])
            .expect("manifest is valid");
        LuaPolicy::load_bundle(
            Bundle::from_sources(
                manifest,
                tree.files
                    .iter()
                    .filter(|(path, _)| path.as_str() != "manifest.json")
                    .map(|(path, source)| (path.as_str(), source.as_str())),
            )
            .expect("bundle is closed"),
        )
        .expect("goal policy loads")
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    #[test]
    fn coding_is_a_closed_four_tool_bundle_with_fixed_grants() {
        static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);
        let workspace = std::env::temp_dir().join(format!(
            "tea-luau-coding-builtin-{}-{}",
            std::process::id(),
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&workspace).expect("coding fixture workspace creates");
        std::fs::write(workspace.join("fixture.txt"), "first\nsecond\n")
            .expect("coding fixture file writes");
        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let tree = coding(limits);
        let descriptor = LuauExtensionEngine
            .describe(&tree)
            .expect("coding descriptor resolves");
        assert_eq!(
            descriptor
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "bash", "edit", "find"],
        );
        assert!(descriptor.tools.iter().all(|tool| {
            !matches!(tool.name.as_str(), "write" | "grep" | "ls")
                && !tool.description.is_empty()
                && tool.schema.as_object().is_some()
                && tool
                    .schema
                    .get("type")
                    .and_then(tea_protocol::JsonValue::as_str)
                    == Some("object")
                && tool.execution_mode == ToolExecutionMode::Parallel
        }));
        assert_eq!(
            descriptor
                .tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool.capability.as_str()))
                .collect::<Vec<_>>(),
            [
                ("read", WORKSPACE_READ_CAPABILITY_V1),
                ("bash", PROCESS_CAPABILITY_V1),
                ("edit", WORKSPACE_MUTATE_CAPABILITY_V1),
                ("find", WORKSPACE_SEARCH_CAPABILITY_V1),
            ],
        );
        assert_eq!(descriptor.prompt_sections.len(), 1);
        assert_eq!(descriptor.prompt_sections[0].id, "coding");
        assert!(descriptor.prompt_sections[0]
            .content
            .contains("There are no separate `write`, `grep`, or `ls`"));
        let edit = descriptor
            .tools
            .iter()
            .find(|tool| tool.name == "edit")
            .expect("edit declaration exists");
        assert!(edit.requires_exclusive_batch);
        assert_eq!(
            edit.cancellation_settlement_mode,
            CancellationSettlementMode::AwaitFuture
        );
        assert!(descriptor.tools.iter().all(|tool| {
            tool.name == "edit"
                || (!tool.requires_exclusive_batch
                    && tool.cancellation_settlement_mode == CancellationSettlementMode::DropFuture)
        }));

        let host = CodingHost::new(&workspace).expect("coding authority configures");
        let mut bindings = ExtensionCapabilityBindings::new();
        let limits = ExtensionToolLimits::default();
        for (name, capability) in [
            (WORKSPACE_READ_CAPABILITY_V1, host.read_capability()),
            (WORKSPACE_SEARCH_CAPABILITY_V1, host.search_capability()),
            (WORKSPACE_MUTATE_CAPABILITY_V1, host.mutate_capability()),
            (PROCESS_CAPABILITY_V1, host.process_capability()),
        ] {
            bindings
                .insert(name, capability, limits)
                .expect("capability grant is unique");
        }
        let resolved = LuauExtensionEngine
            .resolve(
                &tree,
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("each checked-in coding handler loads");
        assert_eq!(
            resolved.tools.names().collect::<Vec<_>>(),
            ["read", "bash", "edit", "find"],
        );
        let context = ToolContext {
            cancellation: tea_core::scheduler::CancellationToken::new(),
            provenance: RunProvenance::default(),
        };
        let call = |name: &str, arguments: &str| ToolCall {
            id: ToolCallId::new(format!("coding-{name}")).expect("test call ID is valid"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        };
        let read = block_on(
            resolved
                .tools
                .get("read")
                .expect("read is resolved")
                .execute(
                    call("read", r#"{"path":"fixture.txt","limit":1}"#),
                    context.clone(),
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("checked-in read handler executes");
        assert_eq!(read.content, "first");
        let edit = block_on(
            resolved
                .tools
                .get("edit")
                .expect("edit is resolved")
                .execute(
                    call(
                        "edit",
                        r#"{"files":[{"path":"created.txt","content":"created\n"}]}"#,
                    ),
                    context.clone(),
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("checked-in edit handler executes");
        assert!(edit.content.contains("created 1 files"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
            "created\n"
        );
        let find = block_on(
            resolved
                .tools
                .get("find")
                .expect("find is resolved")
                .execute(
                    call("find", r#"{"pattern":"*.txt"}"#),
                    context.clone(),
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("checked-in find handler executes");
        assert!(find.content.contains("fixture.txt"));
        let bash = block_on(
            resolved
                .tools
                .get("bash")
                .expect("bash is resolved")
                .execute(
                    call(
                        "bash",
                        r#"{"command":"ls fixture.txt >/dev/null && grep -q second fixture.txt && printf luau-bash"}"#,
                    ),
                    context,
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("checked-in bash handler executes");
        assert_eq!(bash.content, "luau-bash");
        let _ = std::fs::remove_dir_all(workspace);
    }

    fn load_v2_policy(source: &str) -> Result<LuaPolicy, PolicyError> {
        let manifest = BundleManifest::new(
            BUNDLE_ABI_V2_VERSION,
            "init.luau",
            std::iter::empty::<&str>(),
        )
        .expect("v2 manifest is valid");
        let bundle =
            Bundle::from_sources(manifest, [("init.luau", source)]).expect("v2 bundle is closed");
        LuaPolicy::load_bundle(bundle)
    }

    #[test]
    fn goal_is_a_closed_deterministic_bundle() {
        let tree = goal(ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        });
        assert_eq!(tree.extension_id, "goal");
        assert_eq!(
            tree.files.keys().collect::<Vec<_>>(),
            ["init.luau", "manifest.json", "prompts.luau"]
        );
        assert_eq!(
            tree.expected_capabilities,
            Some(BTreeSet::from(["extension.state".into()]))
        );
        let descriptor = LuauExtensionEngine
            .describe(&tree)
            .expect("bundled goal source resolves");
        assert_eq!(descriptor.host_commands[0].name, "/goal");
        assert_eq!(
            descriptor
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["get_goal", "create_goal", "update_goal"],
        );
    }

    #[test]
    fn goal_command_and_idle_policy_keep_goal_semantics_in_luau() {
        let policy = policy();
        let started = policy
            .execute_host_command(
                "/goal",
                &ExtensionCommandInput {
                    arguments: "finish the durable extension design".into(),
                    state: ExtensionStateView::default(),
                },
            )
            .expect("goal starts");
        let state = started.state.expect("command persists state");
        assert_eq!(state.kind, "goal.state.v1");
        assert!(started.internal_input.is_some());

        let edited = policy
            .execute_host_command(
                "/goal",
                &ExtensionCommandInput {
                    arguments: "edit finish the extension and document it".into(),
                    state: ExtensionStateView {
                        latest: BTreeMap::from([(state.kind.clone(), state.content.clone())]),
                    },
                },
            )
            .expect("goal edit preserves accounting");
        assert_eq!(
            edited
                .state
                .as_ref()
                .and_then(|update| update.content.get("objective"))
                .and_then(tea_protocol::JsonValue::as_str),
            Some("finish the extension and document it"),
        );

        let empty_edit = policy
            .execute_host_command(
                "/goal",
                &ExtensionCommandInput {
                    arguments: "edit".into(),
                    state: ExtensionStateView::default(),
                },
            )
            .expect("empty edit is a bounded notice");
        assert_eq!(
            empty_edit.notice.as_deref(),
            Some("Goal objective must not be empty"),
        );

        let oversized = policy
            .execute_host_command(
                "/goal",
                &ExtensionCommandInput {
                    arguments: "x".repeat(4001),
                    state: ExtensionStateView::default(),
                },
            )
            .expect("oversized command is a bounded notice");
        assert_eq!(
            oversized.notice.as_deref(),
            Some("Goal objective must be at most 4000 characters"),
        );

        let paused = policy
            .execute_host_command(
                "/goal",
                &ExtensionCommandInput {
                    arguments: "pause".into(),
                    state: ExtensionStateView {
                        latest: BTreeMap::from([(state.kind.clone(), state.content.clone())]),
                    },
                },
            )
            .expect("goal pauses");
        assert!(paused.internal_input.is_none());
        assert_eq!(
            paused
                .state
                .expect("pause persists state")
                .content
                .get("status")
                .and_then(tea_protocol::JsonValue::as_str),
            Some("paused"),
        );

        let idle = policy
            .on_idle(&ExtensionIdleInput {
                operation_id: "operation-1".into(),
                outcome: ExtensionOperationOutcome::Completed,
                usage: Default::default(),
                elapsed_active_seconds: 2,
                state: ExtensionStateView {
                    latest: BTreeMap::from([(state.kind, state.content)]),
                },
            })
            .expect("idle callback runs");
        assert!(idle.internal_input.is_some());
        assert_eq!(
            idle.state
                .expect("idle accounts state")
                .content
                .get("tokens_used")
                .and_then(tea_protocol::JsonValue::as_u64),
            Some(0),
        );
    }

    #[test]
    fn v1_declarations_reject_v2_host_fields() {
        let error = match LuaPolicy::load(
            r#"return {
                prompt_sections = {},
                commands = {{ name = "/review", help = "review", handler = function(_) return nil end }},
            }"#,
        ) {
            Ok(_) => panic!("v1 must not silently accept host commands"),
            Err(error) => error,
        };
        assert!(
            matches!(error, PolicyError::Contract { message } if message.contains("unknown field"))
        );
    }

    #[test]
    fn v2_commands_reject_duplicates_and_invalid_results() {
        let duplicate = match load_v2_policy(
            r#"return {
                prompt_sections = {},
                commands = {
                    { name = "/review", help = "review", handler = function(_) return nil end },
                    { name = "/review", help = "again", handler = function(_) return nil end },
                },
            }"#,
        ) {
            Ok(_) => panic!("duplicate commands must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(duplicate, PolicyError::Contract { message } if message.contains("duplicate extension command"))
        );

        let policy = load_v2_policy(
            r#"return {
                prompt_sections = {},
                commands = {
                    { name = "/review", help = "review", handler = function(_) return { unexpected = true } end },
                },
            }"#,
        )
        .expect("valid v2 command loads");
        assert!(matches!(
            policy.execute_host_command(
                "/review",
                &ExtensionCommandInput {
                    arguments: String::new(),
                    state: ExtensionStateView::default(),
                },
            ),
            Err(PolicyError::Contract { .. })
        ));
    }
}
