// End-to-end check: drive deepseek-mcp through the official MCP TypeScript SDK.
// Run:  node e2e.mjs <path-to-deepseek-mcp.exe>
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const exe = process.argv[2];
if (!exe) {
  console.error("usage: node e2e.mjs <path-to-exe>");
  process.exit(2);
}

const transport = new StdioClientTransport({
  command: exe,
  args: [],
  env: { ...process.env },
});

const client = new Client({ name: "e2e-client", version: "1.0.0" });

const t0 = performance.now();
await client.connect(transport);
console.log(`connected in ${(performance.now() - t0).toFixed(0)} ms`);
console.log("server:", JSON.stringify(client.getServerVersion()));
console.log("caps:", JSON.stringify(client.getServerCapabilities()));
try {
  console.log("instructions:", (client.getInstructions() ?? "").slice(0, 90), "...");
} catch {
  /* older SDK */
}

const { tools } = await client.listTools();
console.log("tools:", tools.map((t) => t.name).join(", "));
for (const t of tools) {
  console.log(` - ${t.name}: ${Object.keys(t.inputSchema.properties ?? {}).join("|")}`);
}

// fetch through the SDK
const t1 = performance.now();
const fetched = await client.callTool({
  name: "web_fetch",
  arguments: { url: "https://example.com" },
});
console.log(
  `web_fetch ok=${!fetched.isError} in ${(performance.now() - t1).toFixed(0)} ms, chars=${fetched.content[0].text.length}`
);

// search through the SDK
const t2 = performance.now();
const searched = await client.callTool({
  name: "web_search",
  arguments: { query: "Model Context Protocol", count: 2 },
});
const text = searched.content[0].text;
console.log(
  `web_search ok=${!searched.isError} in ${(performance.now() - t2).toFixed(0)} ms, chars=${text.length}`
);
console.log("first result line:", text.split("\n")[1]);

// a failing call must surface as isError, not as a protocol crash
const bad = await client.callTool({ name: "web_fetch", arguments: { url: "not-a-url" } });
console.log("invalid url -> isError:", bad.isError, "|", bad.content[0].text.slice(0, 80));

// unknown tool
const unknown = await client.callTool({ name: "nope", arguments: {} });
console.log("unknown tool -> isError:", unknown.isError);

await client.close();
console.log("closed cleanly");
