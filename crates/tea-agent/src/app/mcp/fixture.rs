//! A self-contained fake MCP stdio server for offline verification.
//!
//! It speaks the same newline-delimited JSON-RPC as a real server and exposes
//! tools that exercise text, structured, unsupported, error, slow/cancelled,
//! paginated, schema-normalization, list-change, and crash behavior. It is
//! compiled into tests and into the `tea-mcp-fixture` binary (feature
//! `mcp-fixture`), which doubles as the local example server.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use tea_protocol::JsonValue;

fn empty() -> JsonValue {
    JsonValue::object(Vec::<(&str, JsonValue)>::new())
}

fn tool(name: &str, description: &str, schema: JsonValue) -> JsonValue {
    JsonValue::object([
        ("name", JsonValue::from(name)),
        ("description", JsonValue::from(description)),
        ("inputSchema", schema),
    ])
}

fn object_schema(properties: Vec<(&str, JsonValue)>, required: &[&str]) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::from("object")),
        ("properties", JsonValue::object(properties)),
        (
            "required",
            JsonValue::Array(required.iter().map(|name| JsonValue::from(*name)).collect()),
        ),
    ])
}

fn typed(kind: &str) -> JsonValue {
    JsonValue::object([("type", JsonValue::from(kind))])
}

fn first_page() -> Vec<JsonValue> {
    vec![
        tool(
            "echo",
            "Echo the given text back.",
            object_schema(vec![("text", typed("string"))], &["text"]),
        ),
        tool(
            "add",
            "Add two numbers and return the sum as structured content.",
            object_schema(vec![("a", typed("number")), ("b", typed("number"))], &["a", "b"]),
        ),
        tool("image", "Return a tiny image.", object_schema(Vec::new(), &[])),
        tool(
            "fail",
            "Always report a tool error.",
            object_schema(Vec::new(), &[]),
        ),
    ]
}

fn second_page() -> Vec<JsonValue> {
    vec![
        tool(
            "slow",
            "Never answers until cancelled.",
            object_schema(Vec::new(), &[]),
        ),
        tool(
            "cancelled_count",
            "Report how many cancellation notifications arrived.",
            object_schema(Vec::new(), &[]),
        ),
        tool(
            "lookup_issue",
            "Look up an issue in the tracker by its identifier.",
            JsonValue::object([
                ("type", JsonValue::from("object")),
                (
                    "properties",
                    JsonValue::object([
                        (
                            "id",
                            JsonValue::object([("$ref", JsonValue::from("#/$defs/IssueId"))]),
                        ),
                        (
                            "url",
                            JsonValue::object([
                                ("type", JsonValue::from("string")),
                                ("format", JsonValue::from("uri")),
                            ]),
                        ),
                    ]),
                ),
                ("required", JsonValue::Array(vec![JsonValue::from("id")])),
                (
                    "$defs",
                    JsonValue::object([(
                        "IssueId",
                        JsonValue::object([
                            ("type", JsonValue::from("string")),
                            ("pattern", JsonValue::from("^[A-Z]+-[0-9]+$")),
                            ("description", JsonValue::from("Issue key such as TEA-1.")),
                        ]),
                    )]),
                ),
            ]),
        ),
        tool(
            "change_tools",
            "Add the late_tool tool and announce a changed tool list.",
            object_schema(Vec::new(), &[]),
        ),
        tool(
            "crash",
            "Exit the server process immediately.",
            object_schema(Vec::new(), &[]),
        ),
    ]
}

fn text(value: &str) -> JsonValue {
    JsonValue::Array(vec![JsonValue::object([
        ("type", JsonValue::from("text")),
        ("text", JsonValue::from(value)),
    ])])
}

