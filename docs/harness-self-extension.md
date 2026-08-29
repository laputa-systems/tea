# Harness self-extension

Tea can evolve a session-local harness without giving a model ambient authority
or an in-place configuration mutation.

HarnessResolver owns immutable trees, snapshots, candidates, and revisions. The
tea_harness host tool accepts one guarded patch plus an explicit
evidence/effect/risk hypothesis. It stages a closed Luau source tree, validates
the resulting snapshot, and records a candidate. A candidate becomes active
only at a safe epoch boundary through a durable HarnessRevisionChangedEntry.

The active epoch remains pinned to the revision selected at its start. A model
cannot replace prompt sections, tools, hooks, or capability bindings midway
through a request. A rejected, invalid, cancelled, or failed candidate leaves
the prior revision active and remains inspectable in retained lineage.

SelfExtensionMode is stored in session metadata before execution begins:

- off exposes no self-extension control tool;
- author permits bounded authoring;
- adaptive permits the host-approved adaptive policy.

All modes use the same v1 ABI and catalog. There is no reload command and no
mutable extension registry outside the durable harness.

The checked-in coding builtins follow this same rule. A candidate can edit a
builtin's Luau source—for example, `read/handler.luau` formatting or schema—and
a new revision becomes visible only to a later epoch. Capability bindings remain
host-owned immutable snapshot data: a changed `read` builtin cannot obtain
workspace mutation or process authority simply by naming another capability.
