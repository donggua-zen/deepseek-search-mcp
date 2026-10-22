//! deepseek-mcp — a tiny local MCP server that exposes DeepSeek Search / Fetch API
//! to any MCP-capable client (Claude Code, Cursor, Cherry Studio, Codex, ...).
//!
//! Design goals:
//!   * zero async runtime, zero MCP framework, one static-ish binary
//!   * stdio transport, newline-delimited JSON-RPC 2.0
//!   * speaks both the legacy `initialize` handshake and the stateless
//!     2026-07-28 protocol (`server/discover`, per-request `_meta`)
//!   * all diagnostics go to stderr, stdout carries protocol frames only

use serde_json::{json, Map, Value};
use std::io::{self, BufRead, BufWriter, Write};
use std::time::Duration;

const SERVER_NAME: &str = "deepseek-search";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_CHARS: usize = 30_000;
const DEFAULT_COUNT: u32 = 10;

/// Newest first. The server accepts every version it knows and echoes the
/// client's choice back, so old and new clients both keep working.
const SUPPORTED_VERSIONS: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// Reserved by the spec for `UnsupportedProtocolVersionError` (SEP-2575).
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "\
This server provides live web access through the DeepSeek Search and Fetch APIs.

- `web_search` returns ranked web results (title, url, snippet, ~700 char summary,
  optional full page text). It understands search operators inline in `query`:
  site:, -site:, -, -\"phrase\", \"phrase\", lang:, after:, before:.
  Prefer `web_search` whenever the answer depends on recent or external facts.
- `web_fetch` turns one URL into clean Markdown. Use it after a search when the
  summary is not enough, or when the user hands you a link to read.

Practical guidance: keep `count` small (3-8) unless you really need breadth;
set `need_content` only for the one or two results you intend to read closely.
";

fn log(args: std::fmt::Arguments) {
    if std::env::var_os("DEEPSEEK_MCP_DEBUG").is_some() {
        let _ = writeln!(io::stderr(), "{}", args);
    }
}

