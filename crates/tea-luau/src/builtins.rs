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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, BundleManifest, BUNDLE_ABI_V2_VERSION};
    use crate::{LuaPolicy, LuauExtensionEngine, PolicyError};
    use tea_core::harness::extension::{
        ExtensionCommandInput, ExtensionEngine, ExtensionIdleInput, ExtensionOperationOutcome,
        ExtensionStateView,
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
