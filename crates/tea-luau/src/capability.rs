//! Versioned, host-controlled capabilities exposed to Luau extensions.
//!
//! This module describes authority; it does not acquire authority.  A host
//! parses a [`CapabilityManifest`], installs only the providers it intends to
//! expose, and uses [`CapabilityGate`] at every call boundary.  The manifest
//! is deliberately represented with the dependency-free protocol
//! [`tea_protocol::JsonValue`] so that it can be persisted, hashed, and exchanged without
//! binding the ABI to `mlua`, an executor, or a provider SDK.
//!
//! The canonical JSON shape is:
//!
//! ```json
//! {
//!   "abi_version": 1,
//!   "agent": ["events", "tools", "stop"],
//!   "world": [
//!     "fs.read",
//!     {"mcp": {"server": "fixture-world", "operation": "call", "target": "execute_code"}}
//!   ],
//!   "trace": ["emit"]
//! }
//! ```
//!
//! Missing module members mean no grant. There are no wildcard operations or
//! target fallbacks, and an MCP grant without a server is rejected. An omitted
//! MCP target matches only a request that also omits its target.

mod domain;
mod gate;
mod manifest;

pub use domain::{
    AgentOperation, CapabilityError, CapabilityGrant, CapabilityModule, CapabilityOperation,
    CapabilityProvider, CapabilityProviderError, CapabilityRequest, CapabilityResponse,
    JsonOperation, McpOperation, McpPermission, TaskOperation, TimeOperation, TraceOperation,
    WorldOperation, CAPABILITY_ABI_VERSION,
};
pub use gate::CapabilityGate;
pub use manifest::CapabilityManifest;

