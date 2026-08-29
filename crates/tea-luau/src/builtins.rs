//! Checked-in closed source trees for Tea's bundled extensions.

use std::collections::{BTreeMap, BTreeSet};
use tea_core::harness::extension::{ExtensionLimits, ExtensionSourceTree, ExtensionToolLimits};

/// Resource grant a host should give the bundled todo extension's
/// `extension.state` binding.
///
/// One invocation parses, normalizes, transforms, encodes, and formats a plan
/// of up to 128 items. Luau charges an interrupt per position scanned by a
/// string pattern, so that work costs several passes over a document of some
/// tens of kilobytes rather than the handful of checks a forwarding handler
/// needs. The grant stays finite and adds no authority; it only makes the
/// extension's own documented ceilings reachable. A host must still keep it
/// within its frozen harness resource limits.
pub fn todo_tool_limits() -> ExtensionToolLimits {
    ExtensionToolLimits {
        max_interrupt_checks: 200_000,
        max_memory_bytes: 4 * 1024 * 1024,
        ..ExtensionToolLimits::default()
    }
}

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

/// Return the exact bundled todo extension source tree.
///
/// Every todo semantic — Markdown parsing, ordering, identity allocation,
/// recursive status transitions, automatic promotion, counts, and the bounded
/// activity projection — lives in these Luau assets. Rust contributes only
/// generic extension persistence and generic presentation plumbing.
///
/// `core.luau` holds that state machine exactly once: `init.luau` requires it
/// for the read-only `/todos` command, and `handler.luau` requires it as the
/// executable tool module, so the two can never drift apart.
pub fn todo(limits: ExtensionLimits) -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: "todo".into(),
        files: BTreeMap::from([
            (
                "manifest.json".into(),
                include_str!("../builtins/todo/manifest.json").into(),
            ),
            (
                "init.luau".into(),
                include_str!("../builtins/todo/init.luau").into(),
            ),
            (
                "prompts.luau".into(),
                include_str!("../builtins/todo/prompts.luau").into(),
            ),
            (
                "core.luau".into(),
                include_str!("../builtins/todo/core.luau").into(),
            ),
            (
                "handler.luau".into(),
                include_str!("../builtins/todo/handler.luau").into(),
            ),
        ]),
        expected_capabilities: Some(BTreeSet::from(["extension.state".into()])),
        limits,
    }
}

/// Return the immutable default `read` builtin source tree.
pub fn read(limits: ExtensionLimits) -> ExtensionSourceTree {
    builtin(
        limits,
        "read",
        include_str!("../builtins/read/manifest.json"),
        include_str!("../builtins/read/init.luau"),
        include_str!("../builtins/read/prompts.luau"),
        include_str!("../builtins/read/handler.luau"),
        "tea.workspace.read.v1",
    )
}

/// Return the immutable default `bash` builtin source tree.
pub fn bash(limits: ExtensionLimits) -> ExtensionSourceTree {
    builtin(
        limits,
        "bash",
        include_str!("../builtins/bash/manifest.json"),
        include_str!("../builtins/bash/init.luau"),
        include_str!("../builtins/bash/prompts.luau"),
        include_str!("../builtins/bash/handler.luau"),
        "tea.process.v1",
    )
}

/// Return the immutable default `edit` builtin source tree.
pub fn edit(limits: ExtensionLimits) -> ExtensionSourceTree {
    builtin(
        limits,
        "edit",
        include_str!("../builtins/edit/manifest.json"),
        include_str!("../builtins/edit/init.luau"),
        include_str!("../builtins/edit/prompts.luau"),
        include_str!("../builtins/edit/handler.luau"),
        "tea.workspace.mutate.v1",
    )
}

/// Return the immutable default `find` builtin source tree.
pub fn find(limits: ExtensionLimits) -> ExtensionSourceTree {
    builtin(
        limits,
        "find",
        include_str!("../builtins/find/manifest.json"),
        include_str!("../builtins/find/init.luau"),
        include_str!("../builtins/find/prompts.luau"),
        include_str!("../builtins/find/handler.luau"),
        "tea.workspace.search.v1",
    )
}

/// Build one closed builtin from compile-time assets.
///
/// Each source tree exposes exactly one model-facing tool and requests only
/// that tool's capability. The host still independently fixes the
/// tool-to-capability mapping before it resolves the tree.
fn builtin(
    limits: ExtensionLimits,
    extension_id: &str,
    manifest: &str,
    init: &str,
    prompts: &str,
    handler: &str,
    capability: &str,
) -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: extension_id.into(),
        files: BTreeMap::from([
            ("manifest.json".into(), manifest.into()),
            ("init.luau".into(), init.into()),
            ("prompts.luau".into(), prompts.into()),
            ("handler.luau".into(), handler.into()),
        ]),
        expected_capabilities: Some(BTreeSet::from([capability.into()])),
        limits,
    }
}

