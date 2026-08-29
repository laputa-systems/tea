//! Behavioral contract for the bundled `todo` extension.
//!
//! The todo state machine lives entirely in Luau, so these tests drive the
//! real bundled source through the real extension engine and a real
//! `extension.state` capability. They assert model-facing results, durable
//! state, and the activity projection published on every call.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tea_core::effect::RunProvenance;
use tea_core::harness::extension::{
    ExtensionCapabilityBindings, ExtensionCommandInput, ExtensionEngine, ExtensionError,
    ExtensionLimits, ExtensionMemoryCollector, ExtensionStateHandle, ExtensionStateStore,
    ExtensionStateUpdate, ExtensionStateView, ResolvedExtension,
};
use tea_core::harness::ExtensionStateCapability;
use tea_core::hooks::NoHooks;
use tea_core::state::{SerializedJson, ToolCallId};
use tea_core::tool::{ToolCall, ToolContext, ToolUpdate, ToolUpdateSink};
use tea_luau::LuauExtensionEngine;
use tea_protocol::JsonValue;

/// Session-shaped state store: latest value per kind, exactly like the durable
/// extension-state reduction the supervisor performs.
#[derive(Default)]
struct MemoryStateStore {
    latest: Mutex<BTreeMap<String, JsonValue>>,
}

impl ExtensionStateStore for MemoryStateStore {
    fn read_extension_state(
        &self,
        _extension_id: &str,
    ) -> Result<ExtensionStateView, ExtensionError> {
        Ok(ExtensionStateView {
            latest: self.latest.lock().expect("state store lock").clone(),
        })
    }

