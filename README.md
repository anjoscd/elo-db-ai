# ELO-DB-AI

A local context and memory engine for coding agents, written in Rust. Works with any IDE and any model: an MCP server plus hook adapters for Claude Code, Codex, Gemini CLI, Cursor and GitHub Copilot.

[Português do Brasil](README.pt-BR.md) · [Specification](docs/SPEC.md) · [Evaluation](docs/EVALUATION.md) · [Research](docs/RESEARCH.md) · [Changelog](CHANGELOG.md)

## Why ELO

**Why we use it.** An agent only follows a rule it can see at the moment it acts. Pasting every rule into every prompt wastes context and buries the one that matters; leaving rules out means the agent repeats old mistakes. ELO sends the few rules and decisions that apply to *this* request, *this* project and *this* file, and lists the rest as short pointers. In a blind test on real prompts it beat full rule injection in 74% of the decided cases while sending about a quarter of the bytes (≈1.4 KB vs ≈5.4 KB per prompt).

**Technologies.**

| Part | Technology |
|---|---|
| Engine | Rust (single static binary, no runtime) |
| Memory | SQLite with FTS5 full-text search |
| Code map | tree-sitter (JavaScript/TypeScript callers, callees, imports) |
| Integration | MCP server (stdio) + hook adapters for Claude Code, Codex, Gemini CLI, Cursor, GitHub Copilot |
| Optional | static local embeddings (model2vec format), off by default |

**Why it makes coding safer.**

- **Nothing leaves your machine.** No network calls, no telemetry; memory, indexes and logs stay in `.elo/`. The only exception is the manual `elo update` command (from 0.2.0), which runs only when you type it.
- **The right rule before the risky step.** Rules anchored to files (migrations, deploy scripts, config) are delivered right before the agent reads or edits those files.
- **Guards on dangerous commands.** Configurable guards can record, ask for confirmation or block a tool call (e.g. force-push, publishing, deleting).
- **Only you write the laws.** Laws require `--as-user`; the agent cannot create or promote them on its own.
- **No stale or mixed context.** Superseded decisions stop being delivered, and another project's items arrive only as pointers.
- **Secrets stay out of logs.** Prompts are scrubbed of keys and tokens before anything is recorded.
- **Auditable.** `elo explain` and `elo stats` show why each item was sent and whether it was used.

## The problem

Coding agents lose rules and decisions between conversations. Loading every memory into every prompt consumes context and can bring back outdated or unrelated information. A useful memory system needs to deliver the right rule when the agent is about to need it, while keeping the rest available for lookup.

## What ELO does

- **Persistent memory:** stores `law`, `rule`, `decision`, `note` and `pitfall` items in SQLite, with keyword and path triggers, importance, history and scopes (`global`, `project:<name>`, `link:<name>`). Supports search, JSONL import, supersession, expiration and archival.
- **Prompt context:** selects current items using scope, triggers and scores, with byte budgets, layer quotas, session deduplication and pointers to additional context. Notes and decisions have separate limits and default to pointers unless stronger signals justify their full text.
- **Hooks at the point of work:** delivers prompt context, a session-start work summary and file-anchored rules before tool calls. Configurable guards can record matches or request confirmation/block a tool call.
- **Local usefulness scoreboard:** records deliveries, bytes, manual feedback and automatic evidence from subsequent tool inputs that touch matching files or identifiers. This is a usage signal, not proof that a rule was followed or a task succeeded.
- **Code map:** incrementally indexes JavaScript and TypeScript (`.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`) with tree-sitter. Tracks entities, imports, calls, JSX rendering, callbacks and re-exports; queries callers, callees, importers and transitive impact. Reference reports distinguish resolved and unresolved results and report stale files.
- **Work board:** tracks projects, tasks, dependencies, milestones and handoffs between conversations. Inactive open work can appear as deferred.
- **Recipes and maintenance:** stores reusable procedures as triggered notes; proposes memory consolidation and applies supported changes with a backup.
- **Evaluation:** runs a labelled local benchmark and replays local prompt logs with aggregate output.

## Principles

- **Local storage and processing.** ELO makes no network calls and has no telemetry. Memory, indexes, session state and logs stay under `.elo/`. Hook output is passed to Claude Code; that application's handling of context is separate from ELO's local processing.
- **Explicit context budgets.** Prompt selection uses `workspace.budget_bytes` in `elo.toml`; notes/decisions, file cues and session summaries also have limits. Bytes measure output size, not exact token consumption.
- **User-controlled laws.** Adding or importing laws requires `--as-user`; promotion accepts only user-originated items. Law writes validate size and budget constraints. Maintenance reports duplicate laws for manual review.
- **Inspectable behavior.** `elo explain`, `elo stats` and local logs expose selection and usage. Unresolved code references remain visible.

