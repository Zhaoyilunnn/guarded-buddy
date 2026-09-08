# guarded-buddy

A **guarded buddy** - not a free-roaming agent, but a CLI that runs specific skills under clear constraints, borrowing local AI CLIs / OpenAI-compatible APIs when needed.

Binary: **`buddy`**

## Skills

```text
buddy
|-- sync                   # archive local chat history to a shared directory
|-- wr | weekly-report     # weekly AI chat report
|   |-- collect | report | run
|   |-- mail <report.md>
|   `-- sources
|-- signoff                # end-of-day silent progress
|   |-- run                # default: ingest -> plan -> act -> email
|   |-- plan               # plan only (no act / no email)
|   `-- mail <signoff.md>
`-- completions <bash|zsh|fish>
```

### `wr` - weekly report

Collects conversation history from Codex / Cursor / Claude Code / Gemini under `$HOME`, writes per-day Markdown, then summarizes in the background to `report.md` (optional mutt email).

```sh
buddy wr run
buddy wr collect --from 2026-07-12 --to 2026-07-18
buddy wr report --days 7
buddy wr mail out/2026-07-12_2026-07-18/report.md
buddy wr sources
```

When `[sync]` is configured, `buddy wr run` first archives this device and then
collects from every archived device plus the live local home. Use `--no-sync`
only when the shared directory is temporarily unavailable.

### `sync` - cross-device conversation archive

`buddy sync` incrementally copies only supported raw conversation-history files.
Each device writes to its own namespace; source deletions are never propagated.
The destination is any local directory selected by the user. Cross-device
replication is delegated to an external sync product; buddy has no provider-specific integration.

```sh
buddy sync
buddy sync --agents codex,claude
```

Raw conversations can contain source code, filesystem paths, prompts, and
secrets. Choose a trusted sync directory; buddy does not encrypt the archive.

Detailed behavior and sequence diagram: [docs/buddy-sync.md](docs/buddy-sync.md).

### `signoff` - end of day

1. **Ingest** last N hours (default 24) of AI coding chats  
2. **Plan** structured todos; model + local gate classify `auto` vs `needs_human`  
3. **Act** on high-confidence todos; Codex sandbox flags come from per-workspace `trust` (default `yolo`)  
4. **Email** summary to `[signoff].mail_to`

```sh
buddy signoff              # background worker
buddy signoff plan         # plan only -> plan.json
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

`~/.config/buddy/config.toml`

```toml
out_dir = "out"

[llm]
backend = "cli"                # cli | api
cli_name = "codex"             # codex | claude | agy | gemini
timeout_secs = 600

[wr]
days = 7
mail_to = ["weekly@example.com"]

[sync]
path = "/path/to/cloud-synced-folder/buddy-history"
device = "work-laptop"          # unique and stable on every device

[signoff]
window_hours = 24
mail_to = ["me@example.com"]
max_auto_todos = 3
act_timeout_secs = 7200
dry_run = false

# Optional trust overrides for act (unlisted paths default to yolo):
# yolo | workspace-write | read-only
[[signoff.workspaces]]
path = "/home/you/proj-a"
trust = "workspace-write"

[[signoff.workspaces]]
path = "/home/you/proj-b"
trust = "yolo"
```

| `trust` | Codex act flags |
|---------|-----------------|
| `yolo` (default) | `--dangerously-bypass-approvals-and-sandbox` |
| `workspace-write` | `-s workspace-write` |
| `read-only` | `-s read-only` |

Priority: **CLI flag > config > defaults**. `wr` and `signoff` use **separate** `mail_to` lists.

## Development

```sh
cargo test
cargo clippy
```
