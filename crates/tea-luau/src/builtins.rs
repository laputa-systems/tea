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

/// Return the exact bundled web-retrieval extension source tree.
///
/// Firecrawl protocol and output policy remain in the checked-in Luau source;
/// the host independently decides whether to grant its route-scoped generic
/// `network.http` capability.
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
        ExtensionCapabilityBindings, ExtensionCapabilityError, ExtensionCapabilityFuture,
        ExtensionCapabilityRequest, ExtensionCapabilityResponse, ExtensionCommandInput,
        ExtensionEngine, ExtensionIdleInput, ExtensionMemoryCollector, ExtensionOperationOutcome,
        ExtensionStateView,
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
            .contains("separate `write`, `grep`, or `ls` tools"));
        let edit = descriptor
            .tools
            .iter()
            .find(|tool| tool.name == "edit")
            .expect("edit declaration exists");
        assert!(edit.description.contains("parent directory must already exist"));
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
        let find = descriptor
            .tools
            .iter()
            .find(|tool| tool.name == "find")
            .expect("find declaration exists");
        let find_properties = find
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
        assert_eq!(edit.content, "Created 1 file.");
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
        let bounded_find = block_on(
            resolved
                .tools
                .get("find")
                .expect("find is resolved")
                .execute(
                    call("find-bounded", r#"{"pattern":"*.txt","limit":1}"#),
                    context.clone(),
                    ToolUpdateSink::disabled(),
                ),
        )
        .expect("checked-in bounded find handler executes");
        assert!(bounded_find.content.ends_with("[1 results limit reached]"));
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

    #[test]
    fn coding_edit_receipt_distinguishes_precise_edits_existing_files_and_creations() {
        static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);
        let workspace = std::env::temp_dir().join(format!(
            "tea-luau-coding-edit-receipt-{}-{}",
            std::process::id(),
            NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&workspace).expect("coding fixture workspace creates");
        std::fs::write(workspace.join("precise.txt"), "before\n")
            .expect("precise fixture writes");
        std::fs::write(workspace.join("complete.txt"), "before\n")
            .expect("complete fixture writes");
        std::fs::write(workspace.join("mixed-precise.txt"), "before\n")
            .expect("mixed precise fixture writes");
        std::fs::write(workspace.join("mixed-complete.txt"), "before\n")
            .expect("mixed complete fixture writes");
        std::fs::write(workspace.join("no-op.txt"), "unchanged\n")
            .expect("no-op fixture writes");

        let limits = ExtensionLimits {
            max_source_bytes: 64 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 10_000,
        };
        let tree = coding(limits);
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
                    let query = request
                        .arguments
                        .get("json")
                        .and_then(|json| json.get("query"))
                        .and_then(tea_protocol::JsonValue::as_str);
                    if query == Some("rate") {
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
                                        tea_protocol::JsonValue::Array(vec![tea_protocol::JsonValue::object([
                                            ("url", tea_protocol::JsonValue::String("https://large.example".into())),
                                            ("title", tea_protocol::JsonValue::String("Large".into())),
                                            ("markdown", tea_protocol::JsonValue::String("é".repeat(20_000))),
                                        ])]),
                                    )]),
                                ),
                            ]),
                        )
                    } else if query == Some("repair") {
                        response_json(
                            r#"{"success":true,"data":{"web":[{"url":"https://one.example","title":"One","markdown":"first"},{"url":"https://two.example","title":"Two"},{"url":"https://three.example","title":"Three"}]}}"#,
                        )
                    } else {
                        response_json(
                            r##"{"success":true,"data":{"web":[{"url":"https://docs.example","title":"Documentation","markdown":"# Evidence\nactual source"}]}}"##,
                        )
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
                                if url.contains("fail") {
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
            Box::pin(async move {
                Ok(ExtensionCapabilityResponse { value: response })
            })
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
            ["handler_source.luau", "init.luau", "manifest.json", "prompts.luau"]
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
            block_on(
                resolved
                    .tools
                    .get("web")
                    .expect("web resolves")
                    .execute(
                        ToolCall {
                            id: ToolCallId::new(id).expect("call ID is valid"),
                            name: "web".into(),
                            arguments: SerializedJson::new(arguments),
                        },
                        context.clone(),
                        ToolUpdateSink::disabled(),
                    ),
            )
            .expect("web handler executes")
        };

        let search = execute("web-search", r#"{"query":"rustls defaults"}"#);
        assert!(!search.is_error);
        assert!(search.content.contains("Mode: developer"));
        assert!(search.content.contains("BEGIN UNTRUSTED WEB CONTENT"));
        let calls = fake.calls.lock().expect("fake web call lock");
        assert_eq!(calls.len(), 1, "search with Markdown needs no follow-up scrape");
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

        let general = execute("web-general", r#"{"query":"general","kind":"web"}"#);
        assert!(!general.is_error);
        let calls = fake.calls.lock().expect("fake web call lock");
        let general_json = calls
            .last()
            .and_then(|call| call.1.get("json"))
            .expect("general search JSON exists");
        assert!(general_json.get("categories").is_none());
        drop(calls);

        let invalid = execute(
            "web-invalid",
            r#"{"query":"foo","action":"search"}"#,
        );
        assert!(invalid.is_error);
        assert!(invalid.content.contains("accepts only query"));

        let partial = execute(
            "web-partial",
            r#"{"urls":["https://one.example","https://fail.example","https://three.example"]}"#,
        );
        assert!(!partial.is_error);
        let first = partial.content.find("[1] Page 1").expect("first source remains first");
        let failed = partial.content.find("[2] FAILED").expect("failure is represented in place");
        let third = partial.content.find("[3] Page 3").expect("later source retains input index");
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
        assert_eq!(calls.last().map(|call| call.0.as_str()), Some("request_many"));
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