macro_rules! debug_log {
    ($($t:tt)*) => { log(format_args!($($t)*)) };
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

struct Config {
    api_key: String,
    base_url: String,
    timeout: Duration,
    max_chars: usize,
    default_count: u32,
}

impl Config {
    fn from_env_and_args(args: &[String]) -> Result<Self, String> {
        let api_key = flag_value(args, "--key")
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
            .or_else(config_file_key)
            .ok_or_else(|| {
                "missing DeepSeek API key: set DEEPSEEK_API_KEY, pass --key, \
                 or put {\"api_key\": \"sk-...\"} in ~/.deepseek-mcp.json"
                    .to_string()
            })?;

        let base_url = flag_value(args, "--base-url")
            .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let timeout = std::env::var("DEEPSEEK_MCP_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let max_chars = std::env::var("DEEPSEEK_MCP_MAX_CHARS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CHARS);

        let default_count = std::env::var("DEEPSEEK_MCP_COUNT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v.clamp(1, 50))
            .unwrap_or(DEFAULT_COUNT);

        Ok(Self {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout,
            max_chars,
            default_count,
        })
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
}

fn config_file_key() -> Option<String> {
    let path = home_dir()?.join(".deepseek-mcp.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("api_key")?.as_str().map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// DeepSeek API client
// ---------------------------------------------------------------------------

struct Client {
    agent: ureq::Agent,
    cfg: Config,
}

#[derive(Debug)]
enum ApiError {
    /// Bad arguments supplied by the caller; never retried.
    Invalid(String),
    Transport(String),
    Status(u16, String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Invalid(m) => write!(f, "invalid arguments: {m}"),
            ApiError::Transport(m) => write!(f, "network error: {m}"),
            ApiError::Status(code, msg) => write!(f, "HTTP {code}: {msg}"),
        }
    }
}

impl Client {
    fn new(cfg: Config) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(cfg.timeout))
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            cfg,
        }
    }

    /// POST a JSON body with up to 3 attempts on 408/429/5xx.
    fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        let url = format!("{}{}", self.cfg.base_url, path);
        let mut last: Option<ApiError> = None;

        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(600 * 2u64.pow(attempt - 1)));
            }
            let resp = self
                .agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .send_json(body);

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last = Some(ApiError::Transport(e.to_string()));
                    continue;
                }
            };

            let status = resp.status().as_u16();
            let mut resp = resp;
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| ApiError::Transport(e.to_string()))?;

            if (200..300).contains(&status) {
                return serde_json::from_str(&text)
                    .map_err(|e| ApiError::Transport(format!("invalid JSON response: {e}")));
            }

            let retryable = status == 408 || status == 429 || status >= 500;
            last = Some(ApiError::Status(status, extract_error(&text)));
            if !retryable {
                break;
            }
            debug_log!("retrying after HTTP {status} (attempt {})", attempt + 1);
        }

        Err(last.unwrap_or_else(|| ApiError::Transport("unknown failure".into())))
    }

    fn search(&self, params: &Value) -> Result<Value, ApiError> {
        let mut body = Map::new();
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(ApiError::Invalid("`query` must not be empty".into()));
        }

        let count = params
            .get("count")
            .and_then(Value::as_u64)
            .map(|v| (v as u32).clamp(1, 50))
            .unwrap_or(self.cfg.default_count);

        body.insert("query".into(), json!(with_filters(&query, params)));
        body.insert("count".into(), json!(count));
        body.insert(
            "need_content".into(),
            json!(params
                .get("need_content")
                .and_then(Value::as_bool)
                .unwrap_or(false)),
        );
        if let Some(q) = params.get("ori_question").and_then(Value::as_str) {
            if !q.trim().is_empty() {
                body.insert("ori_question".into(), json!(q.trim()));
            }
        }
        if let Some(loc) = params.get("localization").and_then(Value::as_str) {
            if !loc.trim().is_empty() {
                body.insert("localization".into(), json!(loc.trim()));
            }
        }

        debug_log!("search body: {}", Value::Object(body.clone()));
        self.post("/v1/tools/search", &Value::Object(body))
    }

    fn fetch(&self, params: &Value) -> Result<Value, ApiError> {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if url.is_empty() {
            return Err(ApiError::Invalid("`url` must not be empty".into()));
        }
        let body = json!({ "url": url });
        self.post("/v1/tools/fetch", &body)
    }
}

