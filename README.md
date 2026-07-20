# CTCL Temporal Port — desktop app (Phase 1 Dashboard complete: Systems & Groups UI)

The local, installable counterpart to [CTCL](https://commoninstant.org) (Common
Temporal Coordinate Layer). Per the
[Temporal Port App whitepaper](../CTCL/docs/CTCL_Temporal_Port_App_通用時間端口技術白皮書_v0.1.md),
this is a **separate product on a separate stack** from the CTCL Worker — Rust core,
desktop-first, not a rewrite of the hosted API.

**License:** [Apache-2.0](LICENSE), same as [CTCL](https://github.com/kakon77777-commits/ctcl).

## Why Rust, why desktop-first

- The App's core job — a local API / background node other apps and agents call
  into — needs a persistent process and a loopback HTTP server. Mobile OSes
  aggressively suspend and kill background processes; that's a platform
  constraint, not a preference, so mobile is a later "companion," never first.
- Compiled Rust is also a genuine deterrent to casual reverse-engineering (no
  decompiler gets back anything close to readable source) — which turned out to
  matter more here than trying to build a custom-language moat for this
  particular product (checked EML's and PHOSPHOR's actual capabilities first:
  EML transpiles to plain unobfuscated Python and ships its own open-source
  reverse-transpiler; PHOSPHOR's custom VM has no I/O and no EML compilation
  path yet — neither is ready or even the right tool for this specific goal).
  See the whitepaper's own §17 "Desktop First" / §24 Phase 0-6 roadmap.

## Structure

- `ctcl-core/` — the reference-instant + heterogeneous-time-transformation logic:
  encodings (unix_s/ms/us/ns, rfc3339), timescales (utc/posix/tai/gps approx),
  and temporal systems (constant/piecewise/paused/table rate). Ported from the
  CTCL Worker's `src/worker.js` for **behavioral parity, verified**: the CLI's
  `convert` output is byte-identical to `commoninstant.org`'s `/v1/convert` for
  the same input.
- `ctcl-store/` — SQLite-backed persistence for instants, custom systems, and
  Temporal Groups. The local, offline equivalent of the Worker's `CTCL_KV`
  registry — same record shapes, same semantics (re-creating a group bumps its
  version; expanding a group across members isolates per-member errors so one
  bad system id doesn't fail the whole request), different storage engine.
- `ctcl-cli/` — the CLI: `now`, `convert`, `serve` (a local no-terminal web
  preview), and `instant`/`system`/`group` subcommands over `ctcl-store`.
- `ctcl-desktop/` — the **real desktop shell** (Tauri 2). Same
  `ctcl-core`/`ctcl-store` as the CLI, called through Tauri's IPC instead of
  HTTP. A genuine double-click-able window, not a browser preview. Also owns
  three background threads that only run when explicitly enabled (all off by
  default, whitepaper §7.2 "default off"): `local_api.rs` (Phase 2 — a
  loopback-only HTTP gateway, bearer-token auth, capability-scope enforced,
  every call audit-logged), `device_observer.rs` (Phase 3 — periodically
  samples the wall clock against a monotonic anchor and classifies drift /
  sleep-wake / manual clock rollback; only anomalies are persisted), and
  `trigger_engine.rs` (Phase 4 — polls due triggers and dispatches their
  action through a pluggable `ActionDispatcher`).
- `ctcl-mcp/` — Phase 4.5C's **Local MCP server** (binary `ctcl-mcp`): a
  stdio-transport [Model Context Protocol](https://modelcontextprotocol.io)
  server (built on the official [`rmcp`](https://crates.io/crates/rmcp) SDK)
  an Agent Runtime spawns as its own local child process. Opens the same
  `ctcl-desktop-data.sqlite3` file `ctcl-desktop` uses by default, so an
  agent sees the live Triggers/WakeEvents the desktop app's background
  engine actually produces.

## Develop

```bash
cargo build
cargo test               # 105 tests across ctcl-core + ctcl-store + ctcl-desktop + ctcl-mcp
cargo run --bin ctcl -- now
cargo run --bin ctcl -- convert --value 1783420000.5 --from unix_s --to rfc3339 --tz Asia/Taipei
cargo run --bin ctcl -- serve                       # opens a browser, no terminal needed after this
cargo run --bin ctcl -- instant register --label "handoff"
cargo run --bin ctcl -- system create --id user:game_world --epoch 1700000000 --rate 20
cargo run --bin ctcl -- group create --id group:demo --members "utc,tai,tz:Asia/Taipei,user:game_world"
cargo run --bin ctcl -- group expand group:demo
```

Or just double-click `Open-CTCL-Preview.bat` — no terminal needed at all.

For the real desktop app:

```bash
cargo tauri dev --manifest-path ctcl-desktop/Cargo.toml
```

## Status

**Phase 0 (Shared Core) complete**: core time math, SQLite persistence
(instants/systems/groups), the full CLI surface, and a local web preview are
all done and tested (23 unit tests + full manual CLI smoke test of every
subcommand, including real cross-process persistence and every error path).

**Phase 1 (Desktop MVP) Neo-verified 2026-07-12**: a real Tauri window (title
"CTCL Temporal Port") reusing the Phase 0 preview's UI, wired to `ctcl-core`
through Tauri commands instead of HTTP. This is the one thing that genuinely
needed a human's eyes rather than automated testing — Neo confirmed the live
clock and the convert flow (Taipei → Tokyo, Taipei → UTC) both produce correct
results in the actual running window.

**Phase 2 (Local Gateway) Neo-verified 2026-07-12**: the loopback HTTP API
(`local_api.rs`) plus a Settings tab generated dynamically from the backend's
scope list and feature-status data, so the UI can't silently drift from what's
real. 6 real socket-level integration tests (raw `TcpStream`, not mocks).

**Phase 3 (Device Clock Observer) shipped 2026-07-12**: a background sampling
thread (`device_observer.rs`) compares the system wall clock against a
monotonic anchor on a configurable interval (default 20s) and classifies the
gap as `normal` / `drift` / `sleep_wake` / `rollback`
(`ctcl_store::device_observer::classify_gap`, a pure function with its own
unit tests — no OS-specific sleep/wake hooks needed, since a wall-clock gap
that vastly exceeds the requested interval is itself a platform-independent
signal that the process wasn't continuously running). Only anomalies are
persisted; the *current* status (including "everything is fine") is a
separate, in-memory concern read live by the Settings UI. Exposed three ways,
consistently: a Tauri command (`device_observer_status` /
`list_device_events`), a Local API route (`GET /v1/device-events`, gated by
the `device_clock.read` scope that Phase 2 had already reserved), and a new
Settings card. Verified with real background-thread tests across real
wall-clock time (not mocked) plus classify_gap unit tests covering every
branch, including the cross-platform ambiguity in whether a monotonic clock's
elapsed reading includes suspended time. **The Settings UI card itself is
implemented but not yet eyeballed in a real window** — same discipline as
every prior UI change in this project: backend fully tested automatically,
visual confirmation of an actual native Tauri window is left for Neo.

**Phase 4 (Trigger Engine) shipped 2026-07-14**, whitepaper §4.3/§9.4:
$I^*=I_{\text{target}} \Rightarrow \text{action}$ (kind `common_instant`, an
absolute deadline) or $\tau_{\text{custom}}=\tau_{\text{target}} \Rightarrow
\text{action}$ (kind `custom_time`, relative to a stored temporal system's
local time, e.g. an agent's active-time clock). Both reduce to one comparison
in `ctcl_store::trigger::Store::due_triggers`: is a live numeric value
(wall-clock unix seconds, or the named system's current local seconds via the
already-existing `system_now`) `>=` or `<=` a target. `==` is deliberately not
offered — under periodic polling it would almost always step over an exact
instant and silently never fire, a footgun rather than a feature. A trigger
fires exactly once (`active` → `fired`), and only after its action actually
succeeds — a failed dispatch leaves it `active` for retry next tick rather
than being marked fired without the action happening.

Two action kinds: `notification` (logged; no OS toast/system-tray integration
yet, honestly not claimed) and `callback` (hands a URI to the OS's own default
handler — `start`/`open`/`xdg-open` — so whatever app owns that scheme decides
what happens next; CTCL does not register or resolve schemes itself, matching
§7.1's "private scheme only" scope for this phase). Dispatch is behind an
`ActionDispatcher` trait so automated tests never actually open a URI or spawn
a process — a `FakeDispatcher` records calls instead, while the real desktop
app uses `RealDispatcher`. Exposed the same three consistent ways as Phase 3:
Tauri commands (`create_trigger`/`list_triggers`/`cancel_trigger`), a Local
API route (`GET /v1/triggers`, gated by the already-reserved `triggers.read`
scope), and a Settings card (create form + live list + cancel buttons).
14 new tests (9 condition/persistence tests in `ctcl-store`, including a
countdown-style `<=` case; 4 dispatch tests in `ctcl-desktop` including one
real background-thread firing over real wall-clock time; 1 new scope-gated
Local API route test). Same discipline as every prior UI change: backend
fully tested automatically, the Settings card's actual rendering in a native
window is left for Neo.

**Dashboard: Systems & Groups UI shipped 2026-07-14** — this closes the last
piece of Phase 1's own roadmap item list (`Dashboard; Convert; Systems;
Groups; Local API; URI Scheme` — Systems/Groups had been backend-only debt
carried since Phase 1). A new third tab ("時鐘與群組") alongside Home and
Settings: a **Custom Systems** card (create a constant-rate system — id,
epoch, rate, offset, matching `ctcl-cli`'s own `system create` scope, which is
also constant-rate-only; piecewise/paused/table systems remain CLI/API-only)
with a live list showing each system's current local seconds, and a
**Temporal Groups** card (create — id, comma-separated members — with a live
list and a per-group "展開/expand" button that projects the current instant
across every member inline). New Tauri commands: `create_system`/`get_system`
(bundles the stored record with a live `system_now` evaluation),
`create_group`/`get_group` (both thin wrappers over already-tested
`ctcl-store` methods — no new store logic, so the existing 64 tests cover the
underlying behavior unchanged; this increment is UI wiring, not new backend
capability). One real naming gotcha caught before it could become a silent
runtime bug: Tauri's default JS-camelCase → Rust-snake_case argument
conversion is easy to get wrong for a multi-word parameter with no way to
verify it interactively (no native-window test tool) — `create_system`
explicitly declares `#[tauri::command(rename_all = "snake_case")]` and the JS
side passes `epoch_parent_sec` verbatim, so there's no implicit-conversion
assumption to get wrong, matching the explicit-match discipline
`create_trigger`'s struct argument already required.

**Phase 4.5A (WakeEvent Core) shipped 2026-07-19**, per the
[CTCL Agent Wake & MCP Temporal Runtime whitepaper](../CTCL/docs/CTCL_Agent_Wake_MCP_Temporal_Runtime_技術白皮書_v0.1.md)'s
own staged roadmap — its recommended starting slice, ahead of Recurrence, the
Local MCP server, Remote MCP, and every later phase, all deliberately deferred.
The whitepaper's central discipline, `Wake ≠ Act`: CTCL's job stops at
reliably recording that an `agent_wake` trigger fired and letting an external
Agent Runtime retrieve/acknowledge that — it never calls an MCP tool or any
other action on the agent's behalf. A new `ActionKind::AgentWake` variant
(alongside `notification`/`callback`) is intercepted by
`trigger_engine.rs::evaluate_once` *before* reaching `ActionDispatcher` at all
— it never does OS-level I/O — and instead calls
`ctcl_store::wake_event::Store::create_wake_event_from_trigger`, which persists
a `WakeEvent` (`event_id`, `trigger_id`, `agent_id`, `reason`, `fired`,
`observed`, `payload`, `status`, `idempotency_key`) to a new `wake_events`
table. Idempotency key is `trigger-fire:{id}:{created_at}`, stable across
retries of the same arming (e.g. a crash between dispatch and `mark_fired`)
but distinct after a rearm, since re-registering a trigger always stamps a
fresh `created_at`. Status starts `pending`; Phase 4.5A implements only manual
`ack` (`pending → acknowledged`, one-way) — delivery, decision receipts, and
the later `delivering`/`delivered`/`decided_*`/`completed`/`retry_wait`/
`dead_letter` states from the whitepaper's full state machine are not built
yet, matching its own phased scope. Exposed the same three consistent ways as
every prior phase: Tauri commands (`list_wake_events`/`ack_wake_event`,
explicitly `rename_all = "snake_case"` for the same multi-word-argument reason
as `create_system`), a Local API route pair (`GET /v1/wake-events?agent_id=&status=`
and `POST /v1/wake-events/{id}/ack`, gated by two new off-by-default scopes
`wake_events.read`/`wake_events.ack` — same "off by default" discipline as
every other write/read-sensitive scope since Phase 2), and a Settings card
(live list + "標記已確認" ack buttons). 15 new tests: 9 in `ctcl-store`
(`wake_event.rs`, including duplicate-idempotency-key and both trigger kinds),
2 in `trigger_engine.rs` (AgentWake bypasses `ActionDispatcher`; a
misdirected AgentWake action reaching `RealDispatcher` fails loudly instead of
silently no-op'ing), 4 in `local_api.rs` (scope gating + query-param
filtering + the ack transition, real socket-level as always).

**Phase 4.5B (Poll-only Bridge) shipped 2026-07-19**, the whitepaper's own
next-recommended slice (§9.3: "the easiest and most reliable MVP" delivery
mode — an external Agent Runtime polls CTCL rather than CTCL pushing to it,
so no Agent Endpoint registry, no HTTP callback, no retry/dead-letter logic is
needed yet; those are Phase 4.5D). Closes the loop §9.3/§10/§23 actually
asked for: an Agent Runtime can now poll pending WakeEvents, ack one, do its
work, file a decision receipt, mark the event complete, and schedule its own
next wake — entirely over the Local API, without the desktop UI in the loop.
- **`wake_events.completed_at` + `Store::complete_wake_event`**: `acknowledged
  → completed`, same one-way-transition discipline as `ack_wake_event` (and
  deliberately requires a prior ack — completing straight from `pending` would
  mean "completed" no longer implies an Agent Runtime actually saw it).
- **New `decision_receipts` table + `decision_receipt.rs`** (whitepaper §6.3):
  `Store::create_decision_receipt` (validates `decision` is `no_action` or
  `action` — not an open string — and that the target WakeEvent exists) and
  `Store::get_latest_decision_receipt`. CTCL never reads `decision` to decide
  anything itself — a receipt is filed, not acted on, the same boundary as
  `ActionKind::AgentWake` itself. The schema allows multiple receipts per
  event (matching §6.3 exactly, no invented UNIQUE constraint); the API
  surfaces the most recent.
- **Local API Trigger write** (§10.1, previously Tauri-only): `POST
  /v1/triggers` (create — reuses the existing "re-post rearms" convention, so
  no separate `/rearm` endpoint), `POST /v1/triggers/{id}/cancel`, `GET
  /v1/triggers/{id}`. This is what actually lets an Agent Runtime "self-
  schedule its next wake" (§23/§25's `schedule_next_wake_if_needed`) — CTCL
  doesn't read a receipt's `next_wake` field and act on it; the agent calls
  this endpoint itself, same non-coupling discipline as everywhere else here.
- **WakeEvent complete + Decision Receipt Local API routes** (§10.2/§10.4):
  `POST /v1/wake-events/{id}/complete`, `POST`+`GET
  /v1/wake-events/{id}/decision`, plus a single-item `GET
  /v1/wake-events/{id}` that Phase 4.5A hadn't added (list-only).
- **Three new off-by-default capability scopes**: `triggers.cancel` (kept
  separate from `triggers.write`, per the whitepaper's own §11 scope list),
  `wake_events.complete`, `decision_receipts.write` — "all side-effecting
  capabilities default off" (§11), same policy as every scope added since
  Phase 2, even where the whitepaper's own suggested default differs (it
  suggests `triggers.read`/`wake_events.read` on by default; this project
  keeps them off, for consistency with `device_clock.read`/`history.read`
  rather than re-litigating that choice per scope).
- Settings card gained a "標記完成" (mark complete) button for acknowledged
  WakeEvents, and shows the latest decision receipt's decision + summary
  inline when one exists — read-only, since authoring a receipt (run_id, tool
  calls, cost data) is an Agent Runtime's job, not something a human types
  into a form.
- 24 new tests: 5 in `decision_receipt.rs`, 3 in `wake_event.rs`
  (`complete_wake_event`'s transition + its two rejection paths), 16 in
  `local_api.rs` (every new route, each scope-gated then verified to succeed
  once granted, real socket-level as always) — **93 tests total** across the
  workspace, `cargo build --workspace` (full link, not just `check`) clean.

**Phase 4.5C (Local MCP server) shipped 2026-07-20**, whitepaper §12 — a new
`ctcl-mcp` binary crate, a real [Model Context Protocol](https://modelcontextprotocol.io)
server over `stdio`, built on the official Rust SDK
([`rmcp`](https://crates.io/crates/rmcp) v2.2.0, `server`+`macros`+`transport-io`
features) rather than hand-rolling the JSON-RPC framing. Matches §12.1's
"Local MCP" deployment form and §9.1's local-process trust model: whoever can
spawn this binary already has local execution rights, so unlike the Local
API's loopback HTTP surface, tool calls here are **not bearer-token gated** -
but capability scopes still are, checked on every call through the same
`require_scope` gate, logged to the *same* `audit_log` table the Local API
already writes to (`method="MCP"`), so `GET /v1/audit` shows both interfaces'
calls side by side. §12.3's "讀寫分離" (read/write separation) is honored by
construction: every write-capable tool lives only here, not on a future
Remote MCP.

15 tools implemented from the whitepaper's own §12.2 list - `ctcl.now`,
`convert`, `register_instant`, `get_instant`, `list_systems`, `system_now`,
`create_system`, `expand_group`, `create_trigger`, `list_triggers`,
`cancel_trigger`, `list_wake_events`, `ack_wake_event`, `complete_wake_event`,
`schedule_pulse` (a convenience wrapper over `create_trigger` letting an agent
self-schedule its next wake `after_seconds` from now, without computing an
absolute timestamp itself - whitepaper §25's `schedule_next_wake_if_needed`).
**Deliberately not implemented**: `ctcl.inspect_boundary`,
`ctcl.resolve_temporal_context`, `ctcl.plan_shared_instant` - these are CTCL
Web (commoninstant.org, a separate Cloudflare Worker in JS) features with no
equivalent in `ctcl-core`/`ctcl-store` today; faking them or silently
omitting them would violate this project's honesty discipline, so the gap is
declared in the server's own MCP `instructions` string instead. No tool for
writing a decision receipt exists either, because the whitepaper's own §12.2
list doesn't name one - an agent posts one over the Local API's Phase 4.5B
route instead.

Two real engineering points worth naming: (1) **`Store::open` now sets a 5s
`busy_timeout`** - Phase 4.5C is the first time two separate OS processes
(`ctcl-desktop` and `ctcl-mcp`) can legitimately have the same SQLite file
open at once, and rusqlite's default `busy_timeout` of 0 would turn routine
write contention into a hard `SQLITE_BUSY` error instead of a brief wait; (2)
tool output intentionally uses `Result<String, String>` (a JSON string as
text content) rather than `rmcp`'s `Json<T>` structured-output wrapper, which
would have required adding a `schemars::JsonSchema` derive to every domain
type in `ctcl-store` (`WakeEvent`, `Trigger`, ...) - an MCP-layer concern
that has no business leaking into the core persistence crate. The agent
still gets the exact same data, just as a JSON string rather than a
schema-typed structured field; that tradeoff is written down here rather
than silently decided.

12 new tests: 11 direct method-level tests (bypassing the transport, calling
the tool functions as plain async methods - scope-gating, audit-logging, and
every tool's underlying behavior) plus **one real end-to-end protocol test**
that spawns the actual compiled `ctcl-mcp` binary as a child process via
`rmcp`'s own `TokioChildProcess` client transport, lists tools for real,
calls `ctcl.now` for real, and confirms a genuinely scope-gated tool is
genuinely refused - the same "real socket, not mocks" discipline
`local_api.rs`'s tests already hold this project to, extended to the new
protocol boundary. **105 tests total** across the workspace, `cargo build
--workspace` (full link) clean.

Still to come: Phase 4.5D (Active Delivery — loopback HTTP/local-process
push, retry, dead-letter, an Agent Endpoint registry) from the Agent Wake
whitepaper, and — unrelated, still paused per Neo's direction while this took
priority — Phase 5 (team sync — note this needs an actual sync-backend
*product* decision, e.g. self-hosted vs. hub on commoninstant.org vs. a new
paid tier, not just more local Rust code, so it's being left for Neo to weigh
in on rather than architected unilaterally) and Phase 6 (mobile companion,
explicitly last per the whitepaper's own ordering).

This is intentionally **not** trying to replicate CTCL Web's whole surface at
once — it starts from the same core math and grows outward, same as the Worker
did.
