# e2e

Drives the built binary through the **official** MCP TypeScript SDK, so the
protocol surface is validated by a real client rather than by hand-written
JSON-RPC frames.

```bash
cd tests/e2e
npm install
DEEPSEEK_API_KEY=sk-xxx npm test          # or: node e2e.mjs /path/to/deepseek-mcp.exe
```

What it checks:

| step | expectation |
|---|---|
| `connect` | handshake completes, server reports its name/version/capabilities |
| `listTools` | exactly `web_search` and `web_fetch` are advertised |
| `web_fetch` | returns Markdown for `https://example.com` |
| `web_search` | returns ranked results for a live query |
| bad URL | `isError: true` (model-visible), not a protocol error |
| unknown tool | `isError: true` |
| `close` | process exits cleanly, no hanging stdio pipes |

Costs two DeepSeek API calls per run (~¥0.01).