## Installation

ELO is a Rust program. Install Rust (which includes Cargo) from [rustup.rs](https://rustup.rs), then install ELO:

```sh
# straight from GitHub
cargo install --git https://github.com/anjoscd/elo-db-ai --locked

# or from a local checkout
cargo install --path . --locked
```

This puts `elo` in `~/.cargo/bin`, which rustup adds to your `PATH`. In the workspace you want to manage, run:

```sh
elo init
elo doctor
```

`elo init` creates `.elo/` and an `elo.toml` containing defaults and discovered projects, preserving an existing configuration. Run workspace commands from that workspace's root. Hooks locate the workspace from the event's `cwd`.

## Updating

ELO never updates automatically, runs updates from hooks, or triggers updates through the MCP server. To update the `elo` binary manually:

```sh
# check the current and latest versions without downloading
elo update --check

# install the latest release for your platform
elo update

# install a specific release
elo update --version 0.2.0
```

The command uses the system `curl` (with `--fail --location --proto =https --tlsv1.2`) to download the matching release archive and its `SHA256SUMS`, verifies the SHA-256 digest, extracts the `elo` binary, and atomically replaces the current binary while keeping the previous one as `elo.old`.

## Use in any IDE (MCP)

The server speaks MCP 2026-07-28 and legacy versions 2025-11-25, 2025-06-18, 2025-03-26, and 2024-11-05.

Run `elo mcp` to expose ELO over MCP stdio to compatible clients, independently of the model. First run `elo init` in the workspace and put `elo` on the client's `PATH`. The server finds the nearest ancestor containing `.elo/`; use `--root` when the client starts elsewhere. Merge the configuration below into existing settings and replace `/path/to/workspace` with the absolute workspace path.

**VS Code — `.vscode/mcp.json`**

```json
{
  "servers": {
    "elo": {
      "type": "stdio",
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

**Cursor — `.cursor/mcp.json`**

```json
{
  "mcpServers": {
    "elo": {
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

**Codex CLI — `~/.codex/config.toml`**

```toml
[mcp_servers.elo]
command = "elo"
args = ["mcp", "--root", "/path/to/workspace"]
```

**Gemini CLI — `.gemini/settings.json`**

```json
{
  "mcpServers": {
    "elo": {
      "command": "elo",
      "args": ["mcp", "--root", "/path/to/workspace"]
    }
  }
}
```

Tools: `elo_recall` searches memories; `elo_context` reads a scope or topic; `elo_add` saves text (default kind `note`, scope `global`, optional `keywords` and `paths`); `elo_explain` explains an item; `elo_map` queries the code map. Writes have agent origin, so laws are refused. Run `elo map sync` before map queries. Other MCP stdio clients use the same command and arguments; tool availability also depends on the client's MCP support.

### Claude Code hooks

Merge this `hooks` block into `~/.claude/settings.json`, preserving existing settings and hooks:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "elo hook user-prompt-submit --live" }] }
    ],
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "elo hook session-start --live" }] }
    ],
    "PreToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "elo hook pre-tool-use --live" }] }
    ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "elo hook post-tool-use --live" }] }
    ]
  }
}
```

| Event | Behavior |
|---|---|
| `UserPromptSubmit` | Selects memory and returns context; records the selection locally. |
| `SessionStart` | Resets delivery state and returns open handoffs and in-progress/blocked tasks for the active board project, when available. |
| `PreToolUse` | Checks guards and delivers items anchored to the file a tool is about to touch. |
| `PostToolUse` | Updates session state and automatic usage evidence; emits no context. |

Use `--shadow` instead of `--live` to process events without returning context or permission decisions. Without either flag, hooks do nothing. Hook errors are handled silently and exit successfully; guards are lexical checks, not a security sandbox.

Prompt logs in `.elo/shadow/log.jsonl` contain selected context and truncated prompt text. Credential-like prompts are redacted heuristically, without a guarantee of removing every secret. Keep `.elo/` private and out of version control.

## Usage

### Memory and retrieval

```sh
elo add rule --scope global --id rule-tests \
  --title "Test changes" --body "Run the relevant tests after changing behavior." \
  --triggers tests,testing
elo add law --scope global --id law-secrets --as-user \
  --title "Protect secrets" --body "Never commit credentials." \
  --triggers credentials,secrets
