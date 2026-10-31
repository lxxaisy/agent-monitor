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

If discovery omits a live session, ordinary tool items for the same observed
turn keep updating its file evidence without repeating a full thread listing.
New sessions and turn boundaries still trigger discovery; scheduled metadata
reconciliation continues. User-input requests are parsed and queued independently
of whether the session already has API metadata.

The activity collector reuses its prepared request-usage upsert statement.
ZCode request rows remain ordered by `started_at,id`; the final model/provider
pair in that existing order is applied once per turn, skipping identical values.
All request usage is still checked, including corrections in old rows after a
WAL change. This avoids per-request turn-metadata rewrites without delaying
live hooks or changing Token/missing-value semantics.

Unchanged execution and metadata upserts do not write to SQLite. A collection
reuses its derived activity cache when persisted data, source health and live
evidence are unchanged, while updating displayed durations and freshness.

History retention defaults to 30 days. Cleanup runs at startup, every six hours,
and after a retention-setting change; it removes only expired terminal turns
and their request usage. The saved cleanup cutoff also excludes expired terminal
turns on subsequent imports so source history cannot restore deleted rows.
Ongoing turns keep their usage, and unread alerts are retained. The setting is
available in the installed application's Settings and Diagnostics page.

Diagnostic version probes run off the UI thread and outside the collector state
lock, with a two-second subprocess deadline. Output uses a private, bounded
regular-file capture rather than waiting for pipe EOF, which can be retained by
an independently spawned app-server. A path change during the query discards
the old binary's version instead of associating it with the new path.

Activity collection owns an immediate SQLite transaction. Any early error or
failed commit rolls it back before the next refresh, while source-specific
savepoints continue to isolate unavailable clients. A failed manual sync has a
retry action and clears when a later collection succeeds.

BLE scans and reconnects share one lazily initialized adapter on Tauri's
persistent async runtime. The macOS btleplug backend keeps a CoreBluetooth
thread alive per adapter; recreating it on every reconnect accumulated kqueue
descriptors until SQLite and source files failed to open. Failed adapter
initialization remains retryable. The macOS resource regression exercises 32
adapter acquisitions and local snapshot writes without scanning or connecting
to a device; run it explicitly with Bluetooth access and one test thread.

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

History is paginated by `(client, sessionId)` before returning results to the
view. A conversation summary reports its latest matching root round, matching
round count, descendant count and normalized Token totals. Root records already
contain descendant request usage, so conversation totals sum roots exactly once.
Expanding a conversation fetches its root rounds in pages of ten; opening a round
keeps child Agents under that round. Status filters scope both the displayed
round count and totals. Source titles remain unchanged in storage. List previews
use the first task line after the known browser-context and attachment envelope;
ordinary XML and other document headings are preserved. Detail exposes the
complete original source separately from the compact round heading. No project,
path or sibling field replaces an empty title.
Child rows always use their Agent/round identity as the main label and keep
their source title as secondary text, even when it is empty. A leading Markdown
link in a list preview displays its label once; the source remains unchanged.

The separate child-Agent history uses `childrenOnly` to retain access to every
visible child record, including records whose source does not identify a root
round. It keeps its own pagination and filters; those records do not add root
tasks or get added a second time to a conversation's rolled-up usage.

Tibo announcements have their own navigation destination. The latest stored
post remains visible on Overview and the default quick window, including when
polling is paused or the source is stale. Publication time and source freshness
remain distinct from the account's actual reset state. Only the existing native
news update event refreshes these surfaces; there is no extra polling timer.
Saving announcement settings broadcasts the same update event to the other
windows immediately. Encoded public post text is decoded with the browser's HTML
parser and rendered as text, without interpreting its markup.

Explicit named clocks such as “By midnight PST” are anchored to the post's day
in the stated offset; midnight is the end of that source day. Timing metadata
preserves whether the source describes a deadline, a point or a relative window.
The console, overview and quick window render that timestamp in the machine's
timezone, retaining deadline wording without claiming the reset has completed.

The main sidebar and quick window use macOS semantic vibrancy materials; their
CSS adds restrained edge highlights and follows the system color scheme.
The content workspace stays opaque for readability. Browser fixture screenshots
verify CSS only and do not establish native vibrancy acceptance.
Quick-window task rows do not use opacity entrance animation: native WebKit
capture showed blank composited rows while their accessibility text remained
present. Hover/pressed transitions and the detail animation remain independent.

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
