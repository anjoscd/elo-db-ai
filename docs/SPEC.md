# ELO specification (v0.1, draft)

This is a behavioral specification. It says **what** ELO must do and how that is checked, not how any existing tool does it. ELO is a clean-room implementation: contributors must not copy code from other context engines whose licenses are incompatible with the project license (FSL-1.1-ALv2).

Key words MUST, SHOULD, MAY follow RFC 2119. Sections marked **(deferred)** are design intent, not a commitment for M0/M1.

---

## 1. Concepts

| Term | Definition |
|---|---|
| **Workspace** | A folder tree that contains one or more projects. ELO keeps its store in `<workspace>/.elo/`. |
| **Project** | A named folder inside the workspace, declared in `elo.toml`. Project roots MUST NOT overlap. A nested root is a configuration error reported by `elo doctor`. |
| **Link** | A named relationship between two or more projects around a subject, such as `payments: [api, payment-gateway]`. |
| **Scope** | Where a knowledge item applies: `global`, `project:<name>`, or `link:<name>`. Every item has exactly one scope. |
| **Item** | A unit of knowledge: `law`, `rule`, `decision`, `note`, `pitfall`. |
| **Law** | A rule marked non-negotiable. Laws have a size cap and a trust requirement (§6). |
| **Session** | One agent conversation, identified by the `session_id` the hook receives. |
| **Event** | One hook invocation, identified by `(session_id, event_seq)`. |
| **Budget** | The maximum UTF-8 bytes ELO may emit per event, counting everything it prints. |

## 2. Configuration (`elo.toml`)

```toml
[workspace]
budget_bytes = 9000
min_score = 0.35

[[project]]
name = "api"
root = "api/"
aliases = ["api", "backend"]          # names that count as explicit naming (§3.3)

[[project]]
name = "payment-gateway"
root = "services/payment-gateway/"
aliases = ["payment-gateway", "gateway"]

[[link]]
name = "payments"
projects = ["api", "payment-gateway"]
keywords = ["payment", "refund", "chargeback"]                 # specific words only
code_signals = ["payment-gateway", "PAYMENT_GATEWAY_URL"]      # strings that reveal the link in code

[quotas]          # share of the budget per layer; unused share flows to the next layer
project = 0.50
link = 0.25
global = 0.25
```

Items live in `.elo/` (SQLite). They can be imported from and exported to Markdown/JSONL.

**Zero configuration is the default.** Every section of `elo.toml` is optional. `elo init` writes what it discovered (§2.1, §2.2), and the user only edits to correct it. ELO MUST work on any repository or monorepo with no hand-written project or link list.