    fn append_extension_state(
        &self,
        _extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), ExtensionError> {
        self.latest
            .lock()
            .expect("state store lock")
            .insert(update.kind, update.content);
        Ok(())
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// The same source-tree limits a host derives from its frozen harness limits.
fn limits() -> ExtensionLimits {
    let harness = tea_core::harness::HarnessResourceLimits::default();
    ExtensionLimits {
        max_source_bytes: harness.source_bytes,
        max_memory_bytes: harness.memory_bytes,
        max_interrupt_checks: harness.instruction_checks as usize,
    }
}

/// One resolved todo extension over one durable namespace.
struct TodoFixture {
    extension: ResolvedExtension,
    store: Arc<MemoryStateStore>,
    calls: std::cell::Cell<u64>,
}

/// One settled `todo` invocation.
struct TodoOutcome {
    content: String,
    is_error: bool,
    updates: Vec<ToolUpdate>,
}

impl TodoOutcome {
    /// The activity projection this call published, which must exist for every
    /// invocation and must describe post-operation state.
    fn activity(&self) -> &str {
        let published = self
            .updates
            .iter()
            .filter_map(|update| update.activity.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            published.len(),
            1,
            "every todo call publishes exactly one activity projection"
        );
        published[0]
    }
}

impl TodoFixture {
    fn new() -> Self {
        let store = Arc::new(MemoryStateStore::default());
        let handle = ExtensionStateHandle::new();
        handle
            .attach(Arc::clone(&store) as Arc<dyn ExtensionStateStore>)
            .expect("state handle attaches once");
        let mut bindings = ExtensionCapabilityBindings::new();
        bindings
            .insert(
                "extension.state",
                Arc::new(
                    ExtensionStateCapability::new("todo", handle).expect("portable extension id"),
                ),
                // Exactly the grant the durable host gives this extension.
                tea_luau::builtins::todo_tool_limits(),
            )
            .expect("capability grant is unique");
        let extension = LuauExtensionEngine
            .resolve(
                &tea_luau::builtins::todo(limits()),
                bindings,
                Arc::new(NoHooks),
                0,
                Arc::new(ExtensionMemoryCollector::default()),
            )
            .expect("bundled todo source resolves");
        Self {
            extension,
            store,
            calls: std::cell::Cell::new(0),
        }
    }

    fn call(&self, arguments: &str) -> TodoOutcome {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let sink_updates = Arc::clone(&updates);
        let sequence = self.calls.get() + 1;
        self.calls.set(sequence);
        let result = block_on(
            self.extension
                .tools
                .get("todo")
                .expect("todo tool is resolved")
                .execute(
                    ToolCall {
                        id: ToolCallId::new(format!("todo-{sequence}")).expect("call id is valid"),
                        name: "todo".into(),
                        arguments: SerializedJson::new(arguments),
                    },
                    ToolContext {
                        cancellation: tea_core::scheduler::CancellationToken::new(),
                        provenance: RunProvenance::default(),
                    },
                    ToolUpdateSink::new(move |update| {
                        sink_updates.lock().expect("update lock").push(update)
                    }),
                ),
        )
        .expect("the todo handler settles");
        let updates = updates.lock().expect("update lock").clone();
        TodoOutcome {
            content: result.content,
            is_error: result.is_error,
            updates,
        }
    }

    /// Successful call; the model-facing content.
    fn ok(&self, arguments: &str) -> TodoOutcome {
        let outcome = self.call(arguments);
        assert!(
            !outcome.is_error,
            "expected success, got error: {}",
            outcome.content
        );
        outcome
    }

    /// Refused call; the model-facing diagnostic.
    fn err(&self, arguments: &str) -> TodoOutcome {
        let outcome = self.call(arguments);
        assert!(
            outcome.is_error,
            "expected refusal, got success: {}",
            outcome.content
        );
        outcome
    }

    fn stored(&self) -> Option<JsonValue> {
        self.store
            .latest
            .lock()
            .expect("state store lock")
            .get("todo.state.v1")
            .cloned()
    }

    fn install(&self, state: &str) {
        self.store.latest.lock().expect("state store lock").insert(
            "todo.state.v1".into(),
            JsonValue::parse(state).expect("fixture state is valid JSON"),
        );
    }

    fn todos_command(&self) -> String {
        let command = self
            .extension
            .host_commands
            .iter()
            .find(|command| command.description().name == "/todos")
            .expect("/todos is declared");
        command
            .invoke(&ExtensionCommandInput {
                arguments: String::new(),
                state: ExtensionStateView {
                    latest: self.store.latest.lock().expect("state store lock").clone(),
                },
            })
            .expect("/todos evaluates")
            .notice
            .expect("/todos prints the list")
    }
}

fn markdown_call(markdown: &str) -> String {
    format!(
        "{{\"markdown\":{}}}",
        JsonValue::String(markdown.to_owned())
            .to_json_string()
            .expect("markdown serializes")
    )
}

/// The canonical plan used by most structural tests.
const ROOT_PLAN: &str = "- [ ] Root\n  - [ ] Child A\n  - [ ] Child B\n";

#[test]
fn empty_state_reads_as_an_empty_list_and_publishes_empty_activity() {
    let todo = TodoFixture::new();
    let read = todo.ok("{}");
    assert_eq!(
        read.content,
        "TODO · 0 active · 0 pending · 0 blocked · 0 dropped · 0 done\n\nThe todo list is empty."
    );
    assert_eq!(read.activity(), "Todo · empty");
    assert_eq!(todo.stored(), None, "a read never writes durable state");

    let resynchronized = todo.ok(&markdown_call(&read.content));
    assert_eq!(resynchronized.content, read.content);
}

#[test]
fn only_one_leading_canonical_header_is_accepted_during_structural_sync() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    let document = todo.ok("{}").content;

    let refused = todo.err(&markdown_call(&format!("{document}\n\n{document}")));
    assert!(
        refused.content.contains("is not a todo row"),
        "a second generated header must remain document text: {}",
        refused.content
    );
}

#[test]
fn an_initial_plan_allocates_deterministic_ids_and_activates_the_first_leaf() {
    let todo = TodoFixture::new();
    let created = todo.ok(&markdown_call(ROOT_PLAN));
    assert_eq!(
        created.content,
        "TODO · 1 active · 2 pending · 0 blocked · 0 dropped · 0 done\n\
         \n\
         - [ ] #1 Root\n\
         \x20 - [>] #2 Child A\n\
         \x20 - [ ] #3 Child B"
    );
    assert_eq!(
        created.activity(),
        "Todo · 1 active · 2 pending\n- [ ] Root\n  - [>] Child A\n  - [ ] Child B"
    );
    assert_eq!(
        todo.stored()
            .as_ref()
            .and_then(|state| state.get("next_id"))
            .and_then(JsonValue::as_f64),
        Some(4.0)
    );
    // A read returns the same canonical document.
    assert_eq!(todo.ok("{}").content, created.content);
}

#[test]
fn structural_append_keeps_existing_ids_and_allocates_monotonically() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    let appended = todo.ok(&markdown_call(
        "- [ ] #1 Root\n  - [>] #2 Child A\n  - [ ] #3 Child B\n  - [ ] Child C\n",
    ));
    assert!(appended.content.contains("- [ ] #4 Child C"));
    assert!(appended.content.contains("- [>] #2 Child A"));
    assert_eq!(
        todo.stored()
            .as_ref()
            .and_then(|state| state.get("next_id"))
            .and_then(JsonValue::as_f64),
        Some(5.0)
    );
}