/// Fold convenience filters into the raw query, unless the caller already
/// wrote the operator by hand.
fn with_filters(query: &str, params: &Value) -> String {
    let mut clauses: Vec<String> = Vec::new();

    // Returns the domains of a "site"/"exclude_site" argument, accepting either
    // a string (comma or space separated) or an array of strings.
    fn domains(value: Option<&Value>) -> Vec<String> {
        match value {
            Some(Value::String(s)) => s
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    if !query.contains("site:") {
        let include = domains(params.get("site"));
        if !include.is_empty() {
            let terms: Vec<String> = include.iter().map(|d| format!("site:{d}")).collect();
            // DeepSeek requires multiple include sites to be OR-ed.
            clauses.push(terms.join(" OR "));
        }
        let exclude = domains(params.get("exclude_site"));
        if !exclude.is_empty() {
            let terms: Vec<String> = exclude.iter().map(|d| format!("-site:{d}")).collect();
            clauses.push(terms.join(" "));
        }
    }

    for (key, op) in [
        ("lang", "lang:"),
        ("after", "after:"),
        ("before", "before:"),
    ] {
        if query.contains(op) {
            continue;
        }
        if let Some(v) = params.get(key).and_then(Value::as_str) {
            let v = v.trim();
            if !v.is_empty() {
                clauses.push(format!("{op}{v}"));
            }
        }
    }

    if clauses.is_empty() {
        query.to_string()
    } else {
        format!("{query} {}", clauses.join(" "))
    }
}

fn extract_error(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return msg.to_string();
        }
        if let Some(msg) = v.get("message").and_then(Value::as_str) {
            return msg.to_string();
        }
    }
    let t = text.trim();
    if t.is_empty() {
        "(empty body)".to_string()
    } else {
        t.chars().take(400).collect()
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// DeepSeek's extractor escapes Markdown punctuation (`\/`, `\#`, `\|`, `\-`, ...).
/// The result is technically valid Markdown but noisy and token-expensive for a
/// model to read, so drop those escapes. Only ASCII punctuation is touched, and a
/// doubled backslash is preserved as a literal backslash.
fn unescape_markdown(s: &str) -> String {
    const PUNCT: &str = "\\`*_{}[]()#+-.!|:/><\"'~$%=&,";
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some(&n) if PUNCT.contains(n) => {
                    out.push(n);
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Page text often carries its own `#` headings, which would compete with the
/// result headings this server emits (`### 1. Title`). Push them down by `delta`
/// levels so the document outline stays unambiguous for the model reading it.
fn demote_headings(s: &str, delta: usize) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        let followed_by_space = line[hashes..]
            .chars()
            .next()
            .is_some_and(|c| c == ' ' || c == '\t');
        if (1..=6).contains(&hashes) && followed_by_space {
            let level = (hashes + delta).min(6);
            for _ in 0..level {
                out.push('#');
            }
            out.push_str(&line[hashes..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

fn truncate(s: &str, budget: usize) -> (String, bool) {
    if s.chars().count() <= budget {
        return (s.to_string(), false);
    }
    let cut: String = s.chars().take(budget).collect();
    (cut, true)
}

fn render_search(payload: &Value, max_chars: usize) -> String {
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if results.is_empty() {
        return "No results found. Try a broader query or drop restrictive filters.".to_string();
    }

    let mut out = String::new();
    // Shared budget for page bodies so one huge page cannot flood the context.
    let mut content_budget = max_chars;

    for (i, r) in results.iter().enumerate() {
        let get = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().trim();
        let title = if get("title").is_empty() {
            "(untitled)"
        } else {
            get("title")
        };
        out.push_str(&format!("### {}. {}\n{}\n", i + 1, title, get("url")));

        let mut meta: Vec<String> = Vec::new();
        if !get("site_name").is_empty() {
            meta.push(get("site_name").to_string());
        }
        if !get("date").is_empty() {
            meta.push(get("date").chars().take(10).collect());
        }
        if let Some(score) = r.get("rerank_score").and_then(Value::as_f64) {
            meta.push(format!("relevance {score:.2}"));
        }
        if let Some(auth) = r.get("authority").and_then(Value::as_i64) {
            if auth >= 0 {
                meta.push(format!("authority {auth}/4"));
            }
        }
        if !meta.is_empty() {
            out.push_str(&meta.join(" · "));
            out.push('\n');
        }

        let body = if get("summary").is_empty() {
            get("snippet")
        } else {
            get("summary")
        };
        if !body.is_empty() {
            out.push_str(&demote_headings(&unescape_markdown(body), 4));
            out.push('\n');
        }

        let content = get("content");
        if !content.is_empty() && content_budget > 0 {
            let cleaned = demote_headings(&unescape_markdown(content), 3);
            let (text, cut) = truncate(&cleaned, content_budget);
            content_budget -= text.chars().count();
            out.push_str("\n<content>\n");
            out.push_str(&text);
            if cut || content_budget == 0 {
                out.push_str("\n…[content truncated]");
            }
            out.push_str("\n</content>\n");
        }

        out.push('\n');
    }

    out.trim_end().to_string()
}

fn render_fetch(payload: &Value, max_chars: usize) -> String {
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (text, cut) = truncate(&unescape_markdown(content), max_chars);

    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("# {title}\n\n"));
    }
    out.push_str(&text);
    if cut {
        out.push_str("\n\n…[content truncated]");
    }
    out
}

// ---------------------------------------------------------------------------
// tool definitions
// ---------------------------------------------------------------------------

fn tools() -> Value {
    json!([
        {
            "name": "web_search",
            "title": "Web search (DeepSeek)",
            "description": "Search the live web with DeepSeek's own search engine and get ranked results. \
    Each result carries a title, url, short snippet, and a ~700 character summary written for models to read. \
    Set need_content=true only for the results you actually intend to read in full.\n\
    Search operators can be embedded directly in `query`: \
    site:github.com, -site:example.com, -\"phrase\", \"exact phrase\", lang:zh,en, after:2025-01-01, before:2026-06-01.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search keywords, 1-128 characters. Inline operators such as site:, lang:, after:, before:, -word are passed straight to the search backend."
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 10,
                        "description": "Number of results to return (1-50). Billing is per request, not per result, but large counts waste context."
                    },
                    "need_content": {
                        "type": "boolean",
                        "default": false,
                        "description": "Also return the extracted page body for each result. Verbose; use sparingly."
                    },
                    "ori_question": {
                        "type": "string",
                        "description": "The user's original question, which helps the engine understand search intent."
                    },
                    "site": {
                        "description": "Restrict results to these domains, e.g. \"github.com\" or [\"github.com\",\"huggingface.co\"] (OR-ed together). Ignored if `query` already contains site:."
                    },
                    "exclude_site": {
                        "description": "Exclude these domains, e.g. \"pinterest.com\" or [\"a.com\",\"b.com\"]."
                    },
                    "lang": {
                        "type": "string",
                        "description": "Restrict language, e.g. \"zh\" or \"zh,en\"."
                    },
                    "after": {
                        "type": "string",
                        "description": "Only pages published after this date, e.g. \"2025\" or \"2025-01-01\"."
                    },
                    "before": {
                        "type": "string",
                        "description": "Only pages published before this date, e.g. \"2026-06-01\"."
                    }
                },
                "required": ["query"]
            },
            "annotations": {
                "readOnlyHint": true,
                "openWorldHint": true
            }
        },
        {
            "name": "web_fetch",
            "title": "Fetch web page (DeepSeek)",
            "description": "Fetch one HTTP/HTTPS URL and return its main text as clean Markdown. \
    Use it to read an article you found via web_search, or a link the user gave you. \
    Fails on pages that cannot be extracted (paywalls, login walls, non-HTML).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http(s) URL to read. Must include a host; credentials in the URL are rejected."
                    }
                },
                "required": ["url"]
            },
            "annotations": {
                "readOnlyHint": true,
                "openWorldHint": true
            }
        }
    ])
}

