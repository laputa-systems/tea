use std::sync::Arc;
use tea_core::harness::extension::NoExtensions;
use tea_core::harness::{
    HarnessActor, HarnessResolver, HarnessResourceLimits, HarnessSeedBuilder, ModelHarnessProfile,
    SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode,
};
use tea_core::runtime::{
    HarnessIdentity, IdleAuthorization, RuntimeServices, SessionSupervisor, SessionSupervisorInput,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason};
use tea_core::tool::ToolRegistry;
use tea_session::{
    Digest, EntryId, HarnessRevisionChangedEntry, LaneId, MemoryArtifactStore, MemorySession,
    ProvisionedEntry, SessionEntry, SessionHeader, SessionId, SessionWriter,
};

struct ScriptedProvider;

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("Hello from one shared execution engine.".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifacts = Arc::new(MemoryArtifactStore::default());
    let services = RuntimeServices::new(Arc::new(ScriptedProvider), ToolRegistry::default()).model(
        ModelDescriptor {
            provider: "scripted".into(),
            model: "example".into(),
            revision: None,
        },
    );
    let profile = ModelHarnessProfile::new(
        "scripted",
        "example",
        None,
        "example-prompt",
        "no-tools",
        "no-compaction",
        "default-projection",
    )?;
    let seeded = HarnessSeedBuilder::new(
        artifacts.clone(),
        Arc::new(NoExtensions),
        Digest::from_bytes("session-example"),
        "Reply concisely.",
        profile,
        SelfExtensionMode::Off,
        HarnessResourceLimits::default(),
        services.runtime_policy_identities(),
    )
    .seed(HarnessActor::Host, 1)?;
    let identity = HarnessIdentity::new(
        seeded.revision.revision_id.clone(),
        seeded.snapshot.id.clone(),
        seeded.profile.profile_id,
    );
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("example-session")?,
        "explicit-synthetic-workspace",
        [(
            SELF_EXTENSION_MODE_METADATA_KEY.into(),
            SelfExtensionMode::Off.metadata_value(),
        )]
        .into_iter()
        .collect(),
    ))?;
    session.append_entry(
        &LaneId::main(),
        ProvisionedEntry {
            id: EntryId::new("initial-harness")?,
            body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                revision_id: seeded.revision.revision_id,
                snapshot_id: seeded.snapshot.id,
                rollback_from: None,
            }),
        },
    )?;
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: Arc::new(HarnessResolver::new(seeded.repository, Default::default())),
        root_identity: identity,
        root_services: services,
        artifacts,
        rollover_budget: 0,
        subagents: None,
    })?;
    let input = runtime.submit_input("Say hello.")?;
    smol::block_on(async {
        runtime
            .drive_next_input(IdleAuthorization::UserInputOnly)
            .await?;
        let result = input.completion().wait().await;
        println!("{result:?}");
        runtime.close().await?;
        Ok::<(), tea_core::harness::HarnessError>(())
    })?;
    for entry in runtime.snapshot()?.entries() {
        if let SessionEntry::AssistantMessage(message) = &entry.body {
            println!("{}", message.content);
        }
    }
    Ok(())
}