#[test]
fn renaming_reordering_and_reparenting_are_inferred_from_the_document() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    let restructured = todo.ok(&markdown_call(
        "- [ ] #1 Root renamed\n  - [ ] #3 Child B\n    - [>] #2 Child A\n",
    ));
    assert_eq!(
        restructured.content,
        "TODO · 1 active · 2 pending · 0 blocked · 0 dropped · 0 done\n\
         \n\
         - [ ] #1 Root renamed\n\
         \x20 - [ ] #3 Child B\n\
         \x20   - [>] #2 Child A"
    );
    let stored = todo.stored().expect("state persists");
    let items = stored.get("items").and_then(JsonValue::as_array).unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[2].get("id").and_then(JsonValue::as_f64), Some(2.0));
    assert_eq!(
        items[2].get("parent_id").and_then(JsonValue::as_f64),
        Some(3.0)
    );
}

#[test]
fn structural_sync_reopens_a_closed_ancestor_of_active_work() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call("- [ ] Finished parent\n- [ ] Active child\n"));
    todo.ok(r#"{"updates":[{"id":1,"status":"done"}]}"#);

    let synchronized = todo.ok(&markdown_call(
        "- [ ] #1 Finished parent\n  - [>] #2 Active child\n",
    ));
    assert!(synchronized.content.contains("- [ ] #1 Finished parent"));
    assert!(synchronized
        .content
        .contains("  - [>] #2 Active child"));
}

#[test]
fn structural_sync_rejects_open_work_below_a_blocked_ancestor() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call("- [ ] Active work\n- [ ] Blocked parent\n"));
    todo.ok(r#"{"updates":[{"id":2,"status":"blocked","reason":"waiting for input"}]}"#);
    let before = todo.stored();

    let refused = todo.err(&markdown_call(
        "- [ ] #2 Blocked parent\n  - [>] #1 Active work\n",
    ));
    assert!(
        refused
            .content
            .contains("open todo #1 beneath blocked todo #2"),
        "{}",
        refused.content
    );
    assert_eq!(todo.stored(), before, "a refused restructure is atomic");
}

#[test]
fn omitting_a_row_removes_it_and_never_reuses_its_id() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    todo.ok(&markdown_call("- [ ] #1 Root\n  - [ ] #3 Child B\n"));
    let rebuilt = todo.ok(&markdown_call(
        "- [ ] #1 Root\n  - [ ] #3 Child B\n  - [ ] Child A again\n",
    ));
    assert!(rebuilt.content.contains("- [ ] #4 Child A again"));
    assert!(!rebuilt.content.contains("#2"));
}

#[test]
fn structural_markdown_markers_do_not_override_existing_statuses() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    todo.ok(r#"{"updates":[{"id":2,"status":"done"}]}"#);
    let synchronized = todo.ok(&markdown_call(
        "- [ ] #1 Root\n  - [ ] #2 Child A\n  - [x] #3 Child B\n",
    ));
    assert!(
        synchronized.content.contains("- [x] #2 Child A"),
        "markers are display hints during structural sync: {}",
        synchronized.content
    );
    assert!(synchronized.content.contains("- [>] #3 Child B"));
}