/// Return the exact bundled web-retrieval extension source tree.
///
/// Web-provider protocol, fallback policy, and output policy remain in the
/// checked-in Luau source; the host independently grants only its route-scoped
/// generic `network.http` capability and any fixed host-only credentials.
pub fn web(limits: ExtensionLimits) -> ExtensionSourceTree {
    ExtensionSourceTree {
        extension_id: "web".into(),
        files: BTreeMap::from([
            (
                "manifest.json".into(),
                include_str!("../builtins/web/manifest.json").into(),
            ),
            (
                "init.luau".into(),
                include_str!("../builtins/web/init.luau").into(),
            ),
            (
                "handler_source.luau".into(),
                include_str!("../builtins/web/handler_source.luau").into(),
            ),
            (
                "prompts.luau".into(),
                include_str!("../builtins/web/prompts.luau").into(),
            ),
        ]),
        expected_capabilities: Some(BTreeSet::from(["network.http".into()])),
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
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;
    use tea_core::effect::RunProvenance;
    use tea_core::harness::extension::ExtensionToolLimits;
    use tea_core::harness::extension::{
        ExtensionCapability, ExtensionCapabilityBindings, ExtensionCapabilityError,
        ExtensionCapabilityFuture, ExtensionCapabilityRequest, ExtensionCapabilityResponse,
        ExtensionCommandInput, ExtensionEngine, ExtensionIdleInput, ExtensionMemoryCollector,
        ExtensionOperationOutcome, ExtensionStateView,
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

    #[derive(Clone)]
    struct FixedProcessCapability {
        response: tea_protocol::JsonValue,
    }

    impl ExtensionCapability for FixedProcessCapability {
        fn invoke(
            &self,
            _request: ExtensionCapabilityRequest,
            _cancellation: tea_core::scheduler::CancellationToken,
        ) -> ExtensionCapabilityFuture {
            let response = self.response.clone();
            Box::pin(async move { Ok(ExtensionCapabilityResponse { value: response }) })
        }
    }

    fn execute_bash_handler(response: tea_protocol::JsonValue) -> tea_core::tool::AgentToolResult {
        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let tree = bash(limits);
        let mut bindings = ExtensionCapabilityBindings::new();
        bindings
            .insert(
                PROCESS_CAPABILITY_V1,
                Arc::new(FixedProcessCapability { response }),
                ExtensionToolLimits::default(),
            )
            .expect("process capability is granted once");
        bindings
            .fix_tool_capabilities(
                BTreeMap::from([("bash".into(), PROCESS_CAPABILITY_V1.into())]),
                BTreeSet::new(),
            )
            .expect("bash capability is fixed");
        let resolved = LuauExtensionEngine
            .resolve(
                &tree,
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("checked-in bash handler loads");
        block_on(
            resolved
                .tools
                .get("bash")
                .expect("bash is resolved")
                .execute(
                    ToolCall {
                        id: ToolCallId::new("bash-settlement").expect("test call ID is valid"),
                        name: "bash".into(),
                        arguments: SerializedJson::new(r#"{"command":"ignored"}"#),
                    },
                    ToolContext {
                        cancellation: tea_core::scheduler::CancellationToken::new(),
                        provenance: RunProvenance::default(),
                    },
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("bash handler settles")
    }

    #[test]
    fn bash_handler_formats_typed_process_settlements_without_new_arguments() {
        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let descriptor = LuauExtensionEngine
            .describe(&bash(limits))
            .expect("bash descriptor resolves");
        let properties = descriptor.tools[0]
            .schema
            .get("properties")
            .and_then(tea_protocol::JsonValue::as_object)
            .expect("bash schema has properties");
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["command", "timeout"]
        );

        let success = execute_bash_handler(tea_protocol::JsonValue::object([
            ("content", tea_protocol::JsonValue::String(String::new())),
            ("truncated", tea_protocol::JsonValue::Bool(false)),
            ("termination", tea_protocol::JsonValue::String("exited".into())),
            (
                "exitCode",
                tea_protocol::JsonValue::Number(tea_protocol::JsonNumber::Signed(0)),
            ),
        ]));
        assert!(!success.is_error);
        assert_eq!(success.content, "(no output)");

        let nonzero = execute_bash_handler(tea_protocol::JsonValue::object([
            ("content", tea_protocol::JsonValue::String(String::new())),
            ("truncated", tea_protocol::JsonValue::Bool(false)),
            ("termination", tea_protocol::JsonValue::String("exited".into())),
            (
                "exitCode",
                tea_protocol::JsonValue::Number(tea_protocol::JsonNumber::Signed(7)),
            ),
        ]));
        assert!(nonzero.is_error);
        assert_eq!(nonzero.content, "command exited with status 7");

        let signaled = execute_bash_handler(tea_protocol::JsonValue::object([
            ("content", tea_protocol::JsonValue::String("partial".into())),
            ("truncated", tea_protocol::JsonValue::Bool(false)),
            ("termination", tea_protocol::JsonValue::String("signaled".into())),
            ("exitCode", tea_protocol::JsonValue::Null),
            (
                "signal",
                tea_protocol::JsonValue::Number(tea_protocol::JsonNumber::Signed(15)),
            ),
        ]));
        assert!(signaled.is_error);
        assert!(signaled.content.contains("partial"));
        assert!(signaled.content.contains("signal 15"));

        let timed_out = execute_bash_handler(tea_protocol::JsonValue::object([
            ("content", tea_protocol::JsonValue::String("partial".into())),
            ("truncated", tea_protocol::JsonValue::Bool(false)),
            ("termination", tea_protocol::JsonValue::String("timed_out".into())),
            ("exitCode", tea_protocol::JsonValue::Null),
        ]));
        assert!(timed_out.is_error);
        assert!(timed_out.content.contains("partial"));
        assert!(timed_out.content.contains("command timed out"));

        let indeterminate = execute_bash_handler(tea_protocol::JsonValue::object([
            ("content", tea_protocol::JsonValue::String("partial".into())),
            ("truncated", tea_protocol::JsonValue::Bool(false)),
            (
                "termination",
                tea_protocol::JsonValue::String("indeterminate".into()),
            ),
            ("exitCode", tea_protocol::JsonValue::Null),
            ("reason", tea_protocol::JsonValue::String("ignored here".into())),
        ]));
        assert!(indeterminate.is_error);
        assert!(
            indeterminate
                .content
                .contains("side effects may already exist")
        );
        assert!(indeterminate.content.contains("inspect state before retrying"));
        assert!(indeterminate.content.contains("ignored here"));
    }

    #[test]
    fn coding_builtins_are_closed_single_tool_extensions_with_fixed_grants() {
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
        let host = CodingHost::new(&workspace).expect("coding authority configures");
        let context = ToolContext {
            cancellation: tea_core::scheduler::CancellationToken::new(),
            provenance: RunProvenance::default(),
        };
        let call = |name: &str, arguments: &str| ToolCall {
            id: ToolCallId::new(format!("coding-{name}")).expect("test call ID is valid"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        };
        for (name, source, capability) in [
            (
                "read",
                read as fn(ExtensionLimits) -> ExtensionSourceTree,
                WORKSPACE_READ_CAPABILITY_V1,
            ),
            ("bash", bash, PROCESS_CAPABILITY_V1),
            ("edit", edit, WORKSPACE_MUTATE_CAPABILITY_V1),
            ("find", find, WORKSPACE_SEARCH_CAPABILITY_V1),
        ] {
            let tree = source(limits);
            assert_eq!(tree.extension_id, name);
            assert_eq!(
                tree.files.keys().collect::<Vec<_>>(),
                ["handler.luau", "init.luau", "manifest.json", "prompts.luau"],
            );
            assert_eq!(
                tree.expected_capabilities,
                Some(BTreeSet::from([capability.into()])),
            );
            let descriptor = LuauExtensionEngine
                .describe(&tree)
                .expect("builtin descriptor resolves");
            assert_eq!(descriptor.requested_capabilities, BTreeSet::from([capability.into()]));
            assert_eq!(descriptor.prompt_sections.len(), 1);
            assert_eq!(descriptor.prompt_sections[0].id, name);
            assert!(!descriptor.prompt_sections[0].content.is_empty());
            assert_eq!(descriptor.tools.len(), 1);
            let tool = &descriptor.tools[0];
            assert_eq!(tool.name, name);
            assert_eq!(tool.capability, capability);
            assert!(!tool.description.is_empty());
            assert_eq!(
                tool.schema
                    .get("type")
                    .and_then(tea_protocol::JsonValue::as_str),
                Some("object")
            );
            assert_eq!(tool.execution_mode, ToolExecutionMode::Parallel);
            if name == "edit" {
                assert!(tool.description.contains("parent directory must already exist"));
                assert!(tool.requires_exclusive_batch);
                assert_eq!(
                    tool.cancellation_settlement_mode,
                    CancellationSettlementMode::AwaitFuture
                );
            } else {
                assert!(!tool.requires_exclusive_batch);
                assert_eq!(
                    tool.cancellation_settlement_mode,
                    CancellationSettlementMode::DropFuture
                );
            }
            if name == "find" {
                let find_properties = tool
                    .schema
                    .get("properties")
                    .and_then(tea_protocol::JsonValue::as_object)
                    .expect("find schema has properties");
                assert_eq!(
                    find_properties
                        .get("pattern")
                        .and_then(|property| property.get("maxLength"))
                        .and_then(tea_protocol::JsonValue::as_u64),
                    Some(4096)
                );
                assert_eq!(
                    find_properties
                        .get("limit")
                        .and_then(|property| property.get("maximum"))
                        .and_then(tea_protocol::JsonValue::as_u64),
                    Some(1000)
                );
            }

            let implementation = match name {
                "read" => host.read_capability(),
                "bash" => host.process_capability(),
                "edit" => host.mutate_capability(),
                "find" => host.search_capability(),
                _ => unreachable!("closed builtin catalog"),
            };
            let mut bindings = ExtensionCapabilityBindings::new();
            bindings
                .insert(capability, implementation, ExtensionToolLimits::default())
                .expect("capability grant is unique");
            bindings
                .fix_tool_capabilities(BTreeMap::from([(name.into(), capability.into())]), BTreeSet::new())
                .expect("tool authority is fixed");
            let resolved = LuauExtensionEngine
                .resolve(
                    &tree,
                    bindings,
                    Arc::new(NoHooks),
                    0,
                    Arc::new(ExtensionMemoryCollector::default()),
                )
                .expect("checked-in builtin handler loads");
            assert_eq!(resolved.tools.names().collect::<Vec<_>>(), [name]);

            match name {
                "read" => {
                    let output = block_on(
                        resolved
                            .tools
                            .get(name)
                            .expect("read is resolved")
                            .execute(
                                call(name, r#"{"path":"fixture.txt","limit":1}"#),
                                context.clone(),
                                ToolUpdateSink::disabled(),
                            ),
                    )
                    .expect("checked-in read handler executes");
                    assert_eq!(output.content, "first");
                }
                "bash" => {
                    let output = block_on(
                        resolved
                            .tools
                            .get(name)
                            .expect("bash is resolved")
                            .execute(
                                call(
                                    name,
                                    r#"{"command":"ls fixture.txt >/dev/null && grep -q second fixture.txt && printf luau-bash"}"#,
                                ),
                                context.clone(),
                                ToolUpdateSink::disabled(),
                            ),
                    )
                    .expect("checked-in bash handler executes");
                    assert_eq!(output.content, "luau-bash");
                }
                "edit" => {
                    let output = block_on(
                        resolved
                            .tools
                            .get(name)
                            .expect("edit is resolved")
                            .execute(
                                call(
                                    name,
                                    r#"{"files":[{"path":"created.txt","content":"created\n"}]}"#,
                                ),
                                context.clone(),
                                ToolUpdateSink::disabled(),
                            ),
                    )
                    .expect("checked-in edit handler executes");
                    assert_eq!(output.content, "Created 1 file.");
                    assert_eq!(
                        std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
                        "created\n"
                    );
                }
                "find" => {
                    let output = block_on(
                        resolved
                            .tools
                            .get(name)
                            .expect("find is resolved")
                            .execute(
                                call(name, r#"{"pattern":"*.txt"}"#),
                                context.clone(),
                                ToolUpdateSink::disabled(),
                            ),
                    )
                    .expect("checked-in find handler executes");
                    assert!(output.content.contains("fixture.txt"));
                    let bounded = block_on(
                        resolved
                            .tools
                            .get(name)
                            .expect("find is resolved")
                            .execute(
                                call("find-bounded", r#"{"pattern":"*.txt","limit":1}"#),
                                context.clone(),
                                ToolUpdateSink::disabled(),
                            ),
                    )
                    .expect("checked-in bounded find handler executes");
                    assert!(bounded.content.ends_with("[1 results limit reached]"));
                }
                _ => unreachable!("closed builtin catalog"),
            }
        }
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn coding_edit_receipt_distinguishes_precise_edits_existing_files_and_creations() {
        static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);
        let workspace = std::env::temp_dir().join(format!(
            "tea-luau-coding-edit-receipt-{}-{}",
            std::process::id(),
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&workspace).expect("coding fixture workspace creates");
        std::fs::write(workspace.join("precise.txt"), "before\n").expect("precise fixture writes");
        std::fs::write(workspace.join("complete.txt"), "before\n")
            .expect("complete fixture writes");
        std::fs::write(workspace.join("mixed-precise.txt"), "before\n")
            .expect("mixed precise fixture writes");
        std::fs::write(workspace.join("mixed-complete.txt"), "before\n")
            .expect("mixed complete fixture writes");
        std::fs::write(workspace.join("no-op.txt"), "unchanged\n").expect("no-op fixture writes");

        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let tree = edit(limits);
        let host = CodingHost::new(&workspace).expect("coding authority configures");
        let mut bindings = ExtensionCapabilityBindings::new();
        let limits = ExtensionToolLimits::default();
        bindings
            .insert(
                WORKSPACE_MUTATE_CAPABILITY_V1,
                host.mutate_capability(),
                limits,
            )
            .expect("capability grant is unique");
        bindings
            .fix_tool_capabilities(
                BTreeMap::from([(
                    "edit".into(),
                    WORKSPACE_MUTATE_CAPABILITY_V1.into(),
                )]),
                BTreeSet::new(),
            )
            .expect("edit authority is fixed");
        let resolved = LuauExtensionEngine
            .resolve(
                &tree,
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("coding edit handler resolves");
        let context = ToolContext {
            cancellation: tea_core::scheduler::CancellationToken::new(),
            provenance: RunProvenance::default(),
        };
        let execute = |id: &str, arguments: &str| {
            block_on(
                resolved
                    .tools
                    .get("edit")
                    .expect("edit is resolved")
                    .execute(
                        ToolCall {
                            id: ToolCallId::new(id).expect("test call ID is valid"),
                            name: "edit".into(),
                            arguments: SerializedJson::new(arguments),
                        },
                        context.clone(),
                        ToolUpdateSink::disabled(),
                    ),
            )
            .expect("checked-in edit handler executes")
        };

        assert_eq!(
            execute(
                "coding-edit-precise",
                r#"{"files":[{"path":"precise.txt","edits":[{"oldText":"before","newText":"after"}]}]}"#,
            )
            .content,
            "Changed 1 existing file with 1 precise replacement."
        );
        assert_eq!(
            execute(
                "coding-edit-complete",
                r#"{"files":[{"path":"complete.txt","content":"after\n"}]}"#,
            )
            .content,
            "Changed 1 existing file."
        );
        assert_eq!(
            execute(
                "coding-edit-create",
                r#"{"files":[{"path":"created.txt","content":"created\n"}]}"#,
            )
            .content,
            "Created 1 file."
        );
        assert_eq!(
            execute(
                "coding-edit-mixed",
                r#"{"files":[{"path":"mixed-precise.txt","edits":[{"oldText":"before","newText":"after"}]},{"path":"mixed-complete.txt","content":"after\n"},{"path":"mixed-created.txt","content":"created\n"}]}"#,
            )
            .content,
            "Changed 2 existing files with 1 precise replacement; created 1 file."
        );
        assert_eq!(
            execute(
                "coding-edit-no-op",
                r#"{"files":[{"path":"no-op.txt","content":"unchanged\n"}]}"#,
            )
            .content,
            "No files changed."
        );

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

    #[derive(Clone, Default)]
    struct FakeWebCapability {
        calls: Arc<Mutex<Vec<(String, tea_protocol::JsonValue)>>>,
    }

    impl tea_core::harness::extension::ExtensionCapability for FakeWebCapability {
        fn invoke(
            &self,
            request: ExtensionCapabilityRequest,
            _cancellation: tea_core::scheduler::CancellationToken,
        ) -> ExtensionCapabilityFuture {
            self.calls
                .lock()
                .expect("fake web call lock")
                .push((request.method.clone(), request.arguments.clone()));
            let response = match request.method.as_str() {
                "request" => {
                    let route = request
                        .arguments
                        .get("route")
                        .and_then(tea_protocol::JsonValue::as_str);
                    if route == Some("tinyfish-search") {
                        let query = request
                            .arguments
                            .get("query")
                            .and_then(|query| query.get("query"))
                            .and_then(tea_protocol::JsonValue::as_str);
                        if query == Some("rate") {
                            response(429, tea_protocol::JsonValue::object([(
                                "error",
                                tea_protocol::JsonValue::String("TinyFish rate limited".into()),
                            )]))
                        } else {
                            response_json(
                                r#"{"results":[{"url":"https://tinyfish.example","title":"TinyFish result","snippet":"fallback snippet","site_name":"tinyfish.example"}]}"#,
                            )
                        }
                    } else if route == Some("tinyfish-fetch") {
                        let urls = request
                            .arguments
                            .get("json")
                            .and_then(|json| json.get("urls"))
                            .and_then(tea_protocol::JsonValue::as_array)
                            .expect("TinyFish fetch has URLs");
                        response(200, tea_protocol::JsonValue::object([
                            (
                                "results",
                                tea_protocol::JsonValue::Array(
                                    urls.iter()
                                        .filter_map(tea_protocol::JsonValue::as_str)
                                        .filter(|url| {
                                            *url == "https://tinyfish.example"
                                                || url.contains("fallback")
                                        })
                                        .map(|url| {
                                            tea_protocol::JsonValue::object([
                                                ("url", tea_protocol::JsonValue::String(url.into())),
                                                ("title", tea_protocol::JsonValue::String("TinyFish page".into())),
                                                ("text", tea_protocol::JsonValue::String("TinyFish fallback body".into())),
                                            ])
                                        })
                                        .collect(),
                                ),
                            ),
                            (
                                "errors",
                                tea_protocol::JsonValue::Array(
                                    urls.iter()
                                        .filter_map(tea_protocol::JsonValue::as_str)
                                        .filter(|url| {
                                            *url != "https://tinyfish.example"
                                                && !url.contains("fallback")
                                        })
                                        .map(|url| {
                                            tea_protocol::JsonValue::object([
                                                ("url", tea_protocol::JsonValue::String(url.into())),
                                                ("message", tea_protocol::JsonValue::String("TinyFish fixture fetch failed".into())),
                                            ])
                                        })
                                        .collect(),
                                ),
                            ),
                        ]))
                    } else {
                    let query = request
                        .arguments
                        .get("json")
                        .and_then(|json| json.get("query"))
                        .and_then(tea_protocol::JsonValue::as_str);
                    if query == Some("tinyfish-query") {
                        response(503, tea_protocol::JsonValue::object([(
                            "error",
                            tea_protocol::JsonValue::String("Firecrawl unavailable".into()),
                        )]))
                    } else if query == Some("rate") {
                        response(429, tea_protocol::JsonValue::object([(
                            "error",
                            tea_protocol::JsonValue::String("quota exhausted".into()),
                        )]))
                    } else if query == Some("large") {
                        response(
                            200,
                            tea_protocol::JsonValue::object([
                                ("success", tea_protocol::JsonValue::Bool(true)),
                                (
                                    "data",
                                    tea_protocol::JsonValue::object([(
                                        "web",
                                        tea_protocol::JsonValue::Array(vec![
                                            tea_protocol::JsonValue::object([
                                                (
                                                    "url",
                                                    tea_protocol::JsonValue::String(
                                                        "https://large.example".into(),
                                                    ),
                                                ),
                                                (
                                                    "title",
                                                    tea_protocol::JsonValue::String("Large".into()),
                                                ),
                                                (
                                                    "markdown",
                                                    tea_protocol::JsonValue::String(
                                                        "é".repeat(20_000),
                                                    ),
                                                ),
                                            ]),
                                        ]),
                                    )]),
                                ),
                            ]),
                        )
                    } else if query == Some("repair") {
                        response_json(
                            r#"{"success":true,"data":{"web":[{"url":"https://one.example","title":"One","markdown":"first"},{"url":"https://two.example","title":"Two"},{"url":"https://three.example","title":"Three"}]}}"#,
                        )
                    } else if query == Some("repair-fallback") {
                        response_json(
                            r#"{"success":true,"data":{"web":[{"url":"https://fallback.example","title":"Fallback repair"}]}}"#,
                        )
                    } else {
                        response_json(
                            r##"{"success":true,"data":{"web":[{"url":"https://docs.example","title":"Documentation","markdown":"# Evidence\nactual source"}]}}"##,
                        )
                    }
                    }
                }
                "request_many" => {
                    let requests = request
                        .arguments
                        .get("requests")
                        .and_then(tea_protocol::JsonValue::as_array)
                        .expect("batch requests are an array");
                    tea_protocol::JsonValue::Array(
                        requests
                            .iter()
                            .enumerate()
                            .map(|(index, request)| {
                                let url = request
                                    .get("json")
                                    .and_then(|json| json.get("url"))
                                    .and_then(tea_protocol::JsonValue::as_str)
                                    .unwrap_or_default();
                                if url.contains("fail") || url.contains("fallback") {
                                    response(429, tea_protocol::JsonValue::object([(
                                        "error",
                                        tea_protocol::JsonValue::String("rate limited".into()),
                                    )]))
                                } else {
                                    response_json(&format!(
                                        r#"{{"success":true,"data":{{"title":"Page {}","markdown":"page {} body"}}}}"#,
                                        index + 1,
                                        index + 1,
                                    ))
                                }
                            })
                            .collect(),
                    )
                }
                method => {
                    let method = method.to_owned();
                    return Box::pin(async move {
                        Err(ExtensionCapabilityError::MethodDenied {
                            capability: "network.http".into(),
                            method,
                        })
                    });
                }
            };
            Box::pin(async move { Ok(ExtensionCapabilityResponse { value: response }) })
        }
    }

    fn response_json(body: &str) -> tea_protocol::JsonValue {
        response(
            200,
            tea_protocol::JsonValue::parse(body).expect("fixture JSON is valid"),
        )
    }

    fn response(status: u64, body: tea_protocol::JsonValue) -> tea_protocol::JsonValue {
        tea_protocol::JsonValue::object([
            ("kind", tea_protocol::JsonValue::String("response".into())),
            ("status", tea_protocol::JsonValue::from(status)),
            ("attempts", tea_protocol::JsonValue::from(1_u64)),
            ("headers", tea_protocol::JsonValue::Object(BTreeMap::new())),
            ("json", body),
        ])
    }

    #[test]
    fn web_bundle_is_closed_and_executes_search_repair_and_url_batches() {
        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let tree = web(limits);
        assert_eq!(tree.extension_id, "web");
        assert_eq!(
            tree.files.keys().collect::<Vec<_>>(),
            [
                "handler_source.luau",
                "init.luau",
                "manifest.json",
                "prompts.luau"
            ]
        );
        assert_eq!(
            tree.expected_capabilities,
            Some(BTreeSet::from(["network.http".into()]))
        );
        let descriptor = LuauExtensionEngine
            .describe(&tree)
            .expect("bundled web source resolves");
        assert_eq!(descriptor.tools.len(), 1);
        let tool = &descriptor.tools[0];
        assert_eq!(tool.name, "web");
        assert_eq!(tool.capability, "network.http");
        assert!(tool.description.contains("batch independent known URLs"));
        let branches = tool
            .schema
            .get("oneOf")
            .and_then(tea_protocol::JsonValue::as_array)
            .expect("web schema is a strict oneOf");
        assert_eq!(branches.len(), 2);

        let fake = FakeWebCapability::default();
        let mut bindings = ExtensionCapabilityBindings::new();
        bindings
            .insert(
                "network.http",
                Arc::new(fake.clone()),
                ExtensionToolLimits {
                    max_memory_bytes: 1536 * 1024,
                    max_interrupt_checks: 100_000,
                    ..ExtensionToolLimits::default()
                },
            )
            .expect("network HTTP is granted once");
        let resolved = LuauExtensionEngine
            .resolve(
                &tree,
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("web handler resolves through the real extension engine");
        let context = ToolContext {
            cancellation: tea_core::scheduler::CancellationToken::new(),
            provenance: RunProvenance::default(),
        };
        let execute = |id: &str, arguments: &str| {
            block_on(resolved.tools.get("web").expect("web resolves").execute(
                ToolCall {
                    id: ToolCallId::new(id).expect("call ID is valid"),
                    name: "web".into(),
                    arguments: SerializedJson::new(arguments),
                },
                context.clone(),
                ToolUpdateSink::disabled(),
            ))
            .expect("web handler executes")
        };

        let search = execute("web-search", r#"{"query":"rustls defaults"}"#);
        assert!(!search.is_error);
        assert!(search.content.contains("Mode: developer"));
        assert!(search.content.contains("BEGIN UNTRUSTED WEB CONTENT"));
        let calls = fake.calls.lock().expect("fake web call lock");
        assert_eq!(
            calls.len(),
            1,
            "search with Markdown needs no follow-up scrape"
        );
        let search_json = calls[0].1.get("json").expect("search JSON request exists");
        assert_eq!(
            search_json
                .get("categories")
                .and_then(tea_protocol::JsonValue::as_array)
                .and_then(|categories| categories.first())
                .and_then(tea_protocol::JsonValue::as_str),
            Some("developer")
        );
        assert_eq!(
            search_json
                .get("scrapeOptions")
                .and_then(|options| options.get("onlyMainContent"))
                .and_then(tea_protocol::JsonValue::as_bool),
            Some(true)
        );
        drop(calls);

        let repair = execute("web-repair", r#"{"query":"repair","kind":"web","limit":3}"#);
        assert!(!repair.is_error);
        let calls = fake.calls.lock().expect("fake web call lock");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].0, "request");
        assert_eq!(calls[2].0, "request_many");
        assert_eq!(
            calls[2]
                .1
                .get("requests")
                .and_then(tea_protocol::JsonValue::as_array)
                .map(|requests| requests.len()),
            Some(2)
        );
        drop(calls);

        let before_repair_fallback = fake.calls.lock().expect("fake web call lock").len();
        let repaired_by_tinyfish = execute("web-repair-fallback", r#"{"query":"repair-fallback"}"#);
        assert!(!repaired_by_tinyfish.is_error);
        assert!(repaired_by_tinyfish.content.contains("TinyFish fallback body"));
        let calls = fake.calls.lock().expect("fake web call lock");
        let repair_fallback_calls = &calls[before_repair_fallback..];
        assert_eq!(repair_fallback_calls.len(), 3);
        assert_eq!(repair_fallback_calls[0].0, "request");
        assert_eq!(repair_fallback_calls[1].0, "request_many");
        assert_eq!(
            repair_fallback_calls[2]
                .1
                .get("route")
                .and_then(tea_protocol::JsonValue::as_str),
            Some("tinyfish-fetch")
        );
        drop(calls);

        let general = execute("web-general", r#"{"query":"general","kind":"web"}"#);
        assert!(!general.is_error);
        let calls = fake.calls.lock().expect("fake web call lock");
        let general_json = calls
            .last()
            .and_then(|call| call.1.get("json"))
            .expect("general search JSON exists");
        assert!(general_json.get("categories").is_none());
        drop(calls);

        let invalid = execute("web-invalid", r#"{"query":"foo","action":"search"}"#);
        assert!(invalid.is_error);
        assert!(invalid.content.contains("accepts only query"));

        let partial = execute(
            "web-partial",
            r#"{"urls":["https://one.example","https://fail.example","https://three.example"]}"#,
        );
        assert!(!partial.is_error);
        let first = partial
            .content
            .find("[1] Page 1")
            .expect("first source remains first");
        let failed = partial
            .content
            .find("[2] FAILED")
            .expect("failure is represented in place");
        let third = partial
            .content
            .find("[3] Page 3")
            .expect("later source retains input index");
        assert!(first < failed && failed < third);

        let all_failed = execute(
            "web-all-failed",
            r#"{"urls":["https://fail-one.example","https://fail-two.example"]}"#,
        );
        assert!(all_failed.is_error);
        assert!(all_failed.content.contains("bash tool with curl"));

        let limited = execute("web-rate", r#"{"query":"rate"}"#);
        assert!(limited.is_error);
        assert!(limited.content.contains("HTTP 429"));
        assert!(limited.content.contains("curl"));

        let before_fallback = fake.calls.lock().expect("fake web call lock").len();
        let fallback = execute("web-tinyfish-query", r#"{"query":"tinyfish-query"}"#);
        assert!(!fallback.is_error, "{}", fallback.content);
        assert!(fallback.content.contains("TinyFish fallback body"));
        let calls = fake.calls.lock().expect("fake web call lock");
        let fallback_calls = &calls[before_fallback..];
        assert_eq!(fallback_calls.len(), 3, "fallback searches then fetches its results");
        assert_eq!(
            fallback_calls[0].1.get("route").and_then(tea_protocol::JsonValue::as_str),
            Some("firecrawl")
        );
        assert_eq!(
            fallback_calls[1].1.get("route").and_then(tea_protocol::JsonValue::as_str),
            Some("tinyfish-search")
        );
        assert_eq!(
            fallback_calls[1].1.get("method").and_then(tea_protocol::JsonValue::as_str),
            Some("GET")
        );
        assert_eq!(
            fallback_calls[2].1.get("route").and_then(tea_protocol::JsonValue::as_str),
            Some("tinyfish-fetch")
        );
        drop(calls);

        let url_fallback = execute(
            "web-tinyfish-urls",
            r#"{"urls":["https://one.example","https://fallback.example"]}"#,
        );
        assert!(!url_fallback.is_error);
        assert!(url_fallback.content.contains("TinyFish fallback body"));
        assert!(!url_fallback.content.contains("[2] FAILED"));

        let truncated = execute("web-large", r#"{"query":"large"}"#);
        assert!(!truncated.is_error);
        assert!(truncated.content.contains("[content truncated;"));
        assert!(truncated.content.len() <= 96 * 1024 + 8 * 1024);

        let urls = execute(
            "web-urls",
            r#"{"urls":["https://a.example","https://b.example"]}"#,
        );
        assert!(!urls.is_error);
        assert!(urls.content.contains("page 1 body"));
        assert!(urls.content.contains("page 2 body"));
        let calls = fake.calls.lock().expect("fake web call lock");
        assert_eq!(
            calls.last().map(|call| call.0.as_str()),
            Some("request_many")
        );
        assert_eq!(
            calls
                .last()
                .and_then(|call| call.1.get("requests"))
                .and_then(tea_protocol::JsonValue::as_array)
                .map(|requests| requests.len()),
            Some(2)
        );
    }

    #[test]
    fn todo_is_a_closed_deterministic_single_tool_bundle() {
        let tree = todo(ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 4 * 1024 * 1024,
            max_interrupt_checks: 250_000,
        });
        assert_eq!(tree.extension_id, "todo");
        assert_eq!(
            tree.files.keys().collect::<Vec<_>>(),
            [
                "core.luau",
                "handler.luau",
                "init.luau",
                "manifest.json",
                "prompts.luau"
            ]
        );
        assert_eq!(
            tree.expected_capabilities,
            Some(BTreeSet::from(["extension.state".into()])),
            "the todo extension requests durable state and nothing else"
        );

        let descriptor = LuauExtensionEngine
            .describe(&tree)
            .expect("bundled todo source resolves");
        assert_eq!(
            descriptor.requested_capabilities,
            BTreeSet::from(["extension.state".into()])
        );
        assert_eq!(
            descriptor
                .tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool.capability.as_str()))
                .collect::<Vec<_>>(),
            [("todo", "extension.state")],
            "the model-facing surface is exactly one tool"
        );
        assert_eq!(
            descriptor
                .host_commands
                .iter()
                .map(|command| (command.name.as_str(), command.allowed_while_active))
                .collect::<Vec<_>>(),
            [("/todos", true)],
            "the host surface is exactly one read-only command"
        );
        assert_eq!(descriptor.prompt_sections.len(), 1);
        assert_eq!(descriptor.prompt_sections[0].id, "todo");

        // The schema has no operation enum and no structural fields.
        let schema = &descriptor.tools[0].schema;
        let properties = schema
            .get("properties")
            .and_then(tea_protocol::JsonValue::as_object)
            .expect("todo schema declares properties");
        assert_eq!(
            properties.keys().collect::<Vec<_>>(),
            ["markdown", "updates"]
        );
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&tea_protocol::JsonValue::Bool(false))
        );
        assert!(schema.get("required").is_none(), "empty arguments read");
        let update = properties
            .get("updates")
            .and_then(|updates| updates.get("items"))
            .expect("updates declares its item schema");
        assert_eq!(
            update
                .get("properties")
                .and_then(tea_protocol::JsonValue::as_object)
                .expect("update items declare properties")
                .keys()
                .collect::<Vec<_>>(),
            ["id", "reason", "status"],
            "the model never sees parent_id, ordering, or pagination"
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