elo recall "tests"
elo context global tests --budget 3000
elo explain rule-tests
elo explain --last
elo good rule-tests
elo stats --by kind
```

`elo bad <id>` records negative feedback. `elo supersede <old> <new>` links a decision or note to its replacement; `elo recall <query> --history` includes inactive items. `elo forget <id>` archives an item and retains its record.

`elo add --triggers` sets keywords. To supply path triggers or `always`, import JSONL. For example, save this single line as `memory.jsonl`:

```json
{"id":"rule-auth","kind":"rule","scope":"global","title":"Authentication checks","body":"Validate permissions before changing account data.","keywords":["authentication"],"paths":["src/auth/*"]}
```

```sh
elo import memory.jsonl --dry-run
elo import memory.jsonl
```

Path matching supports exact paths and prefixes ending in `*`. File-like keywords such as `auth.ts` also anchor pre-tool context to matching path components. Configure project and link scopes in `elo.toml`; use `elo discover` to inspect detected projects and worktrees.

### Guards

Add a guard to `elo.toml`, for example:

```toml
[[guard]]
id = "confirm-push"
tool = "Bash"
field = "command"
all = ["git push"]
action = "ask"
message = "Confirm before pushing changes."
enforce = false
```

Matches are recorded in `.elo/shadow/guards.jsonl`. Set `enforce = true` with a live `PreToolUse` hook to return `ask` or `deny`, according to `action`. An optional `law` field references a memory item in the message. Guards are configured explicitly; ELO does not turn prose laws into checks automatically.

### Code map

Run in a workspace containing supported source files:

```sh
elo map sync
elo map query --contains authenticate
elo map query --calls authenticate
elo map query --callees authenticate
elo map query --imports src/auth.ts
elo map query --impact authenticate --depth 3 --json
```

Use `elo map sync --target src` to index a subtree. Queries read the existing index; rerun sync to refresh stale files. Resolution is static and may leave dynamic references unresolved.

### Projects, tasks and handoffs

```sh
elo project add demo --path . --name "Demo"
elo milestone add --project demo release "First release"
elo task add --project demo "Review authentication" --milestone release
elo task list --project demo
elo handoff new --project demo --title "Authentication review" \
  --body "Permission checks reviewed; add regression coverage next."
elo handoff list --project demo
```

Use returned task IDs with `elo task update <id> --status in_progress`, `elo task add --project demo "Follow-up" --blocked-by <id>` and `elo task done <id>`. Inspect and close handoffs with `elo handoff show <id>` and `elo handoff close <id>`. Board registration is separate from memory scope configuration in `elo.toml`.

### Recipes and maintenance

```sh
elo recipe add "Verify a change" \
  --step "Run the relevant tests." \
  --step "Review the diff for unintended changes." \
  --keywords tests,review