#[test]
fn invalid_structure_is_refused_atomically_and_leaves_state_exactly_as_it_was() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    let before = todo.stored();
    for (markdown, expected) in [
        ("- [ ] #1 Root\n   - [ ] deep\n", "odd indentation"),
        (
            "- [ ] #1 Root\n\t- [ ] tabbed\n",
            "indent with two spaces per level",
        ),
        (
            "- [ ] #1 Root\n      - [ ] jump\n",
            "indents more than one level deeper",
        ),
        ("- [ ] #999 Unknown\n", "does not exist"),
        ("- [ ] #1 Root\n- [ ] #1 Root again\n", "repeats todo #1"),
        ("- [ ] #1 Root\n  - [x] New done row\n", "must use [ ]"),
        ("- [ ]\n", "not a todo row"),
        ("- [ ] \n", "empty task text"),
        ("- [?] #1 Root\n", "unknown marker"),
    ] {
        let refused = todo.err(&markdown_call(markdown));
        assert!(
            refused.content.contains(expected),
            "expected {expected:?} in {:?}",
            refused.content
        );
        assert_eq!(todo.stored(), before, "a refused sync persists nothing");
        // A refusal still republishes the previous valid presentation.
        assert!(refused.activity().starts_with("Todo · 1 active"));
    }
}

#[test]
fn excessive_depth_count_and_text_are_refused() {
    let todo = TodoFixture::new();
    let mut deep = String::new();
    for level in 0..9 {
        deep.push_str(&"  ".repeat(level));
        deep.push_str("- [ ] level\n");
    }
    assert!(todo
        .err(&markdown_call(&deep))
        .content
        .contains("nested deeper than 8 levels"));

    let wide = "- [ ] task\n".repeat(129);
    assert!(todo
        .err(&markdown_call(&wide))
        .content
        .contains("more than 128 tasks"));

    let long = format!("- [ ] {}\n", "t".repeat(201));
    assert!(todo
        .err(&markdown_call(&long))
        .content
        .contains("longer than 200 characters"));

    assert_eq!(todo.stored(), None);
}

#[test]
fn empty_markdown_clears_the_list_without_resetting_identity() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(ROOT_PLAN));
    let cleared = todo.ok(&markdown_call(""));
    assert!(cleared.content.ends_with("The todo list is empty."));
    assert_eq!(cleared.activity(), "Todo · empty");
    let restarted = todo.ok(&markdown_call("- [ ] Fresh start\n"));
    assert!(
        restarted.content.contains("- [>] #4 Fresh start"),
        "ids stay monotonic across a clear: {}",
        restarted.content
    );
}

