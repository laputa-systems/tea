//! The public ABI-v3 helper exercises a bounded private value, command, and idle hook.

use std::collections::{BTreeMap, BTreeSet};
use tea_core::harness::extension::{
    ExtensionCommandInput, ExtensionEngine, ExtensionIdleInput, ExtensionLimits,
    ExtensionOperationOutcome, ExtensionSourceTree, ExtensionStateView,
};
use tea_luau::bundle::{Bundle, BundleManifest, BUNDLE_ABI_V3_VERSION};
use tea_luau::{LuaPolicy, LuauExtensionEngine};
use tea_protocol::JsonValue;

const MANIFEST: &str = include_str!("../examples/run_counter/manifest.json");
const SOURCE: &str = include_str!("../examples/run_counter/init.luau");

fn state(value: &str) -> ExtensionStateView {
    ExtensionStateView {
        value: Some(JsonValue::parse(value).expect("fixture state parses")),
    }
}

#[test]
fn public_run_counter_has_closed_authority_and_saturating_private_state() {
    let source = ExtensionSourceTree {
        extension_id: "run_counter".into(),
        files: BTreeMap::from([
            ("manifest.json".into(), MANIFEST.into()),
            ("init.luau".into(), SOURCE.into()),
        ]),
        expected_capabilities: Some(BTreeSet::from(["extension.state".into()])),
        limits: ExtensionLimits {
            max_source_bytes: 16 * 1024,
            max_memory_bytes: 1024 * 1024,
            max_interrupt_checks: 100_000,
        },
    };
    let descriptor = LuauExtensionEngine
        .describe(&source)
        .expect("public source and manifest resolve");
    assert_eq!(
        descriptor.requested_capabilities,
        BTreeSet::from(["extension.state".into()]),
    );
    assert_eq!(descriptor.state_version.as_deref(), Some("run_counter.v1"));
    assert!(descriptor.tools.is_empty());

    let bundle = Bundle::from_sources(
        BundleManifest::new(BUNDLE_ABI_V3_VERSION, "init.luau", std::iter::empty::<&str>())
            .expect("closed manifest is valid"),
        [("init.luau", SOURCE)],
    )
    .expect("public source is closed");
    let policy = LuaPolicy::load_bundle(bundle).expect("public helper loads");
    assert_eq!(policy.host_commands().len(), 1);
    assert!(policy.has_idle_hook().expect("idle declaration reads"));

    let command = policy
        .execute_host_command(
            "/run-count",
            &ExtensionCommandInput {
                arguments: String::new(),
                state: ExtensionStateView::default(),
            },
        )
        .expect("command runs without ambient authority");
    assert_eq!(command.notice.as_deref(), Some("Completed operations: 0"));
    assert!(command.state.is_none());
    assert!(
        policy
            .execute_host_command(
                "/run-count",
                &ExtensionCommandInput {
                    arguments: String::new(),
                    state: state("{}"),
                },
            )
            .is_err(),
        "an incompatible private value must be rejected instead of reset"
    );

    let completed = policy
        .on_idle(&ExtensionIdleInput {
            operation_id: "operation-1".into(),
            outcome: ExtensionOperationOutcome::Completed,
            usage: Default::default(),
            elapsed_active_seconds: 1,
            state: state("999"),
        })
        .expect("idle hook runs");
    let next = completed.state.expect("completed operation replaces state").value;
    assert_eq!(next, JsonValue::parse("1000").unwrap());

    let saturated = policy
        .on_idle(&ExtensionIdleInput {
            operation_id: "operation-2".into(),
            outcome: ExtensionOperationOutcome::Completed,
            usage: Default::default(),
            elapsed_active_seconds: 1,
            state: ExtensionStateView {
                value: Some(next.clone()),
            },
        })
        .expect("saturated hook runs");
    assert_eq!(saturated.state.expect("state remains bounded").value, next);

    let aborted = policy
        .on_idle(&ExtensionIdleInput {
            operation_id: "operation-3".into(),
            outcome: ExtensionOperationOutcome::Aborted,
            usage: Default::default(),
            elapsed_active_seconds: 1,
            state: state("42"),
        })
        .expect("aborted hook runs");
    assert!(aborted.state.is_none());
}
