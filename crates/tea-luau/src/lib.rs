//! Hermetic Luau policy support for tea-core.
//!
//! A policy declares prompt additions, prompt-facing tool definitions, and a
//! narrow pre-tool decision. It cannot acquire ambient process, network, file,
//! or MCP authority; a host binds each declared capability explicitly.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::result_large_err)]

/// Caller-driven coroutine support for explicit asynchronous capabilities.
pub mod async_runtime;
/// Checked-in immutable extension source trees bundled with Tea.
pub mod builtins;
/// Closed, deterministic source bundles and their manifests.
pub mod bundle;
/// Per-VM execution of closed bundle-local Luau modules.
pub mod bundle_runtime;
/// Versioned, capability-scoped extension ABI values and host gates.
pub mod capability;
/// Luau implementation of the core-owned extension engine contract.
pub mod extension_engine;
/// Coroutine-backed Luau tool handlers adapted to the core tool scheduler.
pub mod tool_handler;

mod policy;
pub use extension_engine::LuauExtensionEngine;
pub use policy::{
    CollectedPolicyMemoryProposal, LuaPolicy, LuaPolicyHookSet, PolicyAfterToolOutput,
    PolicyContextAnnotation, PolicyContextEntry, PolicyContextInput, PolicyContextProjectionPatch,
    PolicyError, PolicyLimits, PolicyMemoryCollector, PolicyMemoryProposal, PolicyMemoryRetention,
    PolicyMemoryVisibility, PolicyPromptSection, PolicyTool,
};

#[cfg(test)]
mod tests {
    use super::{
        LuaPolicy, LuaPolicyHookSet, PolicyContextEntry, PolicyContextInput, PolicyError,
        PolicyLimits, PolicyPromptSection,
    };
    use crate::bundle::{Bundle, BundleManifest, BUNDLE_ABI_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tea_core::error::HookError;
    use tea_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet, Replacement};
    use tea_core::state::{SerializedJson, ToolCallId, Usage};
    use tea_core::tool::CancellationSettlementMode;
    use tea_core::tool::{AgentToolResult, ToolCall};

