# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Hook adapters for Codex, Gemini, Cursor and Copilot, native event aliases, deferred prompt context and edit-driven map synchronization.
- `elo setup <tool> [--project]` prints hook/MCP configuration for nine tools without writing configuration files.
- `elo export` generates scoped agent rules with preserved user text, safe filenames, obsolete-rule cleanup and dry-run previews.
- Binary integration coverage for hook protocols, configuration snippets and rule exports.

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
