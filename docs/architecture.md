# Agent Monitor Architecture

```text
Codex App / CLI / VS Code
        |
        | codex app-server (stdio JSON-RPC)
        v
CodexClient -> Reconciler -> SnapshotStore (SQLite)
        ^             |
        |             +--> normalized Tauri events + notifications
        |
~/.codex/sessions/**/*.jsonl (notify tailer)
```

`CodexClient` owns one long-lived child process and performs the
`initialize`/`initialized` handshake. `thread/list` is the cheap index pass;
changed threads use `thread/read(includeTurns=false)` for metadata and
`thread/turns/list(itemsView=notLoaded, sortDirection=desc, limit=1)` for the
latest turn's lightweight lifecycle fields. The monitor does not routinely
load full turns, items, prompts, assistant replies, or command output.
Notifications from the app-server are buffered while a request is in flight and
trigger a targeted reconciliation for lifecycle events.

Archived threads never trigger turn reads. They are restored from
`thread/list` metadata and may reuse an existing normalized snapshot. On
restart, SQLite snapshots immediately populate both the dashboard and its
lightweight detail cache; opening a detail lazily hydrates current turn metadata
when the shared app-server client is idle. If that metadata omits a plan, plan
counts are retained only when the cached and current turn IDs match. A restored
non-terminal snapshot without turn evidence is rehydrated instead of being
reused indefinitely.

The session tailer is a real-time evidence layer over the authoritative API
terminal state. At startup it reads only a bounded tail of existing rollout
files and accepts an active turn only while a Codex process still has that file
open. Files first seen afterwards are consumed from offset zero so an
already-written first event is not lost. Structured lifecycle and item events
can temporarily override a stale API snapshot as running or waiting for user
input. `task_complete` removes that override but never assumes success; the next
API reconciliation supplies succeeded, failed, or interrupted. The tailer
handles partial lines and file rotation. A two-second metadata poll compares
known file offsets and identities, then incrementally reads only changed files
when macOS filesystem events are missed. A malformed record enables
compatibility mode and never changes a task to failed. Prompt, response, and
command-output fields are neither retained nor exposed. Internal chat
passthrough UUIDs are not treated as provider turn IDs.

Codex `source` values are normalized before they cross the adapter boundary.
Both string values and object-shaped sub-agent variants map to stable source
kinds such as `vscode`, `subAgentReview`, and `subAgentThreadSpawn`; unknown
variants remain `unknown` rather than leaking provider JSON into the UI.

Plan progress uses three explicitly ranked sources. A plan returned by the API
has high confidence, `turn/plan/updated` has medium confidence, and a session
`update_plan` call has low confidence. Event plans are cached by turn ID, so a
previous turn cannot overwrite the current one. Only step title/status data is
kept in memory; SQLite stores normalized counts and progress, not plan text.
`100%` is reserved for a succeeded turn, while every other task state is capped
at `99%` even if all known steps are marked complete.

The `TaskSnapshot` model is the public boundary. Raw Codex JSON is dropped after
normalization and is never sent to the React view. The SQLite store contains only
that snapshot, settings and notification keys. `agent-monitor.v1` is the version
identifier for future provider adapters.

`adapter.rs` defines the provider contract (`list`, `events`, `progress`,
`attention`, and `open` capabilities). Codex is implemented first; the UI and
storage layers do not depend on provider-specific JSON shapes.

## Desktop surfaces

The `quick` window is the default glance surface. It is a small undecorated,
cross-workspace window initially positioned at the top-right of the current
monitor's work area. It is draggable and resizable, and reopening it does not
reset the user's position during the current app run. It contains current live
counts (running and needs-action), today's successful completions, and the
highest-priority live tasks. Historical failed and interrupted turns remain in
the snapshot and full dashboard, but are intentionally excluded from the
default glance and tray counts. The `main` window is the deeper, filterable
dashboard and starts hidden; its default `实时` view includes live tasks and
today's completions, while `全部记录` exposes the retained history. The two
windows are mutually exclusive when opened from the tray or from the in-app
window controls; closing either window hides it so the background monitor and
tray remain alive. On macOS, native notifications expose an action that brings
the quick window forward and opens the associated task detail.
