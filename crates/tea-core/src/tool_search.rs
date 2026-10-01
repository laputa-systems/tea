//! Deferred tool discovery.
//!
//! Port of upstream Pi's `extensions/tool-search/tool.ts`: a BM25 ranker over
//! tool metadata, shared by the optional `tool_search` tool and composition
//! scripts. `tool_search` searches authorized tools with
//! [`ToolExposure::Deferred`] that are not yet declared and loads the matches
//! through [`AgentToolResult::added_tool_names`], so the next request declares
//! them. Loading is recorded in the transcript like any configuration change,
//! so it survives resume and branch forks. Discovery never grants authority:
//! it can only expose tools already in the run's executable registry.

use crate::error::ToolError;
use crate::tool::{
    AgentTool, AgentToolResult, CompositionAccess, ToolCall, ToolCatalogEntry, ToolContext,
    ToolDeclaration, ToolExposure, ToolFuture, ToolUpdateSink,
};
use std::collections::{BTreeMap, BTreeSet};
use tea_protocol::JsonValue;

/// Name of the discovery tool.
pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
/// Default number of tools a search loads.
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;
/// Largest number of tools one search may load.
pub const MAX_TOOL_SEARCH_LIMIT: usize = 32;

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// Naive singular form, so `issues` matches `issue` and `searches` matches `search`.
fn stem(term: &str) -> String {
    let length = term.len();
    if length > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..length - 3]);
    }
    if length > 4 && ["ches", "shes", "sses", "xes", "zes"].iter().any(|suffix| term.ends_with(suffix)) {
        return term[..length - 2].to_owned();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..length - 1].to_owned();
    }
    term.to_owned()
}

/// Lowercase terms split at camelCase boundaries and non-alphanumerics,
/// without stop words, folded to a naive singular.
pub fn tokenize(text: &str) -> Vec<String> {
    // Insert boundaries: `aB` -> `a B` and `ABc` -> `A Bc`, as Pi's regexes do.
    let characters = text.chars().collect::<Vec<_>>();
    let mut spaced = String::with_capacity(text.len() + 8);
    for (index, &character) in characters.iter().enumerate() {
        if index > 0 && character.is_ascii_uppercase() {
            let previous = characters[index - 1];
            let next_is_lower = characters
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase());
            if previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase() && next_is_lower)
            {
                spaced.push(' ');
            }
        }
        spaced.push(character);
    }
    spaced
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// A tool as the ranker sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolSearchDocument {
    /// Tool name.
    pub name: String,
    /// Searchable text.
    pub text: String,
}

impl ToolSearchDocument {
    /// The name, the name with `_` as spaces, the description, and schema
    /// descriptions and property names, recursively.
    pub fn from_declaration(declaration: &ToolDeclaration) -> Self {
        let mut parts = vec![
            declaration.name.clone(),
            declaration.name.replace('_', " "),
            declaration.description.clone(),
        ];
        schema_text(&declaration.schema, &mut parts);
        Self {
            name: declaration.name.clone(),
            text: parts
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

fn schema_text(schema: &JsonValue, parts: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(description) = object.get("description").and_then(JsonValue::as_str) {
        parts.push(description.to_owned());
    }
    if let Some(properties) = object.get("properties").and_then(JsonValue::as_object) {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, parts);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, parts);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = object.get(key).and_then(JsonValue::as_array) {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}

/// One ranked match.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSearchMatch {
    /// Tool name.
    pub name: String,
    /// BM25 score.
    pub score: f64,
}

/// Okapi BM25 with `k1 = 1.2` and `b = 0.75`. Ties keep document order.
pub fn rank(query: &str, documents: &[ToolSearchDocument], limit: usize) -> Vec<ToolSearchMatch> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let query_terms = tokenize(query)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }
    let term_counts = documents
        .iter()
        .map(|document| {
            let mut counts = BTreeMap::<String, u32>::new();
            for term in tokenize(&document.text) {
                *counts.entry(term).or_default() += 1;
            }
            counts
        })
        .collect::<Vec<_>>();
    let lengths = term_counts
        .iter()
        .map(|counts| counts.values().sum::<u32>() as f64)
        .collect::<Vec<_>>();
    let average = lengths.iter().sum::<f64>() / documents.len() as f64;
    let average = if average == 0.0 { 1.0 } else { average };
    let total = documents.len() as f64;
    let idf = query_terms
        .iter()
        .map(|term| {
            let frequency = term_counts
                .iter()
                .filter(|counts| counts.contains_key(term))
                .count() as f64;
            (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln()
        })
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let mut score = 0.0;
        for (term, idf) in query_terms.iter().zip(&idf) {
            let Some(&count) = term_counts[index].get(term) else {
                continue;
            };
            let count = count as f64;
            let norm = K1 * (1.0 - B + B * lengths[index] / average);
            score += idf * (count * (K1 + 1.0)) / (count + norm);
        }
        if score > 0.0 {
            matches.push(ToolSearchMatch {
                name: document.name.clone(),
                score,
            });
        }
    }
    // A stable sort keeps document order for equal scores.
    matches.sort_by(|left, right| right.score.total_cmp(&left.score));
    matches.truncate(limit);
    matches
}