#[test]
fn completion_closes_open_descendants_and_leaves_dropped_work_dropped() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n  - [ ] Beta\n  - [ ] Gamma\n",
    ));
    todo.ok(r#"{"updates":[{"id":3,"status":"dropped"}]}"#);
    todo.ok(r#"{"updates":[{"id":4,"status":"done"}]}"#);
    let completed = todo.ok(r#"{"updates":[{"id":1,"status":"done"}]}"#);
    assert_eq!(
        completed.content,
        "Completed #1 and 1 descendant.\nTODO · 0 active · 0 pending · 0 blocked · 1 dropped · 3 done"
    );
    let listed = todo.ok("{}").content;
    assert!(listed.contains("- [x] #1 Parent"));
    assert!(listed.contains("- [x] #2 Alpha"));
    assert!(listed.contains("- [~] #3 Beta"));
    assert!(listed.contains("- [x] #4 Gamma"));
}

#[test]
fn blocking_records_a_reason_and_blocks_only_open_descendants() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n  - [ ] Beta\n",
    ));
    todo.ok(r#"{"updates":[{"id":2,"status":"done"}]}"#);
    let blocked = todo
        .ok(r#"{"updates":[{"id":1,"status":"blocked","reason":"waiting for upstream fixture"}]}"#);
    assert_eq!(
        blocked.content,
        "Blocked #1 and 1 descendant.\nTODO · 0 active · 0 pending · 2 blocked · 0 dropped · 1 done"
    );
    let listed = todo.ok("{}").content;
    assert!(listed.contains("- [!] #1 Parent — waiting for upstream fixture"));
    assert!(listed.contains("- [x] #2 Alpha"));
    assert!(listed.contains("- [!] #3 Beta"));
    assert!(blocked
        .activity()
        .contains("Parent — waiting for upstream fixture"));
    // Blocked work is never automatically promoted.
    assert!(blocked.content.contains("0 active"));
}

#[test]
fn dropping_abandons_open_work_and_keeps_finished_work_done() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n  - [ ] Beta\n",
    ));
    todo.ok(r#"{"updates":[{"id":2,"status":"done"}]}"#);
    todo.ok(r#"{"updates":[{"id":1,"status":"dropped"}]}"#);
    let listed = todo.ok("{}").content;
    assert!(listed.contains("- [~] #1 Parent"));
    assert!(listed.contains("- [x] #2 Alpha"));
    assert!(listed.contains("- [~] #3 Beta"));
}

#[test]
fn pending_reopens_the_subtree_and_its_closed_ancestors() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n    - [ ] Deep\n",
    ));
    todo.ok(r#"{"updates":[{"id":1,"status":"blocked","reason":"external"}]}"#);
    todo.ok(r#"{"updates":[{"id":1,"status":"done"}]}"#);
    let reopened = todo.ok(r#"{"updates":[{"id":2,"status":"pending"}]}"#);
    assert!(reopened
        .content
        .starts_with("Reopened #2 and 1 descendant."));
    let listed = todo.ok("{}").content;
    assert!(listed.contains("- [ ] #1 Parent"), "{listed}");
    assert!(listed.contains("- [ ] #2 Alpha"), "{listed}");
    assert!(listed.contains("- [>] #3 Deep"), "{listed}");
    assert!(
        !listed.contains("external"),
        "blocker reasons clear: {listed}"
    );
}

#[test]
fn starting_work_reopens_ancestors_and_permits_several_active_items() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n- [ ] Other\n  - [ ] Beta\n",
    ));
    todo.ok(r#"{"updates":[{"id":1,"status":"done"}]}"#);
    let started = todo.ok(r#"{"updates":[{"id":2,"status":"in_progress"}]}"#);
    assert!(started.content.starts_with("Started #2."));
    let listed = todo.ok("{}").content;
    assert!(listed.contains("- [ ] #1 Parent"), "{listed}");
    assert!(listed.contains("- [>] #2 Alpha"), "{listed}");

    let parallel = todo.ok(r#"{"updates":[{"id":4,"status":"in_progress"}]}"#);
    assert!(
        parallel.content.contains("2 active"),
        "independent parallel work is valid: {}",
        parallel.content
    );
}

#[test]
fn closing_the_last_active_item_promotes_the_next_actionable_leaf() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Parent\n  - [ ] Alpha\n  - [ ] Beta\n  - [ ] Gamma\n",
    ));
    let done_alpha = todo.ok(r#"{"updates":[{"id":2,"status":"done"}]}"#);
    assert_eq!(
        done_alpha.content,
        "Completed #2.\nNext: #3 Beta\nTODO · 1 active · 2 pending · 0 blocked · 0 dropped · 1 done"
    );
    let blocked_beta = todo.ok(r#"{"updates":[{"id":3,"status":"blocked","reason":"upstream"}]}"#);
    assert!(
        blocked_beta.content.contains("Next: #4 Gamma"),
        "{}",
        blocked_beta.content
    );
}

#[test]
fn an_invalid_update_discards_every_earlier_update_in_the_same_array() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call("- [ ] Alpha\n- [ ] Beta\n"));
    let before = todo.stored();
    let refused = todo.err(r#"{"updates":[{"id":1,"status":"done"},{"id":99,"status":"done"}]}"#);
    assert!(refused.content.contains("todo #99 does not exist"));
    assert!(refused.content.contains("No status changes were applied."));
    assert_eq!(todo.stored(), before);
    assert!(todo.ok("{}").content.contains("- [>] #1 Alpha"));
}

#[test]
fn several_updates_settle_atomically_with_a_compact_receipt() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call(
        "- [ ] Alpha\n- [ ] Beta\n- [ ] Gamma\n- [ ] Delta\n",
    ));
    let batched = todo.ok(
        r#"{"updates":[{"id":1,"status":"done"},{"id":2,"status":"blocked","reason":"upstream"},{"id":3,"status":"in_progress"}]}"#,
    );
    assert_eq!(
        batched.content,
        "Updated 3 todos.\nNext: #3 Gamma\nTODO · 1 active · 1 pending · 1 blocked · 0 dropped · 1 done"
    );
}

