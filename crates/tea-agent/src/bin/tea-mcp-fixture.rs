//! The fake MCP stdio server used by offline tests and the local example.

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if !tea_agent::mcp_fixture::serve(stdin.lock(), stdout.lock()) {
        // The `crash` tool: exit without answering.
        std::process::exit(3);
    }
}
