//! Ported from Pi's `tool-search.test.ts`, plus discovery-tool behavior.

use super::*;
use crate::state::{SerializedJson, ToolCallId};
use crate::tool::ToolComposition;

fn declaration(name: &str, description: &str, properties: JsonValue) -> ToolDeclaration {
    ToolDeclaration::new(
        name,
        description,
        JsonValue::object([
            ("type", JsonValue::from("object")),
            ("properties", properties),
        ]),
    )
}

fn no_properties() -> JsonValue {
    JsonValue::object(Vec::<(&str, JsonValue)>::new())
}

fn documents() -> Vec<ToolSearchDocument> {
    [
        declaration(
            "mcp__github__list_issues",
            "List issues in a repository.",
            JsonValue::object([(
                "state",
                JsonValue::object([
                    ("type", JsonValue::from("string")),
                    ("description", JsonValue::from("open or closed")),
                ]),
            )]),
        ),
        declaration(
            "mcp__github__create_pull_request",
            "Open a pull request.",
            no_properties(),
        ),
        declaration(
            "mcp__linear__search_issues",
            "Search Linear issues by text.",
            no_properties(),
        ),
        declaration("mcp__docs__search", "Search the documentation.", no_properties()),
    ]
    .iter()
    .map(ToolSearchDocument::from_declaration)
    .collect()
}

fn names(matches: &[ToolSearchMatch]) -> Vec<&str> {
    matches.iter().map(|found| found.name.as_str()).collect()
}

#[test]
fn tokenize_splits_case_and_snake_drops_stop_words_and_folds_plurals() {
    assert_eq!(
        tokenize("listIssues for the GitHub_repo"),
        ["list", "issue", "git", "hub", "repo"]
    );
    assert_eq!(
        tokenize("searches queries HTTPServer"),
        ["search", "query", "http", "server"]
    );
}

#[test]
fn ranks_by_relevance_and_respects_the_limit() {
    let documents = documents();
    assert_eq!(
        names(&rank("issue", &documents, 8)),
        ["mcp__linear__search_issues", "mcp__github__list_issues"]
    );
    assert_eq!(
        rank("pull requests", &documents, 8)[0].name,
        "mcp__github__create_pull_request"
    );
    assert_eq!(rank("search", &documents, 1).len(), 1);
    // Property names and descriptions are searchable.
    assert_eq!(
        names(&rank("closed", &documents, 8)),
        ["mcp__github__list_issues"]
    );
}

#[test]
fn unknown_or_empty_queries_find_nothing() {
    let documents = documents();
    assert!(rank("kubernetes", &documents, 8).is_empty());
    assert!(rank("the", &documents, 8).is_empty());
    // Known limit carried from Pi: no synonyms.
    assert!(rank("tickets", &documents, 8).is_empty());
}

fn entry(name: &str, description: &str, exposure: ToolExposure) -> ToolCatalogEntry {
    ToolCatalogEntry {
        declaration: declaration(name, description, no_properties()),
        exposure,
        script_callable: true,
    }
}

fn call(arguments: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::new("call-search").expect("id"),
        name: TOOL_SEARCH_TOOL_NAME.into(),
        arguments: SerializedJson::new(arguments),
    }
}

#[test]
fn the_tool_loads_only_undeclared_deferred_matches() {
    let catalog = vec![
        entry("read", "Read a file.", ToolExposure::Direct),
        entry("issues_list", "List tracker issues.", ToolExposure::Deferred),
        entry("issues_loaded", "Close tracker issues.", ToolExposure::Deferred),
        entry("issues_inner", "Script-only issue helper.", ToolExposure::Composition),
    ];
    let context = ToolContext {
        cancellation: crate::scheduler::CancellationToken::new(),
        provenance: Default::default(),
        composition: Some(ToolComposition::catalog_only(
            catalog,
            ["read".to_owned(), "issues_loaded".to_owned()],
        )),
    };
    let tool = ToolSearchTool::default();
    let result = smol::block_on(tool.execute(
        call(r#"{"query":"issues"}"#),
        context.clone(),
        ToolUpdateSink::default(),
    ))
    .expect("search succeeds");
    assert_eq!(result.added_tool_names, ["issues_list"]);
    assert!(result.content.starts_with("Loaded 1 tool."));
    assert!(result.content.contains("- issues_list: List tracker issues."));

    let none = smol::block_on(tool.execute(
        call(r#"{"query":"kubernetes"}"#),
        context.clone(),
        ToolUpdateSink::default(),
    ))
    .expect("search succeeds");
    assert!(none.added_tool_names.is_empty());
    assert_eq!(none.content, "No matching tools found.");

    let empty = smol::block_on(tool.execute(
        call(r#"{"query":"  "}"#),
        context,
        ToolUpdateSink::default(),
    ));
    assert!(matches!(empty, Err(ToolError::InvalidArguments { .. })));
    assert!(!tool.script_callable());
}