elo recipe list
elo maintain
elo maintain --apply
```

Recipes require at least two steps and are stored as global notes. The commands use the English option names; the old Portuguese names still work as hidden aliases.

`elo maintain` shows a plan. `--apply` first backs up the store, then attempts valid duplicate supersessions, marks expired items and removes old or orphaned automatic usage rows. Similar decisions/notes are suggestions for manual review; duplicate laws are reported without automatic consolidation.

## Evaluation

From the ELO checkout:

```sh
elo bench --split test
elo bench --split dev --json /tmp/elo-bench.json
```

The benchmark compares ELO's selector with no injection and a keyword-only reference selector, using labelled fixture events, a fixed event clock and session ordering. It reports:

- Forbidden-item and scope-violation rates.
- Relevance precision and required-item recall: full text, reachable through pointers, and within a session.
- Law coverage and superseded/expired item leaks.
- Output bytes at p50/p95 and abstention accuracy.

These are context-selection measurements, not coding-task success, token savings or latency results. [Evaluation methodology](docs/EVALUATION.md) also describes broader protocols and targets; those are not all implemented by `elo bench`.

Replay a workspace's local prompt log with:

```sh
elo bench --replay .elo/shadow/log.jsonl
```

Replay reads the current memory/configuration and logged events without changing the store. It reports aggregate bytes, full items per event, empty outputs and missing/invalid records. Truncated or redacted prompts and absent tool events limit fidelity; replay has no relevance labels.

## Research

[Research notes](docs/RESEARCH.md) collect the work informing ELO's design. [The specification](docs/SPEC.md) includes design requirements and deferred ideas; this README describes the implemented CLI behavior.

## Contributing

Open an issue with a reproducible example or submit a focused pull request. Keep private memories and workspace logs out of reports. For code changes, add relevant tests and run:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

For selector changes, compare benchmark output and explain the effect. Contributions must be original or compatible with the project's license.

## License and forks

[Functional Source License 1.1, ALv2 Future License (FSL-1.1-ALv2)](LICENSE). In short:

- **Free to use:** individuals and companies may use, modify and **fork** ELO, including at work and inside their own products and projects.
- **Not allowed without a license from the author:** offering ELO itself, or a product or service that **competes** with it (for example selling it, or hosting it as a paid service). For a commercial license, contact the author through [GitHub issues](https://github.com/anjoscd/elo-db-ai/issues).
- **Becomes Apache 2.0:** each version turns into Apache License 2.0 two years after its release.
- **Credit is required:** forks and changed copies must keep the [LICENSE](LICENSE) and [NOTICE](NOTICE) files, credit **ELO-DB-AI by anjoscd** (https://github.com/anjoscd/elo-db-ai) and state which files were changed.

## Other coding tools

| Tool | Hooks | MCP (`elo mcp`) | Exported rules |
| --- | --- | --- | --- |
| Claude Code | Yes (default format) | Yes | AGENTS.md |
| Codex | Yes | Yes | AGENTS.md |
| Gemini CLI | Yes | Yes | GEMINI.md |
| Cursor | Yes | Yes | .cursor/rules/*.mdc + AGENTS.md |
| Copilot CLI / agent | Yes | Yes | .github/copilot-instructions.md, .github/instructions/*.instructions.md + AGENTS.md |
| VS Code | — | Yes | Copilot format |
| Windsurf | — | Yes | .devin/rules/*.md + AGENTS.md |
| Cline | — | — | .clinerules/*.md + AGENTS.md |
| Zed / JetBrains | — | Yes | AGENTS.md (load manually if needed) |

```sh
elo setup codex                 # print file paths and configuration snippets
elo setup cursor --project     # project configuration; does not write files
elo hook BeforeAgent --live --format gemini
elo export --format todos --dry-run
elo export --format cursor --output . --scope project:api
```

`elo hook --format claude|codex|gemini|cursor|copilot` accepts each tool's native event names and the existing kebab-case names. Unknown events are silent. Gemini emits JSON only; its BeforeTool event cannot inject context. Cursor's beforeSubmitPrompt returns `{"continue":true}`; Copilot's userPromptSubmitted is silent. Both record the prompt and defer its context to postToolUse, with the work block shown once per project in the session. SessionStart can show that block earlier. Codex apply_patch and Gemini write_file/replace keep an existing code map current; Cursor uses afterFileEdit.

Guards remain in shadow mode unless configured with `enforce = true`. Enforced denials use each tool's protocol; Claude and Codex return exit code 0 with JSON `hookSpecificOutput.permissionDecision: "deny"`. Both preserve `ask`; other formats do not convert it into a denial.

`elo setup` prints hooks and MCP configuration, headed by the destination file path. Merge the snippets into existing configuration. Copilot hooks use `~/.copilot/hooks/elo.json`, or `.github/hooks/elo.json` with `--project`; JetBrains configuration is pasted in its MCP settings UI. The MCP snippets expect an ELO build providing `elo mcp`; setup does not implement or start the server.

`elo export --format agents|cursor|copilot|windsurf|windsurf-legacy|cline|gemini|todos` exports active, unexpired laws. The default is `agents`, the output folder defaults to the current directory, and `--scope` matches the exact stored scope. Always/keyword laws go in AGENTS.md (and GEMINI.md for Gemini); keyword conditions remain readable text. Path laws use native glob rules, including both `folder/migrations*` and `folder/migrations*/**` for ELO prefix paths. Native rule formats also emit AGENTS.md for keyword laws; configure your tool to load it when necessary.

Shared files preserve all text outside `<!-- elo:begin -->` / `<!-- elo:end -->`. Invalid or duplicate markers and symlinks stop the export without changing files. Files named `elo-*` in the selected rule directories are owned by ELO and may be regenerated or removed when obsolete; keep custom rules under other names. Cursor's `elo.mdc` also preserves text outside ELO markers. A scoped export preserves native files belonging to active laws in other scopes, but replaces the shared ELO block with the selected scope. Repeated exports are idempotent; `--dry-run` lists writes and removals without performing them.

Windsurf / Devin Desktop: `elo setup windsurf` prints both the current global MCP path `~/.config/devin/mcp_config.json` and the legacy path `~/.codeium/windsurf/mcp_config.json`, including with `--project`. `elo export --format windsurf` writes `.devin/rules`; use `--format windsurf-legacy` for `.windsurf/rules`. `todos` selects the current layout only. Existing `.clinerules` files receive a marked ELO block; otherwise export uses the supported `.clinerules/` directory. Symlink checks apply to the output folder and its descendants, not its system ancestors.