### 2.1 Project discovery
`elo init` and `elo doctor` detect projects from, in order:
1. workspace manifests (`pnpm-workspace.yaml`, `package.json` workspaces, `Cargo.toml` workspace members, `go.work`, `pyproject` tool configs, `settings.gradle`);
2. nested VCS roots;
3. top-level folders that contain their own package manifest (`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `pom.xml`, `deno.json`, `*.csproj`).

A single-project repository is one project at the root. Overlapping candidates resolve to the outermost manifest, and nested ones are reported.

### 2.2 Link discovery
Links are **inferred from evidence in the code**, then shown to the user as suggestions. Evidence types, each recorded with file and line:
- **imports** across project roots, or of a sibling workspace package;
- **shared identifiers:** the same environment variable name, queue/topic name, or HTTP route path defined in one project and used in another;
- **service references:** a project's name, package name or deploy hostname appearing as a string in another project;
- **shared schema:** the same database table or RPC name read or written by two projects (from the database map, M4).

`elo links suggest` lists candidate links with their evidence count and a strength score. `elo links accept <name>` writes them to `elo.toml`. Accepted links are re-checked on `elo doctor`, and a link whose evidence disappeared is flagged, never deleted silently. Link names default to the dominant shared token (`payments`, `auth`) and can be renamed. Link keywords are suggested from distinctive terms (high idf) in the evidence and in link-scoped items.

## 3. Scope resolution

Input: hook event `{session_id, event_seq, cwd, prompt?, tool_paths?}` plus the session state (§5).

### 3.1 Active projects
1. **cwd:** the project whose root contains `cwd` is **primary**. Roots never overlap (§1), so there is at most one. If `cwd` is in no project, there is **no primary**, and every active project is secondary with equal standing. No tie-break is needed, because secondaries share one proximity value (§4.3).
2. **Session files:** projects of files touched in this session within the last `touch_window` events (default 20) are **secondary**. Touches expire, so an old edit doesn't keep a project active forever.
3. **Explicit naming:** a project alias that appears in the prompt as a whole word, **outside code blocks, quoted text and file paths**, is **secondary** for this event only.

If cwd is the workspace root and no rule matches, the active set is empty and only `global` applies.

### 3.2 Active links
A link is active if any of these holds:
- (a) two or more of its projects are active;
- (b) a file touched within the window, **inside the root of one of the link's projects**, contains one of its `code_signals`;
- (c) a link keyword is in the prompt **and** at least one of its projects is active.

### 3.3 Eligible items
An item is **eligible** for an event if and only if:
- its scope is `global`; or
- its scope is `project:P` and P is active; or
- its scope is `link:L` and L is active.

Links never make a whole foreign project eligible. Knowledge shared through a link MUST be stored with scope `link:L`. A `project:B` item is never eligible while B is inactive, whatever the keywords or links. The only exception is an explicit `elo context` request, which is not a hook event.

Each activation records its reason, and the reason is emitted (§4.6).

## 4. Selection

### 4.1 Candidates
Eligible items whose triggers match the event: keyword (whole word, case-insensitive, accent-insensitive, outside code blocks), path glob against touched files, symbol in touched files, or `always` (laws and rules may be marked always-on within their scope).

### 4.2 Relevant set
`relevant = { c ∈ candidates : score(c) ≥ min_score } ∪ { applicable laws }`, where an applicable law is a law candidate (§4.1). Items outside this set are never emitted, not even as pointers.

### 4.3 Score
Every component is normalized to [0, 1]:

`score = w_p·proximity + w_r·recency + w_i·importance + w_m·match_strength`, with Σw = 1.

- **proximity:** touched file 1.0 · same directory 0.8 · primary project 0.6 · secondary project 0.5 · active link 0.4 · global 0.2
- **recency:** `0.5 ^ (age_days / half_life_days)`, where `now` is an explicit input (§4.7)
- **importance:** an author-set value in [0, 1]. Learned adjustments are deferred (§6.3).
- **match_strength:** `max` over matched triggers of `idf(trigger)`, normalized. Generic words get low idf.

Ties break by `(kind priority: law > pitfall > rule > decision > note, updated_at desc, id asc)`.

### 4.4 Laws
- A law body MUST be ≤ `law_max_bytes` (default 400).
- **Law budget, enforced at write time:** `law_budget = 0.5 × budget_bytes − reserve`, where `reserve` (default 300 bytes) covers the wrapper, the reason prefixes and one overflow line. For every scope combination that can be active together (global + any project + its links), the rendered size of all laws in that combination MUST be ≤ `law_budget`. `elo add`, `elo promote` and imports reject a law that would break this, naming the combination. So, with a valid store, every applicable law always fits in full.
- Laws are emitted first, do not count against layer quotas, and are part of `used`.
- **Hard budget:** `used` MUST NEVER exceed `budget_bytes`. If a store was edited by hand and violates the law budget, ELO emits laws until `budget_bytes − reserve` and then a single overflow line naming the missing law ids. That event is a law-coverage failure, and `elo doctor` fails until the store is fixed.
- **Laws and dedup:** a law already served in the session (§5) is not repeated in full. It is emitted as a one-line reminder `[law] L3 (in force)` until the next compaction. For coverage, a law counts as delivered if its body was served in full in the current session and compaction window (EVALUATION §2).
- **Real-mode noise reduction:** in real mode (`allowed_scopes` empty), the one-line law reminder is also suppressed. A law already delivered in the current session/compaction window is not emitted at all until the next compaction, when it returns in full. This removes a large source of repeated pointer noise without changing benchmark behavior.
- **Rule 3 — repeat window for all pointers:** any pointer (rule, decision, note, or index line) that was already pointed within the last 20 events is suppressed, unless the current prompt matches two or more of its distinct keywords. This applies to both in-focus and out-of-focus items and keeps the context from repeatedly advertising the same item.
- **Rule 4 — hard pointer cap:** the total number of pointers per event is capped at 8 across all categories. The highest-scored candidates are kept; the rest are summarized in a single `+N more → elo context` line.

### 4.5 Quotas and pointers
- After laws, each layer (project → link → global) fills up to its quota of the **remaining** budget with the highest-scoring relevant items, as full bodies.
- Relevant items that don't fit are grouped by scope and topic into **pointers**: `+3 more on payments (R4, R7, D2) → elo context payments`. Pointers list item ids and count against the budget.
- If even pointers don't fit, the last line is `+N relevant items omitted → elo explain`.
- **Abstention:** a layer with no relevant items emits nothing. An event with no relevant items emits nothing at all.

### 4.6 Output format
```text
<elo-7f3a9c event="s1:42" budget="9000" used="3120">
[law · global] L3: Never run migrations against production.
[project:api · via cwd] R4: ...
[link:payments · via code signal in services/pay.ts] D2: ...
+2 more on api (R9, N3) → elo context api
</elo-7f3a9c>
```
- The wrapper tag carries a per-event **nonce** (≥ 48 random bits, from a seeded RNG in the benchmark). Item text cannot predict it, so it cannot close the wrapper.
- Item text is escaped before emission: any `<elo` and `</elo` sequence, plus CR and other control characters except `\n`, are replaced (`<elo` → `‹elo`). Each item is printed on lines prefixed by its reason tag. A newline inside a body is followed by two spaces of indentation, so an item cannot forge another item's reason line.
- Every byte inside and including the wrapper counts toward `used`.

### 4.7 Determinism
Output is a pure function of `(store snapshot, config, event, session state, now)`. The hook passes the real clock; the benchmark fixes `now` and the event sequence. Same inputs MUST produce byte-identical output.

## 5. Sessions and concurrency

- Session state lives in `.elo/sessions/<session_id>` and is never shared across sessions.
- **Serialization:** events of the same session are processed under an exclusive per-session file lock, so concurrent hooks of one session run one at a time. Different sessions never block each other.
- **Ordering:** state updates are keyed by `event_seq`. An event whose `event_seq` is ≤ the last applied one is still answered, but its touches are merged without moving the window backwards, and its served marks only add. Replaying an event never removes state.
- State holds touched files with their `event_seq`, items served (by `(id, content_hash)`), and a compaction counter.
- **Delivery:** an item is marked served only **after** the output is fully written to stdout. A crash before that means it will be served again (at-least-once is the accepted guarantee).
- **Dedup:** an item served in this session is not re-served unless its content hash changed, or the session reports a compaction (the agent's compaction or session-start event resets served items).
- **Store writes** (add, supersede, import) are SQLite transactions. Hooks read a consistent snapshot and never write items.
- Session state older than `session_ttl_days` (default 7) is garbage-collected.

## 6. Memory and trust

### 6.1 Item fields and validity
`id`, `kind`, `scope`, `title`, `body`, `triggers`, `importance`, `created_at`, `updated_at`, `expires_at?`, `status` (`active` | `superseded` | `expired` | `archived`), `superseded_by?`, `origin` (`user` | `agent` | `import`), `evidence?`.

Invariants, enforced in one transaction per write:
- `superseded_by` points to an existing item of the same kind, in the same scope or a wider one.
- An item has at most one active successor. A second supersession of an already-superseded item is rejected with the current successor's id.
- Supersession chains MUST be acyclic.
- **Effective status** is computed on read: an `active` item with `now ≥ expires_at` is treated as `expired` by every reader, hooks included. The stored `status` is updated to `expired` only by write paths (`elo add`, imports, `elo maintain`). Hooks never write items. Laws and rules cannot have `expires_at`.
- A correction MUST name what it corrects (`supersedes`). Otherwise it is stored as a plain note.
- Default retrieval (hooks, `elo recall`) sees `active` only. `--history` shows everything. `elo forget <id>` leaves a tombstone.

### 6.2 Trust boundary
- Item text is **data**. ELO emits it inside the `<elo>` wrapper with its origin, and never as top-level instructions of its own.
- Only `origin = user` items may be `law`. Promoting an agent- or import-originated item to law requires an explicit user command (`elo promote <id>`), which is logged.
- Importers default every item to `note` with `origin = import`, and flag text that looks like instructions to the agent ("ignore previous…", tool-call syntax) for review.
- **(deferred)** §6.3 learned importance MUST NOT use signals the item itself can induce, such as the agent quoting it, and MUST NOT change laws.

### 6.3 Feedback loop (deferred)
Learned `importance` adjustments from user-confirmed usefulness. Off by default, and evaluated on the benchmark before it is enabled.

## 6.4 Calibrated confidence (after M1)

- Each candidate gets `p_useful ∈ [0, 1]`, a **calibrated probability** that the item is in `required` for the event. It comes from the raw score through a monotonic calibrator (isotonic regression; Platt scaling as fallback when labels are few) fitted on the benchmark `dev` split, never on `test`.
- **Calibration quality** is reported on `test`: Expected Calibration Error (ECE, 10 bins), Brier score and a reliability diagram. A calibrator ships only if its ECE is ≤ 0.05.
- `elo explain` shows `p_useful` per item. `min_p` (optional) MAY replace `min_score` as the relevance gate once calibration ships. Laws are never gated by `p_useful`.
- The calibrator is a small local file in `.elo/` with its training commit and metrics recorded. No network, and nothing learned leaves the machine.

## 6.5 Scoreboard (with M0)

- ELO keeps local, append-only counters per session: events, items emitted (full, pointer), bytes, laws delivered, abstentions, and cross-project emissions (items whose scope came only through explicit naming or a link, kept as an audit trail).
- **Usefulness signal:** an item counts as *used* when the user confirms it (`elo good <id>` / `elo bad <id>`), or when the agent's next tool action touches a file or symbol named in the item's triggers. Agent quoting alone never counts (§6.2).
- `elo stats [--since date] [--by project|scope|kind]` prints the scoreboard. It shows used ÷ emitted, the bad-report rate, bytes per event, law delivery, and the trend between ELO versions.
- Scoreboard data is private to the workspace. Only aggregates that the user explicitly exports may leave it.

## 7. Code map (M3)

- Languages first: TypeScript/TSX/JavaScript (including Deno). Others are best-effort.
- Entities: file, function, method, class, component, **callback**. Anonymous functions get an identity from file + container + role, such as `Deno.serve#handler`.
- Edges, each typed and reported separately: `imports`, `calls`, `renders` (JSX `<X/>`), `registers_callback`, `re_exports`.
- Resolution goes through import bindings and `tsconfig` path aliases. Unresolved references are stored with location and reason (`dynamic`, `unknown-binding`, `computed-member`).
- Every reference query returns `{resolved: [...], unresolved: [...], coverage}`. ELO MUST NOT print a bare "0" when `unresolved` is non-empty for the queried file set.
- **Freshness:** hooks never reindex synchronously. A file changed after the last index is reported as `stale: [files]` in query results. Reindexing runs on `elo map sync` or a background watcher (deferred).

## 8. Database map (M4, deferred)

Postgres catalog plus migrations (libpg_query). Client calls `.from('t')…select/insert/update/upsert/delete` give `reads`/`writes`, and `.rpc('f')` gives `invokes`. Dynamic names are unresolved. Structural edges never claim access, and policies are reported, not evaluated.

## 9. CLI (initial)

```text
elo init                       create .elo/ and elo.toml (store schema version 1)
elo hook <event>               agent hook entry (JSON on stdin, see §5)
elo context <topic|project>    what would be injected, and why (explicit; may cross scopes)
elo add <kind> --scope ... --title ... [--supersedes id] [--expires date]
elo promote <id>               make a user-confirmed item a law
elo recall <query> [--history] [--scope ...]
elo explain [event]            candidates, scores, relevant set, what was dropped and why
elo doctor                     overlapping roots, oversized laws, generic triggers, store health
elo bench                      evaluation harness
elo stats [--since] [--by]     local scoreboard (§6.5)
elo good|bad <id>              usefulness feedback for the scoreboard and calibration
```

## 10. Non-functional requirements

- **Reference machine for latency:** documented in `bench/README.md` (CPU, RAM, OS). The fixture is a 200k-line TS workspace with 2,000 items.
- **Latency protocol:** a fixed query and event set (`bench/latency.toml`). **Warm** means 20 discarded warm-up runs, then 500 measured runs in a fresh process each time, with the store in the OS page cache. **Cold** means the first run after writing a fresh copy of the store, reported as a separate number with no p95 claim. Report p50, p95 and max per operation.
- **Hook targets (warm, reference machine):** p95 < 100 ms with no synchronous reindex.
- **Map query targets:** p95 < 50 ms. `elo map sync` of one ≤ 2,000-line file < 200 ms.
- **No network** in the default build. CI runs the tests with networking disabled.
- **Distribution:** one self-contained binary per platform (macOS arm64/x64, Linux x64/arm64 musl, Windows x64). Optional LSP enrichment uses external language servers and is not part of the binary.
- **Store versioning:** schema version recorded from v1. Migrations run under an exclusive file lock, inside a transaction, with a backup before upgrading. A binary refuses to open a newer schema.
- **Paths:** stored normalized (forward slashes, resolved symlinks, case-folded on case-insensitive filesystems). Paths outside the workspace never activate a project.
