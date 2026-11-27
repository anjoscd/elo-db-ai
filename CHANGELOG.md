# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.7]

### Changed

- A name the user types all the time no longer makes a match specific on its own. The hook now keeps a count of the words in the user's real prompts (folded words and counts only, never prompt text; agent-runtime messages and code are left out) in an optional `prompt_words` table created on first use, so the schema version does not change. The first time, it counts the prompts already in the shadow log. Once at least 500 prompts are counted, a word found in 1.4% of them and in at least 10 ("Apple", "iPhone", "Android" in real use) needs a second keyword to promote a global item; project items are unchanged. A compound keyword is common when every part is. Counting is off while `[shadow] enabled = false`, runs in a write transaction (two hooks at once seed once and never lose a count), and the hook reads only the counts of words that are part of an item keyword. On 720 real prompts since 0.3.6, 9 noisy full deliveries went away and 5 came in, 4 of them relevant.
- `elo bench --replay` skips agent-runtime messages, as the hook does, and uses the replayed log for the word counts.

## [0.3.6]

### Changed

- Global decisions and rules can now be delivered in full. Their proximity weight capped them at about 0.58, below `full_min_score` (0.6), so they were only ever pointers and the abstention gate closed on almost every prompt outside a project. In real prompts a global item with a specific match now uses the same proximity as another project's item (0.5): two or more distinct keywords from the current prompt, or one single-word keyword that is rare among non-note items (normalized idf of at least 0.65, decisions included) and reads like a name: capitalised mid-sentence at least as often as in lower case across item titles and bodies outside code, or with a digit or hyphen. Function words never count. At most two global items per prompt are promoted this way, chosen by score before any layer is packed; the others keep their plain score. On 289 real prompts that the gate had cut, 28 now get a non-law item in full instead of 0. Hook latency is unchanged: the name check scans only the few rare words present in the prompt.
- Notes are no longer delivered from the prompt in real use, neither in full nor as pointers (0% measured use); a touched trigger path still delivers them, and `elo recall` and `elo context` still find them. The `+N notes (elo recall ...)` line is gone.
- Pointers in real use: at most three per prompt, and each item is pointed at most once per session (until a compaction or session start).
- The hook no longer prints the feedback request line (no reports in four days after 0.3.5). The `elo_feedback` MCP tool is unchanged; `[feedback] ask_every` is accepted but unused.
- Benchmark mode is unchanged.

## [0.3.5]

### Added

- Optional abstention gate, `[workspace] abstain_below` (default `0.0`, off). In real prompts, when the best non-law candidate triggered by the prompt scores below the threshold, the envelope keeps only laws and items whose trigger path the agent is touching; other full items, pointers and omission notices are dropped. Path-triggered items do not count towards the score, prompts without words (for example image-only) keep their project fallback, and file-triggered delivery before a tool call is unchanged. `elo stats` shows the abstention rate when the gate is on.

### Changed

- The shadow log uses the same score and threshold as the gate (`abstain_below` when set, otherwise `shadow.abstain_min_score`) and records whether the gate actually abstained, so `elo shadow calibrate` predicts exactly what the gate will do. The previous small-gap heuristic was removed.
- The feedback request line lists only items delivered in full that are not laws (at most five, most recent first), names the exact tool and states it is a single optional call.

## [0.3.4]

### Changed

- Stricter full delivery for non-law items: a note, decision, rule or pitfall goes in full only with a score of at least `full_min_score` (default 0.6) or when the agent is touching one of its trigger paths; otherwise it is a pointer. Notes and decisions also need two distinct keywords from the current prompt; history or a single generic keyword can only suggest a pointer. Laws are unchanged. In real use this cut full non-law deliveries by about four fifths. On the bundled fixture, required full recall moved from 21.8% to 21.0% and session recall from 87.3% to 83.9% (all splits); reachable recall stays at 100% and precision is unchanged. Raise or lower `full_min_score` to trade recall for noise.
- Automatic usage now records why each item was delivered (`via`: `prompt` or `path`). Touching a trigger path counts as evidence only for prompt deliveries; for path deliveries the touch caused the delivery itself, so it would be circular. Path deliveries are not counted towards unused demotion. `elo stats` notes this when the period contains path deliveries.
- The usage meter accepts numbers only as reinforcement of other evidence and counts shorter distinctive terms.
- `elo doctor` warns about a nested data directory.
- Store schema 6 (migrated automatically from 5, with a backup; existing deliveries are recorded as `prompt`).

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