fn call_tool(client: &Client, name: &str, args: &Value) -> Value {
    let result = match name {
        "web_search" => client
            .search(args)
            .map(|payload| render_search(&payload, client.cfg.max_chars)),
        "web_fetch" => client
            .fetch(args)
            .map(|payload| render_fetch(&payload, client.cfg.max_chars)),
        other => {
            return json!({
                "content": [{ "type": "text", "text": format!("Unknown tool: {other}") }],
                "isError": true
            })
        }
    };

    match result {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("{name} failed: {e}") }],
            "isError": true
        }),
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC / MCP dispatch
// ---------------------------------------------------------------------------

fn capabilities() -> Value {
    json!({ "tools": { "listChanged": false } })
}

fn server_info() -> Value {
    json!({ "name": SERVER_NAME, "version": SERVER_VERSION })
}

const STATELESS_VERSION: &str = "2026-07-28";
const KEY_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const KEY_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// True for the stateless revision, which mandates `resultType` on every result
/// and `ttlMs` / `cacheScope` on every list result.
///
/// Older revisions must NOT get these fields: a 2025-11-25 client validates
/// `ping`'s result against `EmptyResultSchema`, which is a *strict* object, so
/// an unexpected `resultType` key is a hard parse failure. The spec never asks
/// servers to retro-fit the field onto earlier revisions, so gate on version.
fn is_stateless(version: &str) -> bool {
    version == STATELESS_VERSION
}

