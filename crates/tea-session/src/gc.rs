//! Reference-aware immutable artifact garbage collection.
//!
//! Collection is deliberately split into planning and application. A caller
//! first computes roots from one atomic durable session snapshot plus any
//! harness/experiment roots it owns, inspects the resulting plan, and only
//! then asks the object store to remove the exact unreferenced IDs.

use crate::{
    ArtifactError, ArtifactId, ArtifactInventoryItem, ArtifactStore, LaneRecord, SessionSnapshot,
};
use std::collections::{BTreeMap, BTreeSet};

/// Explicit object-store quota selected by a host or experiment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArtifactQuota {
    /// Maximum retained object count, if bounded.
    pub maximum_objects: Option<usize>,
    /// Maximum retained immutable bytes, if bounded.
    pub maximum_bytes: Option<u64>,
}

/// Content-free quota result suitable for a visible diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactQuotaStatus {
    /// Current object count in the store inventory.
    pub object_count: usize,
    /// Current total immutable byte count.
    pub byte_count: u64,
    /// Whether the object-count ceiling is met.
    pub objects_within_limit: bool,
    /// Whether the byte ceiling is met.
    pub bytes_within_limit: bool,
}

impl ArtifactQuotaStatus {
    /// Return whether every configured quota is currently respected.
    pub const fn is_within_limit(&self) -> bool {
        self.objects_within_limit && self.bytes_within_limit
    }
}

/// One reviewed collection plan derived from immutable roots and inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactGcPlan {
    /// Objects reachable from the durable session and caller-supplied roots.
    pub reachable: BTreeSet<ArtifactId>,
    /// Inventory objects that can be removed without crossing those roots.
    pub unreferenced: Vec<ArtifactInventoryItem>,
    /// Content-free inventory/quota state before collection.
    pub quota_status: ArtifactQuotaStatus,
}

/// Result of applying an exact reviewed collection plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactGcReport {
    /// Objects actually removed in deterministic identity order.
    pub removed: Vec<ArtifactInventoryItem>,
    /// Content-free inventory/quota state after collection.
    pub quota_status: ArtifactQuotaStatus,
}

/// Return every direct object reference held by one durable session prefix.
///
/// This includes semantic entry payloads, compaction recovery indexes,
/// provider-response artifacts, and every retained catalog fact. Callers must
/// add transitive harness, experiment, trace, export, or retention roots via
/// `additional_roots`; this function refuses to invent authority over those
/// independent stores.
pub fn session_artifact_roots(snapshot: &SessionSnapshot) -> BTreeSet<ArtifactId> {
    let mut roots = BTreeSet::new();
    for entry in snapshot.entries() {
        roots.extend(entry.body.artifact_references());
    }
    for record in snapshot.records() {
        if let LaneRecord::ProviderRequestSettled(settled) = &record.record
            && let Some(artifact) = settled.response_artifact
        {
            roots.insert(artifact);
        }
    }
    for fact in snapshot.facts() {
        roots.extend(fact.fact.artifact_references());
    }
    roots
}

/// Construct a fail-closed collection plan. Every reachable root must still
/// exist in the store before any unreferenced object is declared removable.
pub fn plan_artifact_gc(
    store: &dyn ArtifactStore,
    snapshot: &SessionSnapshot,
    additional_roots: impl IntoIterator<Item = ArtifactId>,
    quota: ArtifactQuota,
) -> Result<ArtifactGcPlan, ArtifactError> {
    let mut reachable = session_artifact_roots(snapshot);
    reachable.extend(additional_roots);
    let inventory = store.inventory()?;
    let by_id = inventory
        .iter()
        .map(|item| (item.artifact_id, item))
        .collect::<BTreeMap<_, _>>();
    for artifact_id in &reachable {
        if !by_id.contains_key(artifact_id) {
            return Err(ArtifactError::NotFound {
                artifact_id: *artifact_id,
            });
        }
    }
    let unreferenced = inventory
        .iter()
        .filter(|item| !reachable.contains(&item.artifact_id))
        .cloned()
        .collect::<Vec<_>>();
    Ok(ArtifactGcPlan {
        reachable,
        unreferenced,
        quota_status: quota_status(&inventory, quota),
    })
}

/// Apply an already reviewed plan. The plan's roots are rechecked against a
/// fresh inventory immediately before deletion so a stale plan cannot delete
/// an object that became reachable after planning.
pub fn apply_artifact_gc(
    store: &dyn ArtifactStore,
    plan: &ArtifactGcPlan,
    quota: ArtifactQuota,
) -> Result<ArtifactGcReport, ArtifactError> {
    let current = store.inventory()?;
    let current_by_id = current
        .iter()
        .map(|item| (item.artifact_id, item))
        .collect::<BTreeMap<_, _>>();
    for artifact_id in &plan.reachable {
        if !current_by_id.contains_key(artifact_id) {
            return Err(ArtifactError::NotFound {
                artifact_id: *artifact_id,
            });
        }
    }
    let mut removed = Vec::new();
    for planned in &plan.unreferenced {
        let Some(current_item) = current_by_id.get(&planned.artifact_id) else {
            // Another explicit collector already removed an unreferenced
            // object. This plan is no longer exact; fail rather than silently
            // claim a successful deterministic collection.
            return Err(ArtifactError::NotFound {
                artifact_id: planned.artifact_id,
            });
        };
        if current_item.byte_len != planned.byte_len {
            return Err(ArtifactError::Corruption {
                artifact_id: planned.artifact_id,
                message: "artifact byte length changed after GC planning".into(),
            });
        }
        store.remove(planned.artifact_id)?;
        removed.push(planned.clone());
    }
    let after = store.inventory()?;
    Ok(ArtifactGcReport {
        removed,
        quota_status: quota_status(&after, quota),
    })
}

fn quota_status(inventory: &[ArtifactInventoryItem], quota: ArtifactQuota) -> ArtifactQuotaStatus {
    let object_count = inventory.len();
    let byte_count = inventory
        .iter()
        .fold(0_u64, |total, item| total.saturating_add(item.byte_len));
    ArtifactQuotaStatus {
        object_count,
        byte_count,
        objects_within_limit: quota
            .maximum_objects
            .is_none_or(|limit| object_count <= limit),
        bytes_within_limit: quota.maximum_bytes.is_none_or(|limit| byte_count <= limit),
    }
}
