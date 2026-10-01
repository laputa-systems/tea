//! The fake MCP server speaks newline-delimited JSON-RPC on stdio.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn the_fixture_answers_initialize_and_lists_tools_over_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tea-mcp-fixture"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("fixture starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"test","version":"0"}}}}}}"#
    )
    .expect("write");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#)
        .expect("write");
    let mut lines = Vec::new();
    for _ in 0..4 {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read");
        lines.push(line);
    }
    drop(stdin);
    assert_eq!(lines[0].trim(), "fixture: starting");
    assert!(lines[1].contains(r#""protocolVersion":"2025-06-18""#));
    assert!(lines[2].contains(r#""method":"ping""#));
    assert!(lines[3].contains(r#""nextCursor":"page-2""#));
    assert!(child.wait().expect("exit").success());
}