/// Which protocol revision governs this request.
///
/// Stateless clients send it on every request in `_meta`; legacy clients
/// negotiated it once during `initialize`, so we fall back to the session
/// value in that case.
fn effective_version(params: &Value, session: &str) -> String {
    params
        .get("_meta")
        .and_then(|m| m.get(KEY_PROTOCOL_VERSION))
        .and_then(Value::as_str)
        .unwrap_or(session)
        .to_string()
}

fn negotiate_version(params: &Value) -> String {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("_meta")
                .and_then(|m| m.get(KEY_PROTOCOL_VERSION))
                .and_then(Value::as_str)
        });

    match requested {
        Some(v) if SUPPORTED_VERSIONS.contains(&v) => v.to_string(),
        // Unknown/newer version: answer with our newest so the client can retry.
        Some(_) => SUPPORTED_VERSIONS[0].to_string(),
        None => SUPPORTED_VERSIONS[0].to_string(),
    }
}

/// Single choke point for every result the server emits.
///
/// Fields mandated by 2026-07-28 live here rather than in each `match` arm, so
/// forgetting one is structurally impossible instead of a per-branch risk.
struct ResultBuilder {
    modern: bool,
}

impl ResultBuilder {
    fn new(version: &str) -> Self {
        Self {
            modern: is_stateless(version),
        }
    }

    /// Attach `resultType` (and `serverInfo`) when the revision requires it.
    fn seal(&self, mut body: Map<String, Value>) -> Value {
        if self.modern {
            body.insert("resultType".into(), json!("complete"));
            let meta = body
                .entry("_meta")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(m) = meta.as_object_mut() {
                m.insert(KEY_SERVER_INFO.into(), server_info());
            }
        }
        Value::Object(body)
    }

    /// A plain result: just `resultType` + `_meta`.
    fn complete(&self, body: Value) -> Value {
        self.seal(as_object(body))
    }

    /// A list result. These are `CacheableResult`s in 2026-07-28, where
    /// `ttlMs` and `cacheScope` are required, not optional.
    ///
    /// The tool list is compiled into the binary and cannot change while the
    /// process runs, so a long TTL with public scope is accurate.
    fn list(&self, body: Value) -> Value {
        let mut obj = as_object(body);
        if self.modern {
            obj.insert("ttlMs".into(), json!(86_400_000));
            obj.insert("cacheScope".into(), json!("public"));
        }
        self.seal(obj)
    }
}

fn as_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    }
}

fn handle_initialize(params: &Value) -> Value {
    let version = negotiate_version(params);
    json!({
        "protocolVersion": version,
        "capabilities": capabilities(),
        "serverInfo": server_info(),
        "instructions": INSTRUCTIONS
    })
}

/// Body only: `ResultBuilder::list` adds `resultType` / `ttlMs` / `cacheScope`
/// and the `serverInfo` meta, since discover is a cacheable result too.
fn handle_discover() -> Value {
    json!({
        "supportedVersions": SUPPORTED_VERSIONS,
        "capabilities": capabilities(),
        "instructions": INSTRUCTIONS,
    })
}