#[test]
fn corrupt_durable_state_reports_an_error_instead_of_an_empty_plan() {
    let todo = TodoFixture::new();
    todo.install(r#"{"next_id":2,"items":[{"id":5,"text":"impossible","status":"pending"}]}"#);
    let refused = todo.err("{}");
    assert!(
        refused
            .content
            .starts_with("Durable todo state is unreadable"),
        "{}",
        refused.content
    );
    assert!(refused
        .updates
        .iter()
        .all(|update| update.activity.is_none()));
    assert!(todo.stored().is_some(), "corrupt state is left untouched");
}

#[test]
fn markdown_and_updates_cannot_be_combined() {
    let todo = TodoFixture::new();
    let refused = todo.err(r#"{"markdown":"- [ ] Alpha\n","updates":[{"id":1,"status":"done"}]}"#);
    assert!(refused.content.contains("not both"));
    assert_eq!(todo.stored(), None);
}

#[test]
fn a_blocked_row_round_trips_through_structural_markdown_without_absorbing_its_reason() {
    let todo = TodoFixture::new();
    todo.ok(&markdown_call("- [ ] Alpha\n- [ ] Beta\n"));
    todo.ok(r#"{"updates":[{"id":1,"status":"blocked","reason":"upstream fixture"}]}"#);
    let listed = todo.ok("{}").content;
    let before = todo.stored();
    let resynchronized = todo.ok(&markdown_call(&listed));
    assert_eq!(resynchronized.content, listed);
    assert_eq!(todo.stored(), before, "a read document round-trips exactly");
    assert!(
        resynchronized
            .content
            .contains("- [!] #1 Alpha — upstream fixture"),
        "{}",
        resynchronized.content
    );
}

#[test]
fn the_activity_projection_stays_bounded_for_a_large_plan() {
    let todo = TodoFixture::new();
    let mut plan = String::new();
    for index in 1..=40 {
        plan.push_str(&format!("- [ ] Task {index}\n"));
    }
    let created = todo.ok(&markdown_call(&plan));
    let activity = created.activity();
    let rows = activity.lines().count();
    assert_eq!(rows, 14, "one header, twelve rows, one omission line");
    assert!(activity.ends_with("  … 28 more"), "{activity}");
    assert!(
        created.content.lines().count() > rows,
        "the model-facing read is never paginated"
    );
}

#[test]
fn the_activity_projection_keeps_active_and_blocked_work_before_history() {
    let todo = TodoFixture::new();
    let mut plan = String::new();
    for index in 1..=20 {
        plan.push_str(&format!("- [ ] Task {index}\n"));
    }
    todo.ok(&markdown_call(&plan));
    let mut updates = Vec::new();
    for id in 1..=15 {
        updates.push(format!("{{\"id\":{id},\"status\":\"done\"}}"));
    }
    updates.push(r#"{"id":20,"status":"blocked","reason":"external"}"#.to_owned());
    let outcome = todo.ok(&format!("{{\"updates\":[{}]}}", updates.join(",")));
    let activity = outcome.activity();
    let rows = activity.lines().skip(1).collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        13,
        "twelve rows plus one omission line: {activity}"
    );
    // Every open row survives; completed history fills only the remainder.
    for open in [
        "- [>] Task 16",
        "- [ ] Task 17",
        "- [ ] Task 18",
        "- [ ] Task 19",
        "- [!] Task 20 — external",
    ] {
        assert!(activity.contains(open), "{activity}");
    }
    assert_eq!(
        rows.iter().filter(|row| row.starts_with("- [x]")).count(),
        7,
        "{activity}"
    );
    assert!(activity.ends_with("  … 8 more"), "{activity}");
}

#[test]
fn the_todos_command_prints_the_same_canonical_list() {
    let todo = TodoFixture::new();
    assert_eq!(
        todo.todos_command(),
        "TODO · 0 active · 0 pending · 0 blocked · 0 dropped · 0 done\n\nThe todo list is empty."
    );
    let created = todo.ok(&markdown_call(ROOT_PLAN));
    assert_eq!(todo.todos_command(), created.content);
}

#[test]
fn the_todos_command_reports_unreadable_state_rather_than_an_empty_list() {
    let todo = TodoFixture::new();
    todo.install(r#"{"next_id":"nope","items":[]}"#);
    assert!(todo
        .todos_command()
        .starts_with("Durable todo state is unreadable"));
}

#[test]
fn every_operation_publishes_exactly_one_post_operation_activity() {
    let todo = TodoFixture::new();
    assert_eq!(todo.ok("{}").activity(), "Todo · empty");
    let created = todo.ok(&markdown_call("- [ ] Alpha\n- [ ] Beta\n"));
    assert_eq!(
        created.activity(),
        "Todo · 1 active · 1 pending\n- [>] Alpha\n- [ ] Beta"
    );
    let updated = todo.ok(r#"{"updates":[{"id":1,"status":"done"}]}"#);
    assert_eq!(
        updated.activity(),
        "Todo · 1 active\n- [x] Alpha\n- [>] Beta",
        "the published projection describes post-operation state"
    );
    assert_eq!(todo.ok("{}").activity(), updated.activity());
}

#[test]
fn the_documented_bounds_are_reachable_within_the_extension_resource_budget() {
    let todo = TodoFixture::new();
    // A full-width, full-depth plan at the documented ceilings must parse,
    // synchronize, persist, and format inside the handler's finite VM budgets.
    let mut plan = String::new();
    for index in 0..128 {
        let level = index % 8;
        plan.push_str(&"  ".repeat(level));
        plan.push_str(&format!("- [ ] Task {index}\n"));
    }
    let created = todo.ok(&markdown_call(&plan));
    assert!(
        created.content.contains("#128 Task 127"),
        "{}",
        created.content
    );
    assert_eq!(created.content.lines().count(), 130);
    assert_eq!(created.activity().lines().count(), 14);
    let stored = todo
        .stored()
        .expect("state persists")
        .to_json_string()
        .expect("state serializes");
    assert!(stored.len() < 16 * 1024, "durable state stays storable");

    // A read and a status update over the same maximal plan also settle.
    assert_eq!(todo.ok("{}").content, created.content);
    assert!(todo
        .ok(r#"{"updates":[{"id":8,"status":"done"}]}"#)
        .content
        .starts_with("Completed #8"));
}

#[test]
fn a_plan_too_large_to_store_is_refused_with_actionable_guidance() {
    let todo = TodoFixture::new();
    let mut plan = String::new();
    for index in 0..100 {
        plan.push_str(&format!("- [ ] {index} {}\n", "w".repeat(190)));
    }
    let refused = todo.err(&markdown_call(&plan));
    assert!(
        refused.content.contains("too large to store"),
        "{}",
        refused.content
    );
    assert_eq!(todo.stored(), None);
}

#[test]
fn the_largest_storable_plan_reads_synchronizes_and_updates_within_budget() {
    let todo = TodoFixture::new();
    // The binding grant is sized for a full plan of long tasks: Luau charges an
    // interrupt per scanned pattern position, so document size, not item count,
    // dominates. Keep this close to the storable ceiling.
    let mut plan = String::new();
    for index in 0..128 {
        plan.push_str(&"  ".repeat(index % 8));
        plan.push_str(&format!("- [ ] Task {index} {}\n", "detail ".repeat(8)));
    }
    let created = todo.ok(&markdown_call(&plan));
    assert_eq!(created.content.lines().count(), 130);
    let stored = todo
        .stored()
        .expect("state persists")
        .to_json_string()
        .expect("state serializes");
    assert!(
        (13 * 1024..16 * 1024).contains(&stored.len()),
        "fixture should sit near the storable ceiling, got {} bytes",
        stored.len()
    );

    // Reading, editing, and reporting status over that same plan must settle.
    let read = todo.ok("{}");
    assert_eq!(read.content, created.content);
    let blocked =
        todo.ok(r#"{"updates":[{"id":1,"status":"blocked","reason":"upstream fixture"}]}"#);
    assert!(
        blocked.content.starts_with("Blocked #1"),
        "{}",
        blocked.content
    );
    todo.ok(&markdown_call(&read.content));
    assert!(todo.todos_command().starts_with("TODO · "));
}
