# deepseek-mcp

[简体中文](README.md) | **English**

A tiny local MCP server that wraps DeepSeek's official **Search / Fetch API**, giving any MCP
client (Claude Code, Cursor, Cherry Studio, Codex, Cline, custom agents…) access to DeepSeek's
web search and page extraction.

> DeepSeek's Search / Fetch is currently baked into its own tooling only, so third-party agents
> cannot reach it. This is the missing bridge.

- Single binary, **~1.8 MB**, no runtime dependencies (no Node / Python / .NET)
- Cold start **~12 ms** (measured initialize round trip: 12–20 ms)
- Only `ureq + serde_json` — no async runtime, no MCP framework
- Speaks both protocol generations: the legacy `initialize` handshake and the stateless
  2026-07-28 `server/discover`
- Prebuilt binaries for macOS (Apple Silicon / Intel), Windows and Linux

## Quick start

### 0. Download a prebuilt binary

Grab the file for your platform from
[Releases](https://github.com/donggua-zen/deepseek-search-mcp/releases/latest). No runtime needed.

| Platform | File |
|---|---|
| macOS Apple Silicon (M1/M2/M3/M4) | `deepseek-search-mcp-macos-arm64` |
| macOS Intel | `deepseek-search-mcp-macos-x64` |
| Windows x64 | `deepseek-search-mcp-windows-x64.exe` |
| Linux x64 | `deepseek-search-mcp-linux-x64` |

Every file ships with a matching `.sha256`.

**First run on macOS** (the binaries are ad-hoc signed, not notarised, so Gatekeeper blocks them):

```bash
chmod +x deepseek-search-mcp-macos-arm64
xattr -d com.apple.quarantine deepseek-search-mcp-macos-arm64
./deepseek-search-mcp-macos-arm64 --version
```

macOS 11.0+ is supported (the floor for Apple Silicon); the Intel build also covers 11.0+.

### 1. Build from source

```bash
# Windows: needs MSVC; the script loads vcvars64 for you
./build.bat release

# macOS / Linux, or any environment with a configured toolchain
cargo build --release
```

Output: `target/release/deepseek-mcp` (`.exe` on Windows)

> If `cargo build` fails with a `link.exe` error on Windows, Git's own `link` has usually shadowed
> the MSVC linker on PATH. Use `build.bat`, which calls vcvars64.bat before cargo.

### 2. Configure the API key

Three ways, highest priority first:

```bash
export DEEPSEEK_API_KEY=sk-xxx                        # environment variable
deepseek-mcp --key sk-xxx                             # command line
echo '{"api_key":"sk-xxx"}' > ~/.deepseek-mcp.json    # config file
```

### 3. Wire it into a client

**Claude Code**

```bash
claude mcp add deepseek-search --scope user -- /absolute/path/to/deepseek-mcp
```

**Cursor / Cherry Studio / Cline etc.** (JSON config)

```json
{
  "mcpServers": {
    "deepseek-search": {
      "command": "/absolute/path/to/deepseek-mcp",
      "env": { "DEEPSEEK_API_KEY": "sk-xxx" }
    }
  }
}
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.deepseek-search]
command = "/absolute/path/to/deepseek-mcp"
env = { DEEPSEEK_API_KEY = "sk-xxx" }
```

## Tools

### `web_search`

| Parameter | Type | Description |
|---|---|---|
| `query` | string | **Required**, 1–128 characters. Search operators can be embedded inline |
| `count` | int | 1–50, default 10 |
| `need_content` | bool | Also return page bodies, default false |
| `ori_question` | string | The user's original question, helps the engine read intent |
| `site` / `exclude_site` | string \| array | Restrict / exclude domains, OR-ed together |
| `lang` | string | `zh` or `zh,en` |
| `after` / `before` | string | `2025` or `2025-01-01` |

`site/lang/after/before` are folded into `query`; if you already wrote the same operator by hand
inside `query`, yours wins.

Supported operators (passed straight through to DeepSeek):

```
site:github.com      -site:example.com     -advertisement
-"release notes"     "exact phrase"        lang:zh,en
after:2025-01-01     before:2026-06-01
```

Output is Markdown written for a model to read: numbered heading + URL + (site · date · relevance ·
authority) + long summary, plus an optional `<content>` body block. Total body text is capped by a
character budget so one huge page cannot flood the context.

### `web_fetch`

`url` → clean Markdown. DeepSeek's extractor returns heavy `\/`, `\#`, `\|` escaping; this server
unescapes it and demotes the page's own `#` headings so they don't collide with the result headings.

## Protocol compatibility

The server supports both protocol generations and **picks the result shape by negotiated version**
instead of bolting every field onto every response:

| | 2024-11-05 … 2025-11-25 (legacy) | 2026-07-28 (stateless) |
|---|---|---|
| Handshake | `initialize` + `notifications/initialized` | none; version travels in `_meta` per request |
| Version passing | negotiated once in `initialize` | `_meta["io.modelcontextprotocol/protocolVersion"]` |
| `resultType` | **must not** be sent | **required**, `"complete"` |
| List results | array field only | also needs `ttlMs` + `cacheScope` (`CacheableResult`) |
| `serverInfo` | returned in `initialize` | in every result's `_meta` |
| `ping` / `logging/setLevel` | answered normally | removed → `-32601` |
| Unknown version | — | `-32022` + supported version list |

**Why not just always send `resultType`**: legacy clients validate the `ping` result against a
*strict* empty-object schema, so one extra field is a parse failure. The fields must be gated on the
negotiated version.

`tests/e2e/conformance.mjs` asserts these 29 behaviours at the wire level and runs in CI with a
dummy key, so it costs nothing.

## Why Rust

| Approach | Cold start | Distribution | Verdict |
|---|---|---|---|
| **Rust single binary** | ~12 ms | one binary | ✅ best fit for a local long-lived MCP |
| Node + official SDK | ~80–150 ms | needs node + node_modules | fast to build, but an environment to maintain |
| Python + mcp | ~300 ms+ | needs a venv | heaviest dependencies |
| Rust + `rmcp` framework | same | one binary | complete, but overkill for two tools |

MCP's stdio transport is just "one JSON per line". This server needs four methods, so hand-rolling
is more controllable than pulling in a framework — and it makes dual-protocol support easy.

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `DEEPSEEK_API_KEY` | — | API key |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com` | point at a proxy gateway if needed |
| `DEEPSEEK_MCP_TIMEOUT` | `60` | per-request timeout in seconds |
| `DEEPSEEK_MCP_MAX_CHARS` | `30000` | cap on page body handed to the model (characters) |
| `DEEPSEEK_MCP_COUNT` | `10` | default result count |
| `DEEPSEEK_MCP_DEBUG` | — | set to 1 to log protocol traffic and request bodies on stderr |
| `HTTP_PROXY` / `HTTPS_PROXY` | — | honoured by the HTTP client |

## Command-line smoke tests (no client needed)

```bash
deepseek-mcp search "Rust MCP server" --count 3
deepseek-mcp search "mcp" --content        # include page bodies
deepseek-mcp fetch https://example.com
deepseek-mcp                               # no subcommand = stdio MCP server
```

## Errors and billing

- Tool-level failures (bad URL, no content, insufficient balance) come back as a **normal result**
  with `isError: true`, so the model sees it and can self-correct. Only cases like an unknown method
  become JSON-RPC errors.
- 408 / 429 / 5xx retry with backoff, up to 3 attempts; 4xx does not retry.
- DeepSeek bills per **successful request** (Search ¥5 per 1k, Fetch ¥5 per 1k). `count` does not
  change the unit price, but raising it wastes context for nothing.
- Account-level rate limits: Search 100 QPS, Fetch 25 QPS.

## Development

```bash
cargo test --release      # 12 unit tests
cargo clippy              # zero warnings

# Wire-protocol conformance (no API key, 29 assertions, no cost)
node tests/e2e/conformance.mjs target/release/deepseek-mcp

# End-to-end through the official SDK (needs a real key, ~2 billable requests)
cd tests/e2e && npm install && DEEPSEEK_API_KEY=sk-xxx npm test
```

> The official `@modelcontextprotocol/sdk` currently negotiates at most `2025-11-25`, so it
> **cannot exercise the 2026-07-28 stateless path**. That is exactly why `conformance.mjs` exists:
> it speaks the wire format directly.

> On Windows use `./build.bat test` instead of `cargo test` (see the link.exe note above).
>
> Known environment issue: some security software blocks the test process from starting
> (`os error 5`). Run `cargo test --no-run`, then execute
> `target/debug/deps/deepseek_mcp-*.exe` directly.

Code layout (`src/main.rs`, single file):

```
Config/Client     config loading and ureq calls (with retries)
render_*          DeepSeek JSON -> Markdown for the model
ResultBuilder     single envelope for every result (resultType / ttlMs / cacheScope)
tools()           inputSchema for both tools
dispatch()        MCP method routing
serve()           stdio newline-delimited JSON-RPC loop
run_cli()         search / fetch subcommands
```

## License

MIT