/// Dispatch one request. `session` carries the version negotiated at
/// `initialize`, which legacy clients rely on because they send no `_meta`.
///
/// Returns `None` for unknown methods so the caller can answer with -32601.
fn dispatch(client: &Client, method: &str, params: &Value, session: &str) -> Option<Value> {
    let version = effective_version(params, session);
    let rb = ResultBuilder::new(&version);

    let out = match method {
        "initialize" => {
            // Legacy handshake: not a stateless request, so no resultType even
            // when the negotiated version is a legacy one.
            return Some(handle_initialize(params));
        }
        "server/discover" => rb.list(handle_discover()),
        "tools/list" => rb.list(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let empty = json!({});
            let args = params.get("arguments").unwrap_or(&empty);
            debug_log!("tools/call {name} {args}");
            // A tool error is a normal result carrying isError, so the model
            // can see it; only transport-level faults become JSON-RPC errors.
            rb.complete(call_tool(client, name, args))
        }
        // Removed in 2026-07-28 (SEP-2575), so a stateless client must get
        // method-not-found rather than a result it cannot parse. Older clients
        // still probe with these, and validate the empty object strictly.
        "ping" | "logging/setLevel" => {
            if rb.modern {
                return None;
            }
            json!({})
        }
        "resources/list" => rb.list(json!({ "resources": [] })),
        "resources/templates/list" => rb.list(json!({ "resourceTemplates": [] })),
        "prompts/list" => rb.list(json!({ "prompts": [] })),
        _ => return None,
    };
    Some(out)
}

fn write_frame(out: &mut impl Write, value: &Value) {
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn serve(client: Client) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut reader = io::BufReader::new(stdin.lock());
    let mut line = String::new();
    // Version agreed at `initialize`. Legacy clients send no `_meta`, so this is
    // how their later requests learn which result shape to emit.
    let mut session = SUPPORTED_VERSIONS[0].to_string();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // client closed the pipe
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                debug_log!("bad JSON frame: {e}");
                write_frame(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    }),
                );
                continue;
            }
        };

        let id = msg.get("id").cloned();
        let method = match msg.get("method").and_then(Value::as_str) {
            Some(m) => m.to_string(),
            None => {
                debug_log!("frame without method, ignored");
                continue;
            }
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        debug_log!("<- {method} (id={:?})", id);

        // Notifications carry no id and never get a response.
        if id.is_none() {
            continue;
        }

        let version = effective_version(&params, &session);
        // A stateless client that names a version we do not implement gets a
        // typed error it can act on, instead of silently mangled results.
        if method != "initialize"
            && !SUPPORTED_VERSIONS.contains(&version.as_str())
            && params.get("_meta").is_some()
        {
            write_frame(
                &mut out,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": UNSUPPORTED_PROTOCOL_VERSION,
                        "message": format!("Unsupported protocol version: {version}"),
                        "data": { "supported": SUPPORTED_VERSIONS, "requested": version }
                    }
                }),
            );
            continue;
        }

        if method == "initialize" {
            session = negotiate_version(&params);
        }

        match dispatch(&client, &method, &params, &session) {
            Some(result) => write_frame(
                &mut out,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            ),
            None => write_frame(
                &mut out,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") }
                }),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI mode (handy for smoke tests without an MCP client)
// ---------------------------------------------------------------------------

const HELP: &str = "\
deepseek-mcp — DeepSeek Search/Fetch as a local MCP server

USAGE
  deepseek-mcp                       run the MCP server on stdio (default)
  deepseek-mcp search \"query\" [--count N] [--content]
  deepseek-mcp fetch <url>           one-shot fetch, prints to terminal
  deepseek-mcp --help | --version

