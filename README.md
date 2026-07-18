# ai-weekly-report

一键生成每周 AI 编程助手对话周报。读取本机多个 AI 编程助手的对话历史
（Codex CLI、Cursor、Claude Code、Gemini/Antigravity agy），按天整理成统一
格式的 Markdown，再调用外部 CLI 或 LLM API 总结成周报。

## 功能

- **采集（collect）**：扫描 `$HOME` 下 `.codex`、`.cursor`、`.claude`、
  `.gemini` 四个数据目录，按本地日期归组，在 `out/<开始>_<结束>/` 下
  每天生成一个 Markdown（`2026-07-15.md`），外加 `index.md` 索引；
  幂等（重复运行覆盖同名文件）
- **周报（report）**：对已采集目录生成周报，两种后端：
  - **CLI**：调用本机已安装的 `codex` / `claude` / `agy` / `gemini`（非交互模式，
    prompt 走 stdin 或参数，工作目录设为记录目录让它自己读文件）
  - **API**：内置 OpenAI 兼容 client（`POST {base_url}/chat/completions`），
    内嵌全文（200KB 预算），key 从环境变量读
- **run** = collect + report 一键完成
- 自定义周报模板（`--template`），占位符 `{{start_date}} {{end_date}} {{stats}} {{daily_notes}}`

## 安装

```sh
cargo install --path .
# 或直接用 cargo run -- <args>
```

## 使用

```sh
# 一键：采集最近 7 天 + 用 claude CLI 生成周报（写入 out/<range>/report.md）
ai-weekly-report run

# 只采集，指定日期范围
ai-weekly-report collect --from 2026-07-12 --to 2026-07-18

# 只总结（不重复采集），打印到终端
ai-weekly-report report --days 7 --stdout

# 换后端：codex CLI
ai-weekly-report run --cli-name codex

# 自定义 CLI（按空白切分，prompt 走 stdin）
ai-weekly-report run --cmd "my-wrapper --fast"

# API 后端（DeepSeek 等 OpenAI 兼容服务）
export DEEPSEEK_API_KEY=sk-...
ai-weekly-report run --backend api \
  --base-url https://api.deepseek.com/v1 \
  --api-key-env DEEPSEEK_API_KEY \
  --model deepseek-chat

# 自定义模板 / 只看部分 agent / 纳入 agy prompt 历史
ai-weekly-report run --template my-template.md --agents codex,claude
ai-weekly-report collect --include-prompt-history

# 查看本机检测到的数据源
ai-weekly-report sources
```

## 配置文件

`~/.config/ai-weekly-report/config.toml`（可选），优先级：**CLI flag > config > 内置默认**。

```toml
out_dir = "out"
days = 7
backend = "cli"                # cli | api
cli_name = "claude"            # codex | claude | agy | gemini
timeout_secs = 600
api_base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
api_model = "gpt-4o-mini"
# template = "/path/to/template.md"
# include_prompt_history = false
```

## 数据源格式

| 来源 | 路径 | 说明 |
|---|---|---|
| Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `event_msg` 的 user/agent 消息；resume 产生的重复消息自动去重 |
| Cursor | `~/.cursor/projects/<slug>/agent-transcripts/*/*.jsonl` | 内嵌 `<timestamp>` 解析，fallback 到文件 mtime |
| Claude Code | `~/.claude/projects/<slug>/*.jsonl` | user/assistant 行；跳过 system/thinking/tool_result |
| Gemini | `~/.gemini/tmp/*/chats/session-*.json` + `~/.gemini/antigravity-cli/brain/*/…/transcript.jsonl` | agy 的 `history.jsonl` 默认排除（`--include-prompt-history` 开启） |

容错：目录不存在 = 该源无数据；坏行计入警告（stderr + index.md 脚注），不中断采集。

## 架构

单 crate、library-first、SOLID：

```
src/
  domain.rs      // AgentKind / Role / Message / Session / DateRange
  sources/       // trait HistorySource（OCP：新 agent = 新模块 + 一行注册）
    codex.rs cursor.rs claude.rs gemini.rs
  collect.rs     // merge_sessions（去重）→ group_by_day（跨午夜拆分）→ 落盘
  render/daily.rs// 单日 Markdown + index.md
  template.rs    // 内置中文模板 + 占位符替换
  report/        // trait Summarizer
    cli_backend.rs  // 预设表 + 自定义 --cmd + wait-timeout
    api_backend.rs  // OpenAI 兼容，HttpClient trait 做测试接缝
  cli.rs / config.rs // clap + TOML，flag > config > 默认
  app.rs / main.rs   // 薄编排 + 薄入口
```

## 开发

```sh
cargo test            # 133 单测 + 7 集成测试
cargo clippy          # 零警告
```

全程 TDD 开发：每个模块先写测试（red）再实现（green）。集成测试用 fake
`$HOME` + `TZ=Asia/Shanghai` 子进程验证 UTC→+08 的跨日边界。
