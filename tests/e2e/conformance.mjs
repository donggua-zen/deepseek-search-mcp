// Protocol conformance check for the stateless 2026-07-28 revision.
//
// The official @modelcontextprotocol/sdk currently negotiates at most 2025-11-25,
// so it cannot exercise the stateless path at all. This script speaks the wire
// protocol directly and asserts the fields 2026-07-28 makes mandatory.
//
// Run:  node conformance.mjs <path-to-exe>
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const exe = process.argv[2];
if (!exe) {
  console.error("usage: node conformance.mjs <path-to-exe>");
  process.exit(2);
}

const VERSION = "2026-07-28";
const META_VERSION = "io.modelcontextprotocol/protocolVersion";
const META_SERVER = "io.modelcontextprotocol/serverInfo";

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "inherit"] });
const rl = createInterface({ input: child.stdout });
const pending = new Map();
let nextId = 1;

rl.on("line", (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    fail(`server emitted non-JSON: ${line.slice(0, 120)}`);
    return;
  }
  const waiter = pending.get(msg.id);
  if (waiter) {
    pending.delete(msg.id);
    waiter(msg);
  }
});

function fail(msg) {
  console.error(`FAIL: ${msg}`);
  child.kill();
  process.exitCode = 1;
}

function request(method, params = {}) {
  const id = nextId++;
  const frame = { jsonrpc: "2.0", id, method, params };
  child.stdin.write(JSON.stringify(frame) + "\n");
  return new Promise((resolve, reject) => {
    pending.set(id, resolve);
    setTimeout(() => reject(new Error(`timeout waiting for ${method}`)), 15000);
  });
}

// Every request in this revision carries the version in _meta; there is no
// initialize handshake.
const meta = { [META_VERSION]: VERSION };

let failures = 0;
function check(cond, label) {
  if (cond) {
    console.log(`  ok   ${label}`);
  } else {
    console.log(`  FAIL ${label}`);
    failures++;
  }
}

console.log(`protocol conformance against ${VERSION}`);

// 1. server/discover is mandatory for this revision.
const disc = await request("server/discover", { _meta: meta });
check(!disc.error, "server/discover succeeds");
const d = disc.result ?? {};
check(d.resultType === "complete", "discover.resultType === 'complete'");
check(Array.isArray(d.supportedVersions) && d.supportedVersions.includes(VERSION),
  "discover advertises 2026-07-28");
check(d.capabilities?.tools !== undefined, "discover advertises tools capability");

// 2. tools/list must be a cacheable result.
const list = await request("tools/list", { _meta: meta });
check(!list.error, "tools/list succeeds");
const r = list.result ?? {};
check(r.resultType === "complete", "tools/list resultType === 'complete'");
check(typeof r.ttlMs === "number", "tools/list has numeric ttlMs");
check(r.cacheScope === "public" || r.cacheScope === "private", "tools/list has cacheScope");
check(r._meta?.[META_SERVER]?.name !== undefined, "tools/list carries serverInfo in _meta");
check(
  JSON.stringify((r.tools ?? []).map((t) => t.name).sort()) === JSON.stringify(["web_fetch", "web_search"]),
  "tools/list returns web_search + web_fetch"
);

// 3. resources/* and prompts/list are advertised as empty, but still structured.
for (const [method, key] of [
  ["resources/list", "resources"],
  ["resources/templates/list", "resourceTemplates"],
  ["prompts/list", "prompts"],
]) {
  const res = await request(method, { _meta: meta });
  check(!res.error, `${method} succeeds`);
  check(res.result?.resultType === "complete", `${method} resultType === 'complete'`);
  check(Array.isArray(res.result?.[key]), `${method} has ${key} array`);
  check(typeof res.result?.ttlMs === "number", `${method} has ttlMs`);
}

// 4. Methods removed in this revision must be method-not-found, not results.
for (const method of ["ping", "logging/setLevel"]) {
  const res = await request(method, { _meta: meta });
  check(res.error?.code === -32601, `${method} returns -32601 (removed in 2026-07-28)`);
}

// 5. An unimplemented version must produce the typed -32022 error.
const badVer = await request("tools/list", { _meta: { [META_VERSION]: "1999-01-01" } });
check(badVer.error?.code === -32022, "unknown version returns -32022");
check(Array.isArray(badVer.error?.data?.supported), "-32022 carries supported versions");

// 6. tools/call envelope. Local failure is enough; no API spend needed.
const call = await request("tools/call", {
  _meta: meta,
  name: "web_fetch",
  arguments: { url: "" },
});
check(!call.error, "tools/call returns a result (not a protocol error)");
check(call.result?.resultType === "complete", "tools/call resultType === 'complete'");
check(call.result?.isError === true, "empty url surfaces as isError, not a protocol error");

console.log(failures === 0 ? "\nALL CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
child.kill();
process.exit(failures === 0 ? 0 : 1);