ENVIRONMENT
  DEEPSEEK_API_KEY       API key (or --key, or ~/.deepseek-mcp.json {\"api_key\":\"...\"})
  DEEPSEEK_BASE_URL      default https://api.deepseek.com
  DEEPSEEK_MCP_TIMEOUT   per-request timeout seconds, default 60
  DEEPSEEK_MCP_MAX_CHARS characters of page body handed to the model, default 30000
  DEEPSEEK_MCP_COUNT     default result count, default 10
  DEEPSEEK_MCP_DEBUG     set to 1 to log protocol traffic on stderr
  HTTP_PROXY / HTTPS_PROXY  honoured by the HTTP client
";

fn run_cli(client: &Client, args: &[String]) -> bool {
    // Flags that consume the following token; other tokens starting with '-'
    // are boolean switches. `--` ends flag parsing.
    const VALUE_FLAGS: [&str; 3] = ["--key", "--base-url", "--count"];
    let mut sub: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            sub = (i + 1 < args.len()).then_some(i + 1);
            break;
        }
        if VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') {
            sub = Some(i);
            break;
        }
        i += 1;
    }

    let cmd = sub.map(|i| args[i].as_str()).unwrap_or_default();
    let operand = sub.and_then(|i| args.get(i + 1)).map(String::as_str);

    let code = match cmd {
        "search" => {
            let query = operand.unwrap_or_default().to_string();
            let mut params = json!({
                "query": query,
                "need_content": args.iter().any(|a| a == "--content"),
            });
            if let Some(c) = flag_value(args, "--count").and_then(|v| v.parse::<u32>().ok()) {
                params["count"] = json!(c);
            }
            match client.search(&params) {
                Ok(payload) => {
                    println!("{}", render_search(&payload, client.cfg.max_chars));
                    0
                }
                Err(e) => {
                    eprintln!("search failed: {e}");
                    1
                }
            }
        }
        "fetch" => {
            let url = operand.unwrap_or_default().to_string();
            match client.fetch(&json!({ "url": url })) {
                Ok(payload) => {
                    println!("{}", render_fetch(&payload, client.cfg.max_chars));
                    0
                }
                Err(e) => {
                    eprintln!("fetch failed: {e}");
                    1
                }
            }
        }
        // No subcommand: stay in MCP server mode.
        _ => return false,
    };

    std::process::exit(code);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{SERVER_NAME} {SERVER_VERSION}");
        return;
    }

    let cfg = match Config::from_env_and_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("deepseek-mcp: {e}");
            std::process::exit(2);
        }
    };

    let client = Client::new(cfg);

    // CLI mode is entered only when the first positional token is a subcommand,
    // so a search query that happens to contain the word "search" stays in MCP mode.
    if run_cli(&client, &args) {
        return;
    }

    debug_log!(
        "{SERVER_NAME} {SERVER_VERSION} ready on stdio (base={})",
        client.cfg.base_url
    );

    if let Err(e) = serve(client) {
        eprintln!("deepseek-mcp: stdio error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_are_appended_once() {
        let got = with_filters(
            "rust mcp",
            &json!({ "site": "github.com", "lang": "en", "after": "2025" }),
        );
        assert_eq!(got, "rust mcp site:github.com lang:en after:2025");

        let got = with_filters("rust site:crates.io", &json!({ "site": "github.com" }));
        assert_eq!(got, "rust site:crates.io");
    }

    #[test]
    fn multiple_sites_are_or_ed() {
        let got = with_filters("x", &json!({ "site": ["a.com", "b.com"] }));
        assert_eq!(got, "x site:a.com OR site:b.com");
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(
            negotiate_version(&json!({"protocolVersion": "2025-06-18"})),
            "2025-06-18"
        );
        assert_eq!(
            negotiate_version(&json!({"protocolVersion": "1999-01-01"})),
            SUPPORTED_VERSIONS[0]
        );
        assert_eq!(
            negotiate_version(
                &json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2025-11-25"}})
            ),
            "2025-11-25"
        );
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        let (s, cut) = truncate("中文中文中文", 3);
        assert_eq!(s, "中文中");
        assert!(cut);
    }

    #[test]
    fn page_headings_are_demoted_below_result_headings() {
        let got = demote_headings("# Title\n## Sub\nnot # a heading\n####### too deep", 3);
        assert_eq!(
            got,
            "#### Title\n##### Sub\nnot # a heading\n####### too deep"
        );
    }

    #[test]
    fn markdown_escapes_are_cleaned() {
        assert_eq!(unescape_markdown(r"a\/b \# x \| y"), "a/b # x | y");
        // double backslash stays a literal backslash
        assert_eq!(unescape_markdown(r"c:\\d"), r"c:\d");
        // non punctuation escapes are left alone
        assert_eq!(unescape_markdown(r"line\nbreak"), r"line\nbreak");
    }

    // --- 2026-07-28 result envelope ---------------------------------------

    #[test]
    fn stateless_results_carry_result_type() {
        let rb = ResultBuilder::new("2026-07-28");
        let out = rb.complete(json!({ "content": [], "isError": false }));
        assert_eq!(out["resultType"], "complete");
        assert_eq!(out["_meta"][KEY_SERVER_INFO]["name"], SERVER_NAME);
    }

    #[test]
    fn list_results_carry_ttl_and_cache_scope() {
        let rb = ResultBuilder::new("2026-07-28");
        let out = rb.list(json!({ "tools": [] }));
        // Both are required by CacheableResult, not optional.
        assert!(out["ttlMs"].is_number(), "ttlMs must be present");
        assert_eq!(out["cacheScope"], "public");
        assert_eq!(out["resultType"], "complete");
    }

    #[test]
    fn legacy_results_stay_bare() {
        // A 2025-11-25 client validates ping against a *strict* empty schema,
        // so retro-fitting resultType onto older revisions breaks them.
        let rb = ResultBuilder::new("2025-11-25");
        let out = rb.complete(json!({}));
        assert!(out.get("resultType").is_none());
        assert!(out.get("_meta").is_none());
        assert!(out.as_object().unwrap().is_empty());

        let out = rb.list(json!({ "tools": [] }));
        assert!(out.get("ttlMs").is_none());
        assert!(out.get("cacheScope").is_none());
    }

    #[test]
    fn version_is_read_from_meta_per_request() {
        // Stateless clients put the version on every request instead of
        // negotiating once via initialize.
        let params = json!({
            "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" }
        });
        assert_eq!(effective_version(&params, "2025-06-18"), "2026-07-28");

        // Legacy clients send nothing, so the session value wins.
        assert_eq!(effective_version(&json!({}), "2025-06-18"), "2025-06-18");
    }

    #[test]
    fn every_documented_method_gets_a_compliant_envelope() {
        // Guards the choke point itself: if a new method is added without going
        // through ResultBuilder, this catches it.
        let cfg = Config {
            api_key: "sk-test".into(),
            base_url: "https://example.invalid".into(),
            timeout: Duration::from_millis(1),
            max_chars: 100,
            default_count: 3,
        };
        let client = Client::new(cfg);

        for (method, params) in [
            ("tools/list", json!({})),
            ("server/discover", json!({})),
            ("resources/list", json!({})),
            ("resources/templates/list", json!({})),
            ("prompts/list", json!({})),
        ] {
            let result = dispatch(&client, method, &params, "2026-07-28")
                .unwrap_or_else(|| panic!("{method} should be implemented"));
            assert_eq!(
                result["resultType"], "complete",
                "{method} must carry resultType on 2026-07-28"
            );
        }

        // initialize is the one exception: it is not a stateless request.
        let init = dispatch(&client, "initialize", &json!({}), "2026-07-28").unwrap();
        assert!(init.get("resultType").is_none());
    }

    #[test]
    fn removed_methods_are_not_found_on_modern_protocol() {
        let cfg = Config {
            api_key: "sk-test".into(),
            base_url: "https://example.invalid".into(),
            timeout: Duration::from_millis(1),
            max_chars: 100,
            default_count: 3,
        };
        let client = Client::new(cfg);

        // ping and logging/setLevel were dropped in 2026-07-28, so answering
        // them would hand the client a result it cannot parse.
        for method in ["ping", "logging/setLevel"] {
            assert!(
                dispatch(&client, method, &json!({}), "2026-07-28").is_none(),
                "{method} must be method-not-found on 2026-07-28"
            );
            // Older revisions still expect them to work.
            let out = dispatch(&client, method, &json!({}), "2025-11-25")
                .unwrap_or_else(|| panic!("{method} should still work on 2025-11-25"));
            assert_eq!(out, json!({}));
        }
    }
}
