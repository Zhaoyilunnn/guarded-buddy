# ai-weekly-report

Generate weekly AI coding-assistant conversation reports in one command. Reads local conversation history from multiple AI coding assistants (Codex CLI, Cursor, Claude Code, Gemini/Antigravity agy), organizes it by day into a unified Markdown format, then calls an external CLI or LLM API to summarize into a weekly report.

## Features

- **collect**: Scans `$HOME` for data directories `.codex`, `.cursor`, `.claude`, and `.gemini`, groups by local date, writes one Markdown file per day under `out/<start>_<end>/` (e.g. `2026-07-15.md`) plus an `index.md` index; idempotent (re-runs overwrite same filenames)
- **report**: Generates a weekly report from a collected directory, with two backends:
  - **CLI**: Invokes locally installed `codex` / `claude` / `agy` / `gemini` (non-interactive; prompt via stdin or args; working directory set to the records directory so the tool reads files itself)
  - **API**: Built-in OpenAI-compatible client (`POST {base_url}/chat/completions`), embeds full text (200KB budget), reads API key from environment variables
- **run** = collect + report in one step
- Custom report template (`--template`) with placeholders `{{start_date}}` `{{end_date}}` `{{stats}}` `{{daily_notes}}`

## Install

```sh
cargo install --path .
# or run directly with cargo run -- <args>
```

## Usage

```sh
# One-shot: collect last 7 days + generate report with claude CLI (writes out/<range>/report.md)
ai-weekly-report run

# Collect only, with explicit date range
ai-weekly-report collect --from 2026-07-12 --to 2026-07-18

# Summarize only (no re-collect), print to terminal
ai-weekly-report report --days 7 --stdout

# Switch backend: codex CLI
ai-weekly-report run --cli-name codex

# Custom CLI (split on whitespace, prompt via stdin)
ai-weekly-report run --cmd "my-wrapper --fast"

# API backend (DeepSeek and other OpenAI-compatible services)
export DEEPSEEK_API_KEY=sk-...
ai-weekly-report run --backend api \
  --base-url https://api.deepseek.com/v1 \
  --api-key-env DEEPSEEK_API_KEY \
  --model deepseek-chat

# Custom template / filter agents / include agy prompt history
ai-weekly-report run --template my-template.md --agents codex,claude
ai-weekly-report collect --include-prompt-history

# List detected data sources on this machine
ai-weekly-report sources
```

## Configuration

`~/.config/ai-weekly-report/config.toml` (optional). Priority: **CLI flag > config > built-in defaults**.

```toml
out_dir = "out"
days = 7
backend = "cli"                # cli | api
cli_name = "claude"            # codex | claude | agy | gemini
timeout_secs = 600
api_base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"   # env var *name* only — never put the raw key here
api_model = "gpt-4o-mini"
# template = "/path/to/template.md"
# include_prompt_history = false
```

## Security

- API keys are read only from environment variables (`--api-key-env` / `api_key_env`). The config file stores the variable **name**, never the secret itself.
- Collected Markdown, API prompts, and error messages run through secret redaction (OpenAI/Anthropic-style `sk-…` keys, GitHub tokens, Google `AIza…` keys, AWS `AKIA…` ids, Bearer tokens, PEM private keys, and common `api_key=` / `secret=` assignments).
- Do not commit `.env` files or paste live keys into chat transcripts; re-collect after rotating any key that may have appeared in history.

## Data source formats

| Source | Path | Notes |
|---|---|---|
| Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `event_msg` user/agent messages; duplicate messages from resume are deduplicated |
| Cursor | `~/.cursor/projects/<slug>/agent-transcripts/*/*.jsonl` | Embedded `<timestamp>` parsing, fallback to file mtime |
| Claude Code | `~/.claude/projects/<slug>/*.jsonl` | user/assistant lines; skips system/thinking/tool_result |
| Gemini | `~/.gemini/tmp/*/chats/session-*.json` + `~/.gemini/antigravity-cli/brain/*/…/transcript.jsonl` | agy `history.jsonl` excluded by default (enable with `--include-prompt-history`) |

Fault tolerance: missing directory = no data from that source; bad lines count as warnings (stderr + index.md footnote), collection continues.

## Architecture

Single crate, library-first, SOLID:

```
src/
  domain.rs      // AgentKind / Role / Message / Session / DateRange
  sources/       // trait HistorySource (OCP: new agent = new module + one registry line)
    codex.rs cursor.rs claude.rs gemini.rs
  collect.rs     // merge_sessions (dedup) → group_by_day (midnight split) → write files
  render/daily.rs// daily Markdown + index.md
  template.rs    // built-in Chinese template + placeholder substitution
  secrets.rs     // API key / token redaction
  report/        // trait Summarizer
    cli_backend.rs  // preset table + custom --cmd + wait-timeout
    api_backend.rs  // OpenAI-compatible, HttpClient trait as test seam
  cli.rs / config.rs // clap + TOML, flag > config > default
  app.rs / main.rs   // thin orchestration + thin entry point
```

## Development

```sh
cargo test            # unit + integration tests
cargo clippy          # zero warnings
```

Developed with TDD throughout: each module gets tests first (red) then implementation (green). Integration tests use a fake `$HOME` + `TZ=Asia/Shanghai` subprocess to verify UTC→+08 cross-day boundaries.