    const GAME_POLICY: &str = r#"
        return {
            prompt_sections = {
                { id = "game", content = "Use game tools deliberately." },
            },
            tools = {
                {
                    name = "execute_code",
                    description = "Execute a game script.",
                    capability = "rs-agent",
                    execution_mode = "sequential",
                    schema_json = '{"type":"object","required":["code"]}',
                },
            },
            before_tool = function(call)
                if call.name == "execute_code" then
                    return "allow"
                end
                return { action = "block", reason = "not granted by game policy" }
            end,
        }
    "#;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(format!("call-{name}")).expect("test IDs are non-empty"),
            name: name.to_owned(),
            arguments: SerializedJson::new("{}"),
        }
    }

    #[test]
    fn policy_declares_prompt_tools_and_pre_tool_boundary() {
        let policy = LuaPolicy::load(GAME_POLICY).expect("policy should load");

        assert_eq!(
            policy.prompt_sections(),
            &[PolicyPromptSection {
                id: "game".into(),
                content: "Use game tools deliberately.".into(),
            }]
        );
        assert_eq!(policy.tools().len(), 1);
        assert_eq!(policy.tools()[0].name, "execute_code");
        assert_eq!(policy.tools()[0].capability, "rs-agent");
        assert!(!policy.tools()[0].requires_exclusive_batch);
        assert_eq!(
            policy.tools()[0].cancellation_settlement_mode,
            CancellationSettlementMode::DropFuture
        );
        assert_eq!(
            policy.before_tool_call(&call("execute_code")),
            Ok(BeforeToolCall::Allow)
        );
        assert_eq!(
            policy.before_tool_call(&call("read_resource")),
            Ok(BeforeToolCall::Block {
                reason: "not granted by game policy".to_owned(),
            })
        );
    }

    #[test]
    fn policy_tool_execution_safety_fields_are_typed_and_bounded() {
        let policy = LuaPolicy::load(
            r#"
                return {
                    prompt_sections = {},
                    tools = {
                        {
                            name = "transaction",
                            description = "Commit one transaction.",
                            capability = "world.write",
                            execution_mode = "parallel",
                            requires_exclusive_batch = true,
                            cancellation_settlement_mode = "await_future",
                            schema_json = '{"type":"object"}',
                        },
                    },
                }
            "#,
        )
        .expect("execution policy fields should parse");
        let tool = &policy.tools()[0];
        assert!(tool.requires_exclusive_batch);
        assert_eq!(
            tool.cancellation_settlement_mode,
            CancellationSettlementMode::AwaitFuture
        );
    }

    #[test]
    fn luau_syntax_runs_in_the_jit_enabled_policy_vm() {
        let policy = LuaPolicy::load(
            r#"
                local permitted: boolean = true
                return {
                    prompt_sections = {
                        { id = "jit", content = if permitted then "Luau policy" else "unreachable" },
                    },
                    before_tool = function(_) return "allow" end,
                }
            "#,
        )
        .expect("Luau type annotations and if-expressions should compile");

        assert_eq!(policy.prompt_sections()[0].content, "Luau policy");
        assert_eq!(
            policy.before_tool_call(&call("execute_code")),
            Ok(BeforeToolCall::Allow)
        );
    }

    #[test]
    fn policy_bundle_resolves_only_its_closed_relative_module_graph() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("manifest is valid"),
            [
                (
                    "main.luau",
                    r#"
                        local prompt = require("./parts/prompt.luau")
                        return {
                            prompt_sections = { { id = "closed", content = prompt } },
                            before_tool = function(_) return "allow" end,
                        }
                    "#,
                ),
                ("parts/prompt.luau", "return 'closed bundle policy'"),
            ],
        )
        .expect("closed bundle is valid");

        let policy = LuaPolicy::load_bundle(bundle).expect("closed bundle should load");
        assert_eq!(policy.prompt_sections()[0].content, "closed bundle policy");
        assert_eq!(
            policy.before_tool_call(&call("tool")),
            Ok(BeforeToolCall::Allow)
        );
    }

    #[test]
    fn v1_bundle_exposes_named_prompt_sections() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {
                        {
                            id = "verification",
                            content = "Run the narrowest relevant validator before finalizing.",
                        },
                    },
                    before_tool = function(_) return "allow" end,
                }
            "#,
            )],
        )
        .expect("v1 bundle is closed");

        let policy = LuaPolicy::load_bundle(bundle).expect("v1 policy must load");
        assert_eq!(
            policy.prompt_sections(),
            &[PolicyPromptSection {
                id: "verification".into(),
                content: "Run the narrowest relevant validator before finalizing.".into(),
            }],
        );
        assert_eq!(
            policy.before_tool_call(&call("inspect")),
            Ok(BeforeToolCall::Allow)
        );
    }

    #[test]
    fn v1_bundle_rejects_duplicate_named_prompt_sections() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {
                        { id = "verification", content = "first" },
                        { id = "verification", content = "second" },
                    },
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");

        assert!(matches!(
            LuaPolicy::load_bundle(bundle),
            Err(PolicyError::Contract { .. })
        ));
    }

    #[test]
    fn v1_before_tool_can_normalize_arguments_without_bypassing_core_validation() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {},
                    before_tool = function(call)
                        if call.name == "inspect" then
                            return {
                                action = "normalize",
                                arguments_json = '{"scope":"targeted"}',
                            }
                        end
                        return "allow"
                    end,
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");

        let policy = LuaPolicy::load_bundle(bundle).expect("v1 policy must load");
        assert_eq!(
            policy.before_tool_call(&call("inspect")),
            Ok(BeforeToolCall::Normalize {
                arguments: SerializedJson::new(r#"{"scope":"targeted"}"#),
            }),
        );
    }

    #[test]
    fn v1_after_tool_projects_only_model_visible_fields_and_annotations() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {},
                    after_tool = function(_, result)
                        assert(result.usage == nil)
                        return {
                            content = "bounded projection: " .. result.content,
                            is_error = true,
                            terminate = true,
                            recovery_hint = "read the retained raw artifact",
                            annotations_json = '{"kind":"fixture"}',
                        }
                    end,
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");
        let policy = LuaPolicy::load_bundle(bundle).expect("v1 policy must load");
        let raw = AgentToolResult {
            tool_call_id: call("inspect").id,
            content: "raw tool output".into(),
            details: Some(SerializedJson::new(r#"{"raw":true}"#)),
            usage: Some(Usage {
                output_tokens: Some(11),
                ..Usage::default()
            }),
            added_tool_names: vec!["host-only".into()],
            terminate: false,
            is_error: false,
            failure: None,
        };

        let projection = policy
            .after_tool_call(&call("inspect"), &raw)
            .expect("projection should be accepted");
        assert_eq!(
            projection.content,
            Replacement::Replace("bounded projection: raw tool output".into())
        );
        assert_eq!(projection.is_error, Replacement::Replace(true));
        assert_eq!(projection.terminate, Some(true));
        assert_eq!(
            projection.details,
            Replacement::Replace(Some(SerializedJson::new(
                r#"{"annotations":{"kind":"fixture"},"recovery_hint":"read the retained raw artifact"}"#
            )))
        );
        assert_eq!(projection.usage, Replacement::Keep);
        assert_eq!(projection.failure, Replacement::Keep);
        assert_eq!(projection.added_tool_names, Replacement::Keep);
    }

    #[test]
    fn v1_after_tool_rejects_unknown_behavior_changing_fields() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {},
                    after_tool = function(_, _)
                        return { usage = 999 }
                    end,
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");
        let policy = LuaPolicy::load_bundle(bundle).expect("v1 policy must load");
        let raw = AgentToolResult {
            tool_call_id: call("inspect").id,
            content: "raw".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        };

        assert!(matches!(
            policy.after_tool_call(&call("inspect"), &raw),
            Err(PolicyError::Contract { .. })
        ));
    }

    #[test]
    fn v1_context_projection_receives_metadata_only_and_returns_a_bounded_patch() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {},
                    context_projection = function(context)
                        assert(#context.entries == 2)
                        assert(context.entries[1].content == nil)
                        assert(context.entries[1].id == "entry-root")
                        assert(context.entries[1].protected == true)
                        return {
                            omit_eligible_entries = { "entry-tool" },
                            selected_memory = { "entry-memory" },
                            annotations = {
                                { id = "selection", content = "Use the retained root." },
                            },
                            requested_compaction_strategy = "bounded-summary",
                        }
                    end,
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");
        let policy = LuaPolicy::load_bundle(bundle).expect("context policy loads");
        let patch = policy
            .context_projection(&PolicyContextInput {
                entries: vec![
                    PolicyContextEntry {
                        id: "entry-root".into(),
                        kind: "user".into(),
                        model_visible: true,
                        protected: true,
                    },
                    PolicyContextEntry {
                        id: "entry-tool".into(),
                        kind: "tool".into(),
                        model_visible: true,
                        protected: false,
                    },
                ],
            })
            .expect("bounded context patch parses");
        assert_eq!(patch.omit_eligible_entries, vec!["entry-tool"]);
        assert_eq!(patch.selected_memory, vec!["entry-memory"]);
        assert_eq!(patch.annotations.len(), 1);
        assert_eq!(patch.annotations[0].id, "selection");
        assert_eq!(
            patch.requested_compaction_strategy.as_deref(),
            Some("bounded-summary")
        );
    }

    #[test]
    fn v1_resume_hooks_persist_bounded_state_and_receive_only_their_own_values() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("v1 manifest is valid"),
            [(
                "main.luau",
                r#"
                return {
                    prompt_sections = {},
                    resume_hooks = {
                        {
                            id = "first",
                            before_operation = function()
                                return '{"owner":"first"}'
                            end,
                            before_epoch = function()
                                return '{"epoch":"first"}'
                            end,
                            before_resume = function(state)
                                assert(state.operation.owner == "first")
                                assert(state.operation.other == nil)
                                assert(state.epoch.epoch == "first")
                            end,
                        },
                        {
                            id = "second",
                            before_operation = function()
                                return '{"owner":"second","other":"private"}'
                            end,
                            before_epoch = function()
                                return '{"epoch":"second"}'
                            end,
                            before_resume = function(state)
                                assert(state.operation.owner == "second")
                                assert(state.epoch.epoch == "second")
                            end,
                        },
                    },
                }
            "#,
            )],
        )
        .expect("v1 source tree is closed");
        let policy = LuaPolicy::load_bundle(bundle).expect("v1 policy must load");

        let operation = policy
            .before_operation_resume_data()
            .expect("operation hook state is valid");
        let epoch = policy
            .before_epoch_resume_data()
            .expect("epoch hook state is valid");
        assert_eq!(
            operation.get("first"),
            Some(&tea_protocol::JsonValue::parse(r#"{"owner":"first"}"#).unwrap())
        );
        assert_eq!(
            operation.get("second"),
            Some(
                &tea_protocol::JsonValue::parse(r#"{"other":"private","owner":"second"}"#).unwrap()
            )
        );
        assert_eq!(
            epoch.get("first"),
            Some(&tea_protocol::JsonValue::parse(r#"{"epoch":"first"}"#).unwrap())
        );

        policy
            .before_resume(&operation, &epoch)
            .expect("each resume hook sees only its own durable state");
    }

    #[test]
    fn policy_bundle_applies_source_limit_to_every_module() {
        let bundle = Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .expect("manifest is valid"),
            [
                ("main.luau", "return require('./prompt.luau')"),
                (
                    "prompt.luau",
                    "return { prompt_sections = { { id = 'bounded', content = 'large enough to exceed limit' } } }",
                ),
            ],
        )
        .expect("closed bundle is valid");
        let result = LuaPolicy::load_bundle_with_limits(
            bundle,
            PolicyLimits {
                max_source_bytes: 8,
                ..PolicyLimits::default()
            },
        );
        let error = match result {
            Ok(_) => panic!("a non-entrypoint source cannot evade the aggregate bound"),
            Err(error) => error,
        };
        assert!(matches!(error, PolicyError::SourceTooLarge { .. }));
    }

    #[test]
    fn sandbox_has_no_ambient_os_or_module_loader() {
        for source in [
            "return { prompt_sections = { { id = 'ambient', content = os.time() } } }",
            "return { prompt_sections = { { id = 'ambient', content = require('filesystem') } } }",
        ] {
            assert!(
                LuaPolicy::load(source).is_err(),
                "source should be rejected: {source}"
            );
        }
    }

    #[test]
    fn interrupt_budget_terminates_an_unbounded_hook() {
        let policy = LuaPolicy::load_with_limits(
            r#"
                return {
                    prompt_sections = {},
                    before_tool = function(_) while true do end end,
                }
            "#,
            PolicyLimits {
                max_source_bytes: 4 * 1024,
                max_memory_bytes: 1024 * 1024,
                max_interrupt_checks: 2,
            },
        )
        .expect("policy source should load without executing the hook");

        let error = policy
            .before_tool_call(&call("execute_code"))
            .expect_err("unbounded hook must be interrupted");
        assert!(matches!(error, PolicyError::Runtime { .. }));
    }

    #[test]
    fn source_limit_is_checked_before_vm_evaluation() {
        let error = LuaPolicy::load_with_limits(
            GAME_POLICY,
            PolicyLimits {
                max_source_bytes: 8,
                ..PolicyLimits::default()
            },
        )
        .err()
        .expect("oversized source should not enter the VM");

        assert!(matches!(error, PolicyError::SourceTooLarge { .. }));
    }

    #[test]
    fn duplicate_tool_names_are_rejected_before_host_binding() {
        let error = LuaPolicy::load(
            r#"
                return {
                    prompt_sections = {},
                    tools = {
                        {
                            name = "inspect",
                            description = "First declaration.",
                            capability = "world",
                            execution_mode = "parallel",
                            schema_json = "{}",
                        },
                        {
                            name = "inspect",
                            description = "Second declaration.",
                            capability = "world",
                            execution_mode = "parallel",
                            schema_json = "{}",
                        },
                    },
                }
            "#,
        )
        .err()
        .expect("a policy must not shadow a tool binding");

        assert!(matches!(error, PolicyError::Contract { .. }));
    }

    #[test]
    fn policy_retains_optional_tool_handler_source_without_granting_authority() {
        let policy = LuaPolicy::load(
            r#"
                return {
                    prompt_sections = {},
                    tools = {
                        {
                            name = "world_echo",
                            description = "Echo through an explicit host capability.",
                            capability = "world",
                            execution_mode = "sequential",
                            schema_json = "{}",
                            handler_source = "return function(call) return call.arguments_json end",
                        },
                    },
                }
            "#,
        )
        .expect("a declaration may retain handler source");

        assert_eq!(
            policy.tools()[0].handler_source.as_deref(),
            Some("return function(call) return call.arguments_json end")
        );
        let error = match LuaPolicy::load(
            r#"
                return {
                    prompt_sections = {},
                    tools = {{
                        name = "empty_handler",
                        description = "Invalid handler.",
                        capability = "world",
                        execution_mode = "sequential",
                        schema_json = "{}",
                        handler_source = "  ",
                    }},
                }
            "#,
        ) {
            Ok(_) => panic!("an explicitly empty handler has no executable contract"),
            Err(error) => error,
        };
        assert!(matches!(error, PolicyError::Contract { .. }));
    }

    #[test]
    fn policy_denial_does_not_reach_the_host_hook() {
        let policy = Arc::new(LuaPolicy::load(GAME_POLICY).expect("policy should load"));
        let calls = Arc::new(AtomicUsize::new(0));
        let hooks = LuaPolicyHookSet::new(
            policy,
            Arc::new(CountingHooks {
                before_calls: Arc::clone(&calls),
            }),
        );

        assert!(matches!(
            hooks.before_tool_call(&call("read_resource")),
            Ok(BeforeToolCall::Block { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            hooks.before_tool_call(&call("execute_code")),
            Ok(BeforeToolCall::Allow)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    struct CountingHooks {
        before_calls: Arc<AtomicUsize>,
    }

    impl HookSet for CountingHooks {
        fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
            self.before_calls.fetch_add(1, Ordering::Relaxed);
            Ok(BeforeToolCall::Allow)
        }

        fn after_tool_call(
            &self,
            _call: &ToolCall,
            _result: &AgentToolResult,
        ) -> Result<AfterToolCall, HookError> {
            Ok(AfterToolCall::default())
        }

        fn transform_context(
            &self,
            context: ContextEnvelope,
        ) -> Result<ContextEnvelope, HookError> {
            Ok(context)
        }

        fn convert_to_llm(&self, _context: ContextEnvelope) -> Result<String, HookError> {
            Ok("[]".to_owned())
        }
    }
}
