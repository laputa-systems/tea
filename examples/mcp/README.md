# Local MCP example

This example connects `tea` to the repository's fake MCP stdio server. It
needs no network, account, or credentials.

1. Build the fake server:

   ```sh
   cargo build -p tea-agent --features mcp-fixture --bin tea-mcp-fixture
   ```

2. Add it to `$TEA_HOME/config.toml` (adjust the path to your checkout):

   ```toml
   [mcp.servers.tracker]
   command = "/path/to/tea/target/debug/tea-mcp-fixture"
   ```

   See [config.toml](config.toml) for every supported key.

3. Start `tea` and ask, for example, "Is issue TEA-1 open? Use the issue
   tracker." The model sees `tool_search`, finds
   `mcp__tracker__lookup_issue`, and calls it from its next step.

The server process belongs to the attached session's runtime: it starts in
the background when the session needs tools, and it is shut down (input
closed, then killed after two seconds) when `tea` exits. A crashed server's
tools disappear from the next request and `tea` shows a notice; restart `tea`
to reconnect.
