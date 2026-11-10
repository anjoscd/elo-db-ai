# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
