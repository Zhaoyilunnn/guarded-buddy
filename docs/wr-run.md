# Weekly report execution

This sequence describes the current `buddy wr run` implementation. Collection runs
in the foreground; summarization and optional email run in a separate worker.
The output root is configurable; `out/<range>` below denotes its default layout.

```mermaid
sequenceDiagram
    actor User
    participant CLI as buddy wr run
    participant Sources as Local and archived histories
    participant Files as Output directory
    participant Worker as buddy wr report --worker
    participant Agent as External CLI or API
    participant Mail as mutt

    User->>CLI: Run with options
    CLI->>CLI: Load config and resolve date range and backend
    opt Sync configured and --no-sync absent
        CLI->>Sources: Copy changed local histories to device archive
        Sources-->>CLI: Sync result
        Note over CLI,Sources: Reported sync failure stops the foreground command
    end
    CLI->>Sources: Read current HOME and configured devices/*/home
    Note over CLI,Sources: Codex scans all date directories and skips files with mtime before the first local day
    Sources-->>CLI: Parsed sessions and source warnings
    CLI->>CLI: Merge sessions and deduplicate messages
    CLI->>CLI: Filter message dates and group by local day
    CLI->>Files: Write daily Markdown and index.md
    CLI-->>User: Collection summary and warnings
    CLI->>Files: Validate collected input
    CLI->>CLI: Check API key if API backend selected
    CLI->>Files: Open report.log in append mode
    CLI->>Worker: Spawn with resolved options and redirected output
    CLI->>Files: Write report.pid
    CLI-->>User: Print worker PID, report path and report.log path, then exit
    Note over Worker,Files: Worker continues independently of the foreground process
    Worker->>Files: Append summarization start to report.log
    Worker->>Files: Read Markdown input and compute statistics
    Worker->>Worker: Render configured or default prompt template
    alt CLI backend
        Worker->>Files: Create unique agent-timestamp-pid.log
        Worker->>Files: Print absolute agent log path into report.log
        Worker->>Agent: Spawn in range directory and supply file-manifest prompt
        Note over Agent,Files: Prompt asks the agent to read listed Markdown files
        loop As stdout or stderr arrives
            Agent-->>Worker: Output chunk
            Worker->>Files: Flush timestamped chunk to agent log
        end
        alt Successful exit
            Agent-->>Worker: Exit code zero, stdout is the report
        else Nonzero exit or timeout
            Worker->>Files: Record exit or timeout in agent log
            Worker->>Files: Report error in report.log and stop worker
            Note over Worker,Agent: Timeout requests termination, captured logs remain
        end
    else API backend
        Worker->>Agent: Send inline prompt with character budget
        Agent-->>Worker: Report content or API error
        Note over Worker,Files: No CLI agent log, API errors stop the worker
    end
    opt Summarization succeeded
        Worker->>Files: Write report.md and log completion
        opt Recipients configured
            Worker->>Mail: Check availability
            alt mutt available
                Worker->>Mail: Send report body
                Mail-->>Worker: Success or failure
                Worker->>Files: Log email result and retain report.md on failure
            else mutt unavailable
                Worker->>Files: Log installation hint and retain report.md
            end
        end
    end
```

## Reading the flow

- The default interval is seven local calendar days including today. Explicit
  `--from` and `--to` override it; otherwise `--days` overrides `[wr].days`.
- Sync handles raw file changes independently of the report date range. An
  external sync product replicates the archive; buddy does not wait for cloud
  uploads or downloads to complete.
- Session identity is `(agent, session ID, project)`. Message deduplication uses
  millisecond timestamp, role, and content. Filtering happens per message;
  sessions spanning multiple days appear in multiple daily files.
- `wr collect` performs collection without automatic sync. `wr report` starts
  summarization from existing output. The internal `--worker` path skips sync
  and collection.
- The foreground command succeeding means the worker was launched, not that
  generation or email succeeded. Inspect `report.log` and `report.md` for the
  outcome. The agent log records emitted CLI output, not unreported internal
  activity. Logs can contain raw conversation content and credentials.

## Current limitations

- Codex scans all session directories in both live and archived homes. Only
  JSONL files with mtime before the first requested local day are skipped,
  with no upper mtime cutoff. Message timestamps still determine inclusion.
  This optimization assumes trustworthy file modification times. Restored or
  externally modified timestamps can cause omissions. Unavailable mtime emits
  a warning and the file is still read.
- Markdown input loading excludes `index.md` but currently includes other
  Markdown files, including an existing `report.md` on repeated runs.
- Archive enumeration can silently return no device homes on directory access
  failure. A successful report does not establish archive completeness.
- Multiple workers for the same range are not prevented. They share report.log
  and report.md, although CLI agent logs are separate per invocation.

## Implementation references

- [Dispatch and worker lifecycle](../src/main.rs)
- [Detached worker launch](../src/job.rs)
- [Source aggregation and summarization](../src/app.rs)
- [Message merge and date filtering](../src/collect.rs)
- [External CLI execution and logging](../src/report/cli_backend.rs)