#[cfg(test)]
use tea_protocol::JsonValue;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn world_mcp(target: Option<&str>) -> CapabilityOperation {
        WorldOperation::mcp("fixture-world", McpOperation::Call, target)
            .expect("test MCP permission")
            .into()
    }

    #[test]
    fn manifest_round_trips_to_deterministic_json() {
        let manifest = CapabilityManifest::new([
            CapabilityGrant::new(
                CapabilityModule::World,
                [
                    CapabilityOperation::World(WorldOperation::Exec),
                    world_mcp(Some("execute_code")),
                    CapabilityOperation::World(WorldOperation::FsRead),
                ],
            )
            .expect("valid world grant"),
            CapabilityGrant::new(
                CapabilityModule::Agent,
                [CapabilityOperation::Agent(AgentOperation::Stop)],
            )
            .expect("valid agent grant"),
        ])
        .expect("valid manifest");

        let encoded = manifest.to_json_string().expect("manifest encodes");
        assert_eq!(
            encoded,
            r#"{"abi_version":1,"agent":["stop"],"world":["fs.read","exec",{"mcp":{"operation":"call","server":"fixture-world","target":"execute_code"}}]}"#
        );
        assert_eq!(
            CapabilityManifest::parse_json(&encoded).expect("manifest decodes"),
            manifest
        );
    }

    #[test]
    fn request_is_denied_without_an_exact_module_and_operation_grant() {
        let manifest = CapabilityManifest::new([CapabilityGrant::new(
            CapabilityModule::World,
            [CapabilityOperation::World(WorldOperation::FsRead)],
        )
        .expect("valid grant")])
        .expect("valid manifest");

        let request = CapabilityRequest::new(
            CapabilityOperation::World(WorldOperation::FsWrite),
            JsonValue::Null,
        );
        assert!(matches!(
            manifest.check(&request),
            Err(CapabilityError::Denied {
                module: CapabilityModule::World,
                ..
            })
        ));
    }

    #[test]
    fn mcp_permissions_are_server_and_target_scoped() {
        let manifest = CapabilityManifest::parse_json(
            r#"{"abi_version":1,"world":[{"mcp":{"server":"fixture-world","operation":"call","target":"execute_code"}}]}"#,
        )
        .expect("valid MCP manifest");

        let allowed = CapabilityRequest::new(world_mcp(Some("execute_code")), JsonValue::Null);
        let wrong_tool = CapabilityRequest::new(world_mcp(Some("read_state")), JsonValue::Null);
        let wrong_server = CapabilityRequest::new(
            WorldOperation::mcp("other", McpOperation::Call, Some("execute_code"))
                .expect("test MCP permission")
                .into(),
            JsonValue::Null,
        );
        assert!(manifest.check(&allowed).is_ok());
        assert!(manifest.check(&wrong_tool).is_err());
        assert!(manifest.check(&wrong_server).is_err());
    }

    #[test]
    fn mcp_targetless_grants_do_not_authorize_other_targets() {
        let manifest = CapabilityManifest::new([CapabilityGrant::new(
            CapabilityModule::World,
            [world_mcp(None)],
        )
        .expect("valid targetless grant")])
        .expect("valid manifest");

        let targetless = CapabilityRequest::new(world_mcp(None), JsonValue::Null);
        let targeted = CapabilityRequest::new(world_mcp(Some("execute_code")), JsonValue::Null);
        assert!(manifest.check(&targetless).is_ok());
        assert!(manifest.check(&targeted).is_err());
    }

    #[test]
    fn malformed_grants_reject_unknown_modules_operations_and_fields() {
        for input in [
            r#"{"abi_version":1,"filesystem":["read"]}"#,
            r#"{"abi_version":1,"world":["network"]}"#,
            r#"{"abi_version":1,"world":[{"mcp":{"server":"fixture-world","operation":"call","unexpected":true}}]}"#,
            r#"{"abi_version":1,"world":[{"mcp":{"server":"","operation":"call"}}]}"#,
        ] {
            assert!(
                CapabilityManifest::parse_json(input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn duplicate_grants_and_operations_are_rejected() {
        let duplicate_grant = CapabilityManifest::new([
            CapabilityGrant::new(
                CapabilityModule::Agent,
                [CapabilityOperation::Agent(AgentOperation::Events)],
            )
            .expect("valid grant"),
            CapabilityGrant::new(
                CapabilityModule::Agent,
                [CapabilityOperation::Agent(AgentOperation::Stop)],
            )
            .expect("valid grant"),
        ]);
        assert!(matches!(
            duplicate_grant,
            Err(CapabilityError::DuplicateGrant {
                module: CapabilityModule::Agent
            })
        ));

        let duplicate_operation = CapabilityGrant::new(
            CapabilityModule::Agent,
            [
                CapabilityOperation::Agent(AgentOperation::Events),
                CapabilityOperation::Agent(AgentOperation::Events),
            ],
        );
        assert!(matches!(
            duplicate_operation,
            Err(CapabilityError::DuplicateOperation {
                module: CapabilityModule::Agent,
                ..
            })
        ));
        assert!(
            CapabilityManifest::parse_json(r#"{"abi_version":1,"agent":["events","events"]}"#)
                .is_err()
        );
    }

    struct CountingProvider(AtomicUsize);

    impl CapabilityProvider for CountingProvider {
        fn provide(
            &self,
            _request: &CapabilityRequest,
        ) -> Result<CapabilityResponse, CapabilityProviderError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(CapabilityResponse::new(JsonValue::Bool(true)))
        }
    }

    #[test]
    fn gate_checks_before_provider_and_returns_structured_result() {
        let provider = CountingProvider(AtomicUsize::new(0));
        let gate = CapabilityGate::new(
            CapabilityManifest::new([CapabilityGrant::new(
                CapabilityModule::Agent,
                [CapabilityOperation::Agent(AgentOperation::Stop)],
            )
            .expect("valid grant")])
            .expect("valid manifest"),
            provider,
        );
        let request = CapabilityRequest::new(
            CapabilityOperation::Agent(AgentOperation::Stop),
            JsonValue::Null,
        );
        assert_eq!(
            gate.provide(&request).expect("authorized request").value,
            JsonValue::Bool(true)
        );
        assert_eq!(gate.provider.0.load(Ordering::Relaxed), 1);

        let denied = CapabilityRequest::new(
            CapabilityOperation::Agent(AgentOperation::Events),
            JsonValue::Null,
        );
        assert!(gate.provide(&denied).is_err());
        assert_eq!(gate.provider.0.load(Ordering::Relaxed), 1);
    }
}
