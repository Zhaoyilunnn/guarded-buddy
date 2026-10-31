# ChatGPT web conversation sync

buddy can receive ChatGPT web conversations from a Chromium extension, archive
those snapshots across devices, and include their visible text in weekly reports.
The browser uses its existing ChatGPT login. No OpenAI API key is required.

## Setup

1. Run `buddy web serve` on the same machine as the browser. Keep this foreground
   process running; Ctrl-C stops it. The default address is `http://127.0.0.1:18242`.
2. Run `buddy web pair` in another terminal and copy its pairing token.
3. Open `chrome://extensions` (or the equivalent in Edge/Brave), enable Developer
   mode, select **Load unpacked**, and choose this repository's `extension/chatgpt`
   directory. The extension is not distributed through a browser store.
4. Open ChatGPT in that browser and sign in. Open the extension popup, paste the
   token, and select **Save and connect**.
5. Select **Save current conversation** on a saved ChatGPT conversation, or
   **Sync history** to read the paginated conversation list. Optionally enable
   **Sync every 15 minutes**.

```sh
buddy web serve
buddy web pair
buddy web status
buddy wr collect --agents chatgpt --days 7
buddy sync --agents chatgpt
```

All three `web` subcommands accept `--home`. `serve` and `status` accept `--port`;
when changing it, set the same port in the extension popup. `pair` prints a local
receiver credential: keep it private. It is separate from your ChatGPT login.
`status` prints the local conversation count and checks receiver health, failing
when the receiver is unavailable. No service installation or automatic process
startup is performed.

## Storage and report behavior

Snapshots are stored as versioned JSON envelopes containing the provider's raw
conversation response and the local capture time:

```text
~/.local/share/buddy/web/
|-- token
`-- chatgpt/
    `-- <SHA-256 account namespace>/
        `-- <conversation-id>.json
```

The schema version is currently `1`. Filenames use conversation IDs, so renaming
a conversation updates the same file. The account namespace is derived from the
logged-in user's ID; different accounts stay separate. No browser cookies or
provider access tokens are sent to buddy. The raw conversation response can
include nontext metadata and asset references. Images and attachments are not
downloaded, and no OCR is performed.

`buddy sync` copies only the ChatGPT JSON snapshots into the existing device HOME
archive. It excludes the pairing token. Configure `[sync]` as described in
[buddy-sync.md](buddy-sync.md). The existing cloud-folder client replicates the
archive; the receiver does not write directly to another device.

`wr collect` and `wr run` select one complete snapshot per account and
conversation across the live HOME and all configured archive homes. The provider
update timestamp wins; capture time breaks ties. Selection happens before the
report date filter, so old messages from an obsolete snapshot are not resurrected.
The reader follows `current_node` back through its parents and includes visible
user/assistant text from that branch only. Other branches remain in the raw JSON.
System messages, tool messages, hidden messages, and internal reasoning channels
are excluded. Dates use each message's original creation timestamp, converted to
the local timezone. Missing timestamps or broken branch references produce a
source warning instead of invented report dates.

Daily Markdown shows the ChatGPT title and original conversation URL. The source
slug is `chatgpt`, distinct from the existing `codex` coding source. Existing
message truncation and secret redaction apply. Web conversations are excluded
from `signoff` ingestion and automatic actions.

## Incremental sync and recovery

- **Sync history** compares list update timestamps with locally acknowledged
  revisions, fetching details only for changed or unrecognized conversations.
- **Recheck all history** fetches every listed conversation regardless of the
  checkpoint. Use it after restoring/deleting the local archive or if a provider
  update timestamp failed to reflect a change. Saving the current conversation
  also bypasses the checkpoint.
- A snapshot is queued in extension-local storage before delivery. The receiver
  writes through a temporary file and acknowledges only after replacing the live
  JSON file. Unchanged content is skipped and older provider revisions are ignored.
- Pagination position and per-conversation acknowledgements survive extension
  worker suspension. A one-minute alarm checks for incomplete work even when
  periodic sync is disabled. Interrupted items may be replayed safely.
- Provider 429/5xx responses use bounded retries with backoff. Failed syncs retain
  their progress and retry after 15 minutes; the popup displays the failure.
  A failed conversation stops that run at its checkpoint. Login failures require
  signing in again. A queued snapshot for another account requires returning to
  that account before recovery can finish.
- The browser must be running for alarms to run. Closing the popup does not
  cancel synchronization. The receiver must be reachable for delivery.
- Deleting a provider conversation or a local source file does not delete an
  archived copy. This is a one-way archive, not a browser history restore tool.

The receiver accepts at most 20 MiB per request and listens only on IPv4 loopback.
It validates the Host, bearer token, and any supplied Origin, and exposes no CORS
access to ordinary websites. Storage paths and filenames are constrained, symlink
storage destinations are rejected, and newly created files/directories are private
on Unix. Run one receiver per HOME. The extension requests `unlimitedStorage` for
large pending snapshots, plus storage, alarms, active-tab access, and access to
ChatGPT and the loopback receiver. Conversation data is not encrypted by buddy.

## Protocol and limitations

The extension calls ChatGPT's private session, conversation-list, and conversation
detail endpoints using the user's browser session. This approach is inspired by
[Kept's browser adapters](https://github.com/egroup-labs/kept/tree/main/extension/platforms).
It reads API responses rather than rendered page text. These private endpoints
can change or reject extension requests; login challenges may require opening
ChatGPT in the browser. Only conversations exposed by the list endpoint or an
explicit current-conversation save are captured. Temporary chats and inaccessible
or separately archived conversations are not guaranteed to appear.

The local protocol has two authenticated endpoints:

| Endpoint | Behavior |
| --- | --- |
| `GET /health` | Returns `app: "buddy"` and `schema_version: 1` |
| `POST /v1/chatgpt` | Accepts `schema_version`, `account`, `captured_at`, and `conversation`; returns `status` (`created`, `updated`, `skipped`, or `stale`) |

```mermaid
sequenceDiagram
    actor User
    participant Extension as Browser extension
    participant ChatGPT as ChatGPT private API
    participant Receiver as buddy web serve
    participant Local as Local ChatGPT snapshots
    participant Archive as Device archive
    participant Report as Weekly report collector

    User->>Extension: Pair with buddy web pair token
    User->>Extension: Save current / Sync history / Enable periodic sync
    Extension->>Receiver: Authenticated health check
    Extension->>ChatGPT: Use browser login to list and fetch conversations
    Extension->>Extension: Persist pending snapshot and pagination progress
    Extension->>Receiver: POST /v1/chatgpt with receiver token
    Receiver->>Local: Validate and atomically save snapshot
    Receiver-->>Extension: Acknowledge saved / skipped / stale
    Extension->>Extension: Persist checkpoint and clear pending snapshot
    User->>Archive: buddy sync copies snapshots to device HOME archive
    Note over Archive: External sync product replicates the directory
    User->>Report: buddy wr collect / run
    Report->>Local: Read live snapshots
    Report->>Archive: Read device snapshots
    Report->>Report: Select newest snapshot, then current branch and report dates
    Report-->>User: Daily Markdown and weekly-report input
```

## Verification

```sh
cargo test
cargo clippy
node extension/chatgpt/tests/api.test.js
node extension/chatgpt/tests/background.test.js
```

Rust integration tests exercise a real loopback receiver, authentication, atomic
updates, archival, branch filtering, latest-snapshot selection, CLI collection,
and signoff exclusion. Extension tests mock provider/receiver responses and
browser storage, including offline delivery and worker restart. They do not
replace a live test in an authenticated browser account.
