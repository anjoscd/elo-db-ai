# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.3]

### Fixed

- Resolve database-using CLI commands and hooks from the nearest ancestor with a `.elo/elo.db` file, skipping incomplete `.elo` directories; setup, update, bench, and hook dispatch do not require a CLI root. Only init creates the main store.
- Preserve existing-store migrations and keep hooks silent when the database is missing.
- Warn about nested ELO databases that hide workspace laws, up to three directory levels below the root; skip unreadable entries, symlinks, and generated directories.
- Initialize new stores under the migration lock using the connection that creates the database.
- Restore both init and explicit-root guidance when MCP cannot find a root.

## [0.3.2]

### Changed

- Real prompts demote unused decisions, rules, and notes to pointers after six events with full deliveries within 14 days, configurable through `demote_unused_min` and `demote_unused_days`. Automatic usage or a `used` report prevents demotion; laws are exempt, and the rolling window allows retries. Statistics and explanations show the reasons; benchmarks preserve the previous policy.
- Recent notes and decisions no longer arrive in full on a single common word: they require a specific word (a digit or eight characters) or two keywords.
- `elo shadow calibrate` replaces owner ratings with the `agent ignored` proportion among reports tied to items delivered in the same event, showing `n/a` when no reports exist.
- A file being touched keeps its items in full even when they are demoted for disuse.
- An optional index keeps the disuse lookup fast (about 34 ms p95 with 22,000 deliveries); it is created on open when missing, and the schema stays at version 5, so older builds keep working on the same store.

## [0.3.1]

### Fixed

- `elo_maintain_propose` and `elo maintain review` now validate paths with the same rule used at delivery: a trailing `*` is a prefix of the whole relative path (`dir/*` matches files in any subfolder), anything else is an exact file. Directory globs were rejected in 0.3.0 even when files existed. `**` and `*` in the middle are refused with a clear message; files must be regular files inside the root (no symlink escape), and the search skips `node_modules`, `.git`, `target` and `dist`.

## [0.3.0]

### Added

- Shadow abstention telemetry: on each prompt the selector records, without changing what is delivered, whether it would have stayed silent (top score, gap, matched words). Stored locally in `.elo/shadow/selector.jsonl` without prompt text or titles, rotated at 5 MB. `elo stats` shows the would-abstain rate and `elo shadow calibrate` compares score bands with observed usage and owner feedback, so the threshold can be chosen from data before any gate is enabled.
- Agent self-report, in shadow mode: every `[feedback] ask_every` prompts (default 5, `0` disables) the envelope asks the agent to call the new MCP tool `elo_feedback` with the ids it used or ignored. Only ids actually delivered in the same session within the last 40 events are accepted; reports are stored apart from the owner's `good`/`bad`, cross-checked against objective usage signals (`corroborated` / `contradicted` in `elo stats`) and do not change selection.
- Agent-proposed maintenance: MCP tools `elo_maintain_catalog` (compact catalog, 30 items per page, prioritising always-on, never-delivered and never-used items) and `elo_maintain_propose` (`set_paths`, `drop_always`, `set_scope`, `merge`). Every operation is validated before it is accepted (globs relative to the root, matching real files, no symlink escape; known scopes; merges only within the same kind and scope and never for laws) and stored as a pending proposal. Nothing is applied until `elo maintain review --apply <ids>` or `--apply-nonlaw`; laws need an explicit id. Proposals are revalidated and applied atomically, and only triggers or scope change, never titles or bodies. An idle-cycle invitation appears at session start after 24 hours and five sessions (`[maintain] auto_invite`).
- `elo stats` reports pointers per event (p50/p95) and the share of repeated pointers.

### Changed

- Much less noise per prompt in live mode: a law already delivered in the session is no longer repeated as a reminder (it returns in full after compaction), an item already delivered in full is not pointed again, a pointer is not repeated within 20 events unless the current prompt matches two of its keywords, and at most 8 pointers are sent per event, the rest summarised in one short line. Measured on real use: pointers per prompt went from 13/40 (p50/p95) to 1/4 and the median envelope from 1,096 to 275 bytes. Benchmark mode is unchanged.
- Store schema 5 (migrated automatically from 4, with a backup).
- The hook latency test runs in its own test binary.

## [0.2.0]

### Added

- `elo update [--check] [--version <VERSION>]` for manual, network-free-by-default self-updates. Uses the system `curl` to download release assets, verifies SHA-256 checksums, and atomically replaces the running binary while keeping the previous one as `<binary>.old`.
- Prebuilt release binaries for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, packaged as `elo-<version>-<target>.tar.gz` with a `SHA256SUMS` file and attested build provenance.
- Pointer limits, response-based usage signals, conversation subject matching, project-image association, rare-word handling and recent-decision recall in context selection.
- `ELO_REPLAY_DUMP` support for replay benchmark output.

### Changed

- Renamed CLI flags to English: `--step`, `--keywords`, `--apply`, `--scope`, `--format`, `--project`, `--output`. The old Portuguese names remain hidden aliases. The `elo setup` tool stays a positional argument (`elo setup <TOOL>`).
- Renamed `windsurf-legado` export format to `windsurf-legacy` with `windsurf-legado` as a hidden alias.
- Enforced Claude/Codex denials exit with code 2 and explain the decision on stderr; shadow guards remain silent.

## [0.1.0]

### Added

- Local Rust CLI with SQLite storage, workspace initialization, project/worktree discovery and configuration diagnostics.
- Memory items (`law`, `rule`, `decision`, `note`, `pitfall`) with scopes, keyword/path triggers, search, JSONL import, supersession, expiration and archival.
- User-origin requirements and size/budget validation for laws.
- Context selection with scores, byte budgets, layer quotas, session deduplication, pointers and separate limits for notes and decisions.
- Claude Code hooks for `UserPromptSubmit`, `SessionStart`, `PreToolUse` and `PostToolUse`, with live and shadow modes.
- Configurable tool guards with shadow logging and enforced `ask`/`deny` decisions, plus file-anchored context before tool calls.
- Local delivery statistics, manual feedback and automatic usage evidence from tool inputs.
- Incremental tree-sitter code map for JavaScript/TypeScript, including JSX/TSX, with entities, imports, calls, rendering, callbacks and re-exports.
- Caller, callee, importer and transitive impact queries with unresolved references and staleness reporting.
- Project/task board with milestones, dependencies, deferred work, conversation handoffs and session-start summaries.
- Reusable recipes stored as triggered notes.
- Maintenance plans and backup-before-apply consolidation, expiration updates and automatic usage cleanup; manual-review suggestions for similar memories and duplicate laws.
- Labelled context-selection benchmark and read-only local log replay with aggregate reports.
- English and Brazilian Portuguese documentation with installation, hook configuration and command examples.
