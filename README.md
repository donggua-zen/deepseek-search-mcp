# deepseek-mcp

> **English** — A tiny local MCP server that exposes DeepSeek's [Search / Fetch API](https://api-docs.deepseek.com/guides/search)
> to any MCP client. DeepSeek's search is currently baked into its own tooling, so third-party
> agents cannot reach it; this is the missing bridge. Single ~1.8 MB binary, ~12 ms cold start,
> no Node/Python/.NET runtime, no async runtime, no MCP framework. Speaks both the legacy
> `initialize` handshake and the stateless 2026-07-28 protocol. See [Usage](#接入客户端) below.

把 DeepSeek 官方的 **Search / Fetch API** 包成一个本机小 MCP server，让任何支持 MCP 的
客户端（Claude Code、Cursor、Cherry Studio、Codex、Cline、各种自研 agent…）都能直接用上
DeepSeek 的联网搜索和网页抓取。

> DeepSeek 的 Search/Fetch 目前只内建在它自家工具里，第三方 agent 用不上。这个项目就是
> 那层缺失的桥。

- 单个 exe，**约 1.8 MB**，无运行时依赖（不需要 Node / Python / .NET）
- 冷启动 **约 12 ms**（实测 initialize 往返 12–20 ms）
- 只用 `ureq + serde_json`，没有 async runtime，没有 MCP 框架
- 兼容新旧协议：旧的 `initialize` 握手和 2026-07-28 的无状态 `server/discover` 都支持

## 协议兼容性

这个 server 同时伺候两代协议，并且**按版本决定输出形状**，不是简单地都加字段：

| | 2024-11-05 … 2025-11-25（旧） | 2026-07-28（新，无状态） |
|---|---|---|
| 握手 | `initialize` + `notifications/initialized` | 无，每个请求在 `_meta` 里带版本 |
| 版本传递 | `initialize` 协商一次 | `_meta["io.modelcontextprotocol/protocolVersion"]` |
| `resultType` | 不加 | **必加** `"complete"` |
| 列表结果 | 只有数组字段 | 另需 `ttlMs` + `cacheScope`（`CacheableResult`） |
| `serverInfo` | `initialize` 里返回 | 每个结果的 `_meta` 里带 |
| `ping` / `logging/setLevel` | 正常响应 | 已移除，返回 `-32601` |
| 未知版本 | — | `-32022` + 支持的版本列表 |

**为什么不能统一都加 `resultType`**：旧版客户端校验 `ping` 结果时用的是严格空对象 schema，
多一个字段就是解析失败。所以字段必须按协商版本开关。

`tests/e2e/conformance.mjs` 直接按线格式校验这 29 项，CI 里用 dummy key 跑，不花钱。

## 为什么是 Rust

| 方案 | 冷启动 | 分发 | 结论 |
|---|---|---|---|
| **Rust 单二进制** | ~12 ms | 一个 exe | ✅ 本机常驻 MCP 最合适 |
| Node + 官方 SDK | ~80–150 ms | 需 node + node_modules | 开发快，但要维护环境 |
| Python + mcp | ~300 ms+ | 需 venv | 依赖最重 |
| Rust + `rmcp` 框架 | 同左 | 一个 exe | 能力全，但对 2 个工具是过度设计 |

MCP 的 stdio 传输本质就是「一行一个 JSON」，这个 server 只需要实现 4 个方法，手写比引框架
更可控，还能顺手同时兼容两版协议。

## 快速开始

### 0. 直接下载预编译二进制

从 [Releases](https://github.com/donggua-zen/deepseek-search-mcp/releases/latest) 下载对应平台的文件，
无需安装任何运行时：

| 平台 | 文件 |
|---|---|
| macOS Apple Silicon (M1/M2/M3/M4) | `deepseek-search-mcp-macos-arm64` |
| macOS Intel | `deepseek-search-mcp-macos-x64` |
| Windows x64 | `deepseek-search-mcp-windows-x64.exe` |
| Linux x64 | `deepseek-search-mcp-linux-x64` |

**macOS 首次运行**（二进制是 ad-hoc 签名、未做公证，会被 Gatekeeper 拦）：

```bash
chmod +x deepseek-search-mcp-macos-arm64
xattr -d com.apple.quarantine deepseek-search-mcp-macos-arm64
./deepseek-search-mcp-macos-arm64 --version
```

最低支持 macOS 11.0（Apple Silicon 的底线），Intel 版同样覆盖 11.0+。

### 1. 编译

```bash
# Windows：需要 MSVC，脚本会自动加载 vcvars64
./build.bat release

# macOS / Linux，或已配好工具链的环境
cargo build --release
```

产物：`target/release/deepseek-mcp`（Windows 上是 `.exe`）

> Windows 上如果 `cargo build` 报 `link.exe` 相关错误，通常是 Git 自带的 `link`
> 抢占了 PATH，改用 `build.bat` 即可（它先 call vcvars64.bat 再调 cargo）。

### 2. 配置 API Key

三种方式，优先级从高到低：

```bash
export DEEPSEEK_API_KEY=sk-xxx          # 环境变量
deepseek-mcp --key sk-xxx               # 命令行
echo '{"api_key":"sk-xxx"}' > ~/.deepseek-mcp.json   # 配置文件
```

### 3. 接入客户端

**Claude Code**

```bash
claude mcp add deepseek-search --scope user -- /绝对路径/deepseek-mcp.exe
```

**Cursor / Cherry Studio / Cline 等**（JSON 配置）

```json
{
  "mcpServers": {
    "deepseek-search": {
      "command": "D:/tools/deepseek-mcp.exe",
      "env": { "DEEPSEEK_API_KEY": "sk-xxx" }
    }
  }
}
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.deepseek-search]
command = "D:/tools/deepseek-mcp.exe"
env = { DEEPSEEK_API_KEY = "sk-xxx" }
```

## 提供的工具

### `web_search`

| 参数 | 类型 | 说明 |
|---|---|---|
| `query` | string | **必填**，1–128 字符，可直接内嵌搜索语法 |
| `count` | int | 1–50，默认 10 |
| `need_content` | bool | 是否附带网页正文，默认 false |
| `ori_question` | string | 用户原始问题，帮助理解搜索意图 |
| `site` / `exclude_site` | string \| array | 限定 / 排除域名，多个之间 OR |
| `lang` | string | `zh` 或 `zh,en` |
| `after` / `before` | string | `2025` 或 `2025-01-01` |

`site/lang/after/before` 会被拼进 `query`；如果 `query` 里已经手写了同名操作符，就尊重手写值。

支持的搜索操作符（原样透传给 DeepSeek）：

```
site:github.com      -site:example.com     -广告
-"release notes"     "精确短语"            lang:zh,en
after:2025-01-01     before:2026-06-01
```

输出是给模型读的 Markdown：编号标题 + URL + （站点 · 日期 · 相关性 · 权威度）+ 长摘要，
可选 `<content>` 正文块，正文总量有字符预算，避免单页刷爆上下文。

### `web_fetch`

`url` → 干净的 Markdown。DeepSeek 返回的正文会带大量 `\/`、`\#`、`\|` 转义，这里已经
统一反转义，并把页面自带的 `#` 标题降级，避免和结果编号标题混在一起。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `DEEPSEEK_API_KEY` | — | API Key |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com` | 可指向代理网关 |
| `DEEPSEEK_MCP_TIMEOUT` | `60` | 单次请求超时秒数 |
| `DEEPSEEK_MCP_MAX_CHARS` | `30000` | 交给模型的正文上限（字符） |
| `DEEPSEEK_MCP_COUNT` | `10` | 默认返回条数 |
| `DEEPSEEK_MCP_DEBUG` | — | 设为 1 时把协议与请求体打到 stderr |
| `HTTP_PROXY` / `HTTPS_PROXY` | — | 走代理 |

## 命令行自测（不接客户端也能用）

```bash
deepseek-mcp search "Rust MCP server" --count 3
deepseek-mcp search "mcp" --content        # 带正文
deepseek-mcp fetch https://example.com
deepseek-mcp                               # 无子命令 = stdio MCP server
```

## 错误处理与计费注意

- 工具级错误（URL 非法、查不到内容、额度不足）返回 `isError: true` 的**正常结果**，
  模型能看见并自我纠正；只有方法不存在等情况才走 JSON-RPC 错误。
- 408 / 429 / 5xx 自动退避重试 3 次；4xx 不重试。
- DeepSeek 按**成功请求**计费（Search 1k 次 ¥5，Fetch 1k 次 ¥5），`count` 不影响单价，
  但调大 `count` 会白占上下文。
- 账号级限速：Search 100 QPS、Fetch 25 QPS。

## 开发

```bash
cargo test --release      # 12 个单元测试
cargo clippy              # 零警告

# 协议合规检查（不需 API key，29 项断言，不花钱）
node tests/e2e/conformance.mjs target/release/deepseek-mcp

# 官方 SDK 端到端（需要真实 API Key，约 2 次计费请求）
cd tests/e2e && npm install && DEEPSEEK_API_KEY=sk-xxx npm test
```

> 官方 `@modelcontextprotocol/sdk` 目前最多只协商到 `2025-11-25`，**测不到 2026-07-28
> 的无状态路径**。这就是 `conformance.mjs` 存在的原因：它直接按线格式发报文。

> Windows 下用 `./build.bat test` 代替 `cargo test`（见上文 link.exe 说明）。
>
> 已知环境问题：部分安全软件会拦截 `cargo test` 启动测试进程（报 os error 5）。
> 用 `cargo test --no-run` 编译后手动运行 `target/debug/deps/deepseek_mcp-*.exe` 即可。

代码结构（`src/main.rs` 单文件）：

```
Config/Client     配置加载与 ureq 调用（含重试）
render_*          DeepSeek JSON → 给模型看的 Markdown
tools()           两个工具的 inputSchema
dispatch()        MCP 方法路由
serve()           stdio 逐行 JSON-RPC 循环
run_cli()         search / fetch 子命令
```

## License

MIT