/// The `tool_search` description. It does not list the searchable tools, so
/// it stays the same while tools are registered (for example when an MCP
/// server connects) and keeps the request prefix cacheable.
pub const TOOL_SEARCH_DESCRIPTION: &str = "Search tools that are available but not yet loaded, and load the best matches so you can call them from your next step. Some tools, such as tools of MCP servers, are not provided upfront; use this tool to find them. Searches tool names, descriptions, and parameter names.";

/// The optional discovery tool.
#[derive(Debug)]
pub struct ToolSearchTool {
    schema: JsonValue,
}

impl Default for ToolSearchTool {
    fn default() -> Self {
        Self {
            schema: JsonValue::object([
                ("type", JsonValue::from("object")),
                (
                    "properties",
                    JsonValue::object([
                        (
                            "query",
                            JsonValue::object([
                                ("type", JsonValue::from("string")),
                                (
                                    "description",
                                    JsonValue::from("Search query for tools that are not loaded yet."),
                                ),
                            ]),
                        ),
                        (
                            "limit",
                            JsonValue::object([
                                ("type", JsonValue::from("integer")),
                                ("minimum", JsonValue::from(1_u64)),
                                ("maximum", JsonValue::from(MAX_TOOL_SEARCH_LIMIT as u64)),
                                (
                                    "description",
                                    JsonValue::from(format!(
                                        "Maximum number of tools to load. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}."
                                    )),
                                ),
                            ]),
                        ),
                    ]),
                ),
                ("required", JsonValue::Array(vec![JsonValue::from("query")])),
                ("additionalProperties", JsonValue::Bool(false)),
            ]),
        }
    }
}

/// Rank searchable, not-yet-declared deferred tools in a catalog.
pub fn search_deferred(
    catalog: &[ToolCatalogEntry],
    is_declared: impl Fn(&str) -> bool,
    query: &str,
    limit: usize,
) -> Vec<(ToolSearchMatch, ToolDeclaration)> {
    let candidates = catalog
        .iter()
        .filter(|entry| {
            entry.exposure == ToolExposure::Deferred && !is_declared(&entry.declaration.name)
        })
        .collect::<Vec<_>>();
    let documents = candidates
        .iter()
        .map(|entry| ToolSearchDocument::from_declaration(&entry.declaration))
        .collect::<Vec<_>>();
    rank(query, &documents, limit)
        .into_iter()
        .filter_map(|found| {
            candidates
                .iter()
                .find(|entry| entry.declaration.name == found.name)
                .map(|entry| (found, entry.declaration.clone()))
        })
        .collect()
}

impl AgentTool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn description(&self) -> &str {
        TOOL_SEARCH_DESCRIPTION
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
    }

    fn composition_access(&self) -> CompositionAccess {
        CompositionAccess::Catalog
    }

    fn script_callable(&self) -> bool {
        // Searching changes what the model sees; scripts have their own
        // catalog search instead.
        false
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = JsonValue::parse(call.arguments.as_str()).map_err(|error| {
                ToolError::InvalidArguments {
                    tool: TOOL_SEARCH_TOOL_NAME.into(),
                    message: error.to_string(),
                }
            })?;
            let query = arguments
                .get("query")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            if query.trim().is_empty() {
                return Err(ToolError::InvalidArguments {
                    tool: TOOL_SEARCH_TOOL_NAME.into(),
                    message: "query must not be empty".into(),
                });
            }
            let limit = arguments
                .get("limit")
                .and_then(JsonValue::as_u64)
                .map_or(DEFAULT_TOOL_SEARCH_LIMIT, |limit| limit as usize)
                .clamp(1, MAX_TOOL_SEARCH_LIMIT);
            let found = match &context.composition {
                Some(composition) => search_deferred(
                    composition.catalog(),
                    |name| composition.is_declared(name),
                    query,
                    limit,
                ),
                None => Vec::new(),
            };
            let content = if found.is_empty() {
                "No matching tools found.".to_owned()
            } else {
                let mut content = format!(
                    "Loaded {} tool{}. They are available from your next call:",
                    found.len(),
                    if found.len() == 1 { "" } else { "s" }
                );
                for (_, declaration) in &found {
                    let summary = declaration.description.trim().lines().next().unwrap_or("");
                    content.push_str(&format!("\n- {}: {summary}", declaration.name));
                }
                content
            };
            Ok(AgentToolResult {
                tool_call_id: call.id,
                content,
                details: None,
                usage: None,
                added_tool_names: found
                    .into_iter()
                    .map(|(found, _)| found.name)
                    .collect(),
                terminate: false,
                is_error: false,
                failure: None,
            })
        })
    }
}

#[cfg(test)]
mod tests;