/// Serve one connection until input ends. `crash` returns `false` to ask a
/// process host to exit abnormally.
pub fn serve(input: impl BufRead, mut output: impl Write) -> bool {
    let mut late_tool = false;
    let mut cancellations = 0_u64;
    let mut slow = BTreeSet::new();
    // A stray line before any response exercises client tolerance.
    let _ = writeln!(output, "fixture: starting");
    let _ = output.flush();
    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(message) = JsonValue::parse(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(id) = message.get("id").cloned() else {
            if method == "notifications/cancelled" {
                cancellations += 1;
                if let Some(request) = message
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                    .and_then(JsonValue::as_u64)
                {
                    slow.remove(&request);
                }
            }
            continue;
        };
        if message.get("result").is_some() || message.get("error").is_some() {
            // A response to our ping.
            continue;
        }
        let params = message.get("params").cloned().unwrap_or_else(empty);
        let result = match method.as_str() {
            "initialize" => Ok(JsonValue::object([
                ("protocolVersion", JsonValue::from(super::PROTOCOL_VERSION)),
                (
                    "capabilities",
                    JsonValue::object([(
                        "tools",
                        JsonValue::object([("listChanged", JsonValue::Bool(true))]),
                    )]),
                ),
                (
                    "serverInfo",
                    JsonValue::object([
                        ("name", JsonValue::from("tea-mcp-fixture")),
                        ("version", JsonValue::from("1.0.0")),
                    ]),
                ),
            ])),
            "tools/list" => {
                let page = params.get("cursor").and_then(JsonValue::as_str);
                let (mut tools, next) = match page {
                    None => (first_page(), Some("page-2")),
                    Some("page-2") => (second_page(), None),
                    Some(_) => (Vec::new(), None),
                };
                if next.is_none() && late_tool {
                    tools.push(tool(
                        "late_tool",
                        "A tool that appeared after a list change.",
                        object_schema(Vec::new(), &[]),
                    ));
                }
                let mut fields = vec![("tools", JsonValue::Array(tools))];
                if let Some(next) = next {
                    fields.push(("nextCursor", JsonValue::from(next)));
                }
                Ok(JsonValue::object(fields))
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or_else(empty);
                match name {
                    "echo" => Ok(JsonValue::object([(
                        "content",
                        text(arguments.get("text").and_then(JsonValue::as_str).unwrap_or("")),
                    )])),
                    "add" => {
                        let sum = arguments.get("a").and_then(JsonValue::as_f64).unwrap_or(0.0)
                            + arguments.get("b").and_then(JsonValue::as_f64).unwrap_or(0.0);
                        Ok(JsonValue::object([
                            ("content", JsonValue::Array(Vec::new())),
                            (
                                "structuredContent",
                                JsonValue::object([("sum", JsonValue::Number(tea_protocol::JsonNumber::Float(sum)))]),
                            ),
                        ]))
                    }
                    "image" => Ok(JsonValue::object([(
                        "content",
                        JsonValue::Array(vec![
                            JsonValue::object([
                                ("type", JsonValue::from("text")),
                                ("text", JsonValue::from("a tiny image follows")),
                            ]),
                            JsonValue::object([
                                ("type", JsonValue::from("image")),
                                ("mimeType", JsonValue::from("image/png")),
                                ("data", JsonValue::from("iVBORw0KGgo=")),
                            ]),
                        ]),
                    )])),
                    "fail" => Ok(JsonValue::object([
                        ("content", text("the fixture failed on purpose")),
                        ("isError", JsonValue::Bool(true)),
                    ])),
                    "slow" => {
                        if let Some(id) = id.as_u64() {
                            slow.insert(id);
                        }
                        continue;
                    }
                    "cancelled_count" => Ok(JsonValue::object([(
                        "content",
                        text(&cancellations.to_string()),
                    )])),
                    "lookup_issue" => Ok(JsonValue::object([(
                        "content",
                        text(&format!(
                            "issue {} is open",
                            arguments.get("id").and_then(JsonValue::as_str).unwrap_or("?")
                        )),
                    )])),
                    "late_tool" => Ok(JsonValue::object([("content", text("late tool ran"))])),
                    "change_tools" => {
                        late_tool = true;
                        let _ = writeln!(
                            output,
                            "{}",
                            JsonValue::object([
                                ("jsonrpc", JsonValue::from("2.0")),
                                ("method", JsonValue::from("notifications/tools/list_changed")),
                            ])
                            .to_json_string()
                            .unwrap_or_default()
                        );
                        Ok(JsonValue::object([("content", text("tools changed"))]))
                    }
                    "crash" => return false,
                    other => Err((-32602_i64, format!("unknown tool {other}"))),
                }
            }
            "ping" => Ok(empty()),
            other => Err((-32601, format!("method {other} not found"))),
        };
        let response = match result {
            Ok(result) => JsonValue::object([
                ("jsonrpc", JsonValue::from("2.0")),
                ("id", id),
                ("result", result),
            ]),
            Err((code, message)) => JsonValue::object([
                ("jsonrpc", JsonValue::from("2.0")),
                ("id", id),
                (
                    "error",
                    JsonValue::object([
                        ("code", JsonValue::from(code)),
                        ("message", JsonValue::from(message)),
                    ]),
                ),
            ]),
        };
        if writeln!(output, "{}", response.to_json_string().unwrap_or_default()).is_err()
            || output.flush().is_err()
        {
            break;
        }
        if method == "initialize" {
            // Exercise a server-initiated request the client must answer.
            let _ = writeln!(
                output,
                "{}",
                JsonValue::object([
                    ("jsonrpc", JsonValue::from("2.0")),
                    ("id", JsonValue::from("server-ping")),
                    ("method", JsonValue::from("ping")),
                ])
                .to_json_string()
                .unwrap_or_default()
            );
            let _ = output.flush();
        }
    }
    true
}
