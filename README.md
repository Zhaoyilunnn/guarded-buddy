# guarded-buddy

A **guarded buddy** — not a free-roaming agent, but a CLI that runs specific skills under clear constraints, borrowing local AI CLIs / OpenAI-compatible APIs when needed.

Binary: **`buddy`**

## Skills

```text
buddy
├── wr | weekly-report     # weekly AI chat report
│   ├── collect | report | run
│   ├── mail <report.md>
│   └── sources
├── signoff                # end-of-day silent progress
│   ├── run                # default: ingest → plan → act → email
│   ├── plan               # plan only (no act / no email)
│   └── mail <signoff.md>
└── completions <bash|zsh|fish>
```

### `wr` — weekly report

Collects conversation history from Codex / Cursor / Claude Code / Gemini under `$HOME`, writes per-day Markdown, then summarizes in the background to `report.md` (optional mutt email).

```sh
buddy wr run
buddy wr collect --from 2026-07-12 --to 2026-07-18
buddy wr report --days 7
buddy wr mail out/2026-07-12_2026-07-18/report.md
buddy wr sources
```

### `signoff` — end of day

1. **Ingest** last N hours (default 24) of AI coding chats  
2. **Plan** structured todos; model + local gate classify `auto` vs `needs_human`  
3. **Act** only on high-confidence todos whose workspace is in `allowed_workspaces`  
4. **Email** summary to `[signoff].mail_to`

```sh
buddy signoff              # background worker
buddy signoff plan         # plan only → plan.json
buddy signoff --dry-run    # plan + signoff.md, no agent actions
buddy signoff mail out/signoff/2026-08-04/signoff.md
```

## Install

```sh
cargo install --path .
# binary: buddy
eval "$(buddy completions bash)"   # optional
```

## Configuration

`~/.config/buddy/config.toml` (falls back to legacy `~/.config/ai-weekly-report/config.toml`).

```toml
out_dir = "out"

[llm]
backend = "cli"                # cli | api
cli_name = "codex"             # codex | claude | agy | gemini
timeout_secs = 600

[wr]
days = 7
mail_to = ["weekly@example.com"]

[signoff]
window_hours = 24
mail_to = ["me@personal.com"]
allowed_workspaces = ["/home/you/proj-a", "/home/you/proj-b"]
max_auto_todos = 3
act_timeout_secs = 1800
dry_run = false
```

Priority: **CLI flag > config > defaults**. `wr` and `signoff` use **separate** `mail_to` lists.

## Security

- API keys only via env var names (`api_key_env`).  
- Secret redaction in collected Markdown / prompts / errors.  
- Signoff **Act** is allowlist-gated; ambiguous todos stay in `needs_human`.

## Migration from ai-weekly-report

| Old | New |
|---|---|
| `ai-weekly-report run` | `buddy wr run` |
| `ai-weekly-report collect` | `buddy wr collect` |
| `ai-weekly-report report` | `buddy wr report` |
| `ai-weekly-report mail …` | `buddy wr mail …` |
| `ai-weekly-report sources` | `buddy wr sources` |
| `~/.config/ai-weekly-report/` | `~/.config/buddy/` (legacy still read) |

## Development

```sh
cargo test
cargo clippy
```
