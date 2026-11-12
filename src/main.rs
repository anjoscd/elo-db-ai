//! Command line entry point for ELO.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use elo_ctx::{
    bench::run::{self, Split},
    doctor, init,
};

#[derive(Debug, Parser)]
#[command(
    name = "elo",
    version,
    about = "Local context and memory engine for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum ShadowCommand {
    /// Compare score bands with observed usage and subsequent owner feedback.
    Calibrate,
}

// The placeholder subcommands already parse their arguments, but only later
// tasks consume the values; keep the fields without a dead-code warning.
#[allow(dead_code)]
#[derive(Debug, Subcommand)]
enum RecipeCmd {
    /// Add a recipe: elo recipe add "title" --step "..." --step "..." --keywords a,b
    Add {
        title: String,
        #[arg(long = "step", alias = "passo", required = true)]
        steps: Vec<String>,
        #[arg(long = "keywords", alias = "palavras", value_delimiter = ',')]
        keywords: Vec<String>,
        #[arg(long)]
        as_user: bool,
    },
    /// List the active recipes.
    List,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect passive abstention telemetry.
    Shadow {
        #[command(subcommand)]
        command: ShadowCommand,
    },
    /// Serve MCP over stdin/stdout.
    Mcp {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Create `.elo/` and a default `elo.toml`.
    Init,
    /// Validate `elo.toml` and open the store.
    Doctor,
    /// Handle an agent hook event.
    Hook {
        /// Hook event name, e.g. `user-prompt-submit` or `post-tool-use`.
        event: String,
        /// Record what ELO would inject under `.elo/shadow/`, print nothing.
        #[arg(long)]
        shadow: bool,
        /// Record under `.elo/shadow/` and emit context where supported; overrides --shadow.
        #[arg(long)]
        live: bool,
        #[arg(
            long = "format",
            alias = "formato",
            value_enum,
            default_value = "claude"
        )]
        formato: elo_ctx::adapt::Format,
    },
    /// Print configuration snippets without writing files.
    Setup {
        #[arg(value_enum)]
        tool: elo_ctx::setup::Tool,
        #[arg(long = "project", alias = "projeto")]
        project: bool,
    },
    /// Export active laws to agent rule files.
    Export {
        #[arg(
            long = "format",
            alias = "formato",
            value_enum,
            default_value = "agents"
        )]
        format: elo_ctx::export::Format,
        #[arg(long = "output", alias = "saida")]
        output: Option<PathBuf>,
        #[arg(long = "scope", alias = "escopo")]
        scope: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the full text of the active items of a scope or topic (explicit; may cross scopes).
    Context {
        /// global, project:NAME (or NAME), link:NAME, or a topic.
        target: String,
        /// Extra words that rank the items by relevance.
        words: Vec<String>,
        /// Show exactly these item ids (comma separated).
        #[arg(long, value_delimiter = ',')]
        ids: Option<Vec<String>>,
        /// Byte budget; defaults to workspace.budget_bytes.
        #[arg(long)]
        budget: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Add an item to the store (SPEC §6.1).
    Add {
        /// law, rule, decision, note or pitfall (laws need --as-user).
        kind: String,
        /// global, project:NAME or link:NAME.
        #[arg(long)]
        scope: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: String,
        /// Comma separated keywords that trigger the item.
        #[arg(long, value_delimiter = ',')]
        triggers: Vec<String>,
        /// Importance in [0, 1].
        #[arg(long, default_value_t = 0.5)]
        importance: f64,
        /// Id of the decision or note this one replaces.
        #[arg(long)]
        supersedes: Option<String>,
        /// Expiry date YYYY-MM-DD (not for laws or rules).
        #[arg(long)]
        expires: Option<String>,
        /// Explicit id (default: kind-date-slug).
        #[arg(long)]
        id: Option<String>,
        /// Record origin = user instead of agent.
        #[arg(long)]
        as_user: bool,
    },
    /// Mark a decision or note as replaced by another existing item.
    Supersede { old: String, new: String },
    /// Turn an origin=user item into a law (logged in its evidence).
    Promote { id: String },
    /// Archive an item, leaving a tombstone.
    Forget { id: String },
    /// Search stored items.
    Recall {
        /// Free text query.
        query: String,
        /// Include superseded, expired and archived items.
        #[arg(long)]
        history: bool,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Explain an item (kind, scope, status, triggers) or the last hook event.
    Explain {
        /// Item id.
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        id: Option<String>,
        /// Explain the last event recorded in `.elo/shadow/log.jsonl`.
        #[arg(long)]
        last: bool,
        /// With --last, only events of this session.
        #[arg(long, requires = "last")]
        session: Option<String>,
    },
    /// Run the local benchmark.
    Bench {
        #[arg(long)]
        replay: Option<PathBuf>,
        #[arg(long, default_value = "bench/fixture/v0")]
        fixture: PathBuf,
        #[arg(long, value_enum, default_value = "test")]
        split: Split,
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Recipes: procedures that worked, delivered when their subject comes up.
    Recipe {
        #[command(subcommand)]
        cmd: RecipeCmd,
    },
    /// Memory housekeeping: propose (default) or apply (--apply) consolidation.
    Maintain {
        #[command(subcommand)]
        cmd: Option<MaintainCmd>,
        /// Apply the plan (a full copy of the store is made first).
        #[arg(long = "apply", alias = "aplicar")]
        apply: bool,
    },
    /// Show the local scoreboard (SPEC §6.5).
    Stats {
        /// Only records at or after this unix timestamp (seconds).
        #[arg(long)]
        since: Option<i64>,
        /// Breakdown by item kind or by scope.
        #[arg(long, value_enum, default_value = "kind")]
        by: By,
    },
    /// Import items from a JSONL file (SPEC §6.2: laws only with --as-user).
    Import {
        file: PathBuf,
        /// Trust the file as written by you: origin = user, laws allowed.
        #[arg(long)]
        as_user: bool,
        /// Report only; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show discovered projects and git worktrees (read-only).
    Discover,
    /// Update this binary to the latest release (manual only).
    Update {
        /// Only show current and latest versions; do not download anything.
        #[arg(long)]
        check: bool,
        /// Install a specific version instead of the latest one.
        #[arg(long = "version", value_name = "VERSION")]
        version: Option<String>,
    },
    /// Code map: sync the index and query entities (SPEC §7).
    Map {
        #[command(subcommand)]
        cmd: MapCommand,
    },
    /// Mark an item as useful.
    Good { id: String },
    /// Mark an item as not useful.
    Bad { id: String },
    /// Projects of the work board.
    #[command(subcommand)]
    Project(elo_ctx::work::cli::ProjectCmd),
    /// Tasks of the work board.
    #[command(subcommand)]
    Task(elo_ctx::work::cli::TaskCmd),
    /// Milestones of the work board.
    #[command(subcommand)]
    Milestone(elo_ctx::work::cli::MilestoneCmd),
    /// Handoffs for the next conversation.
    #[command(subcommand)]
    Handoff(elo_ctx::work::cli::HandoffCmd),
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum By {
    Kind,
    Scope,
}

#[derive(Debug, Subcommand)]
enum MaintainCmd {
    /// Review pending agent proposals.
    Review {
        /// Apply the listed proposal ids (comma separated). Laws are allowed when explicit.
        #[arg(long = "apply", value_name = "IDS", value_delimiter = ',')]
        apply: Option<Vec<String>>,
        /// Apply all pending proposals except those touching laws.
        #[arg(long = "apply-nonlaw")]
        apply_nonlaw: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MapCommand {
    /// Index the code map (incremental).
    Sync {
        /// Folder to index, relative to the repository root.
        #[arg(long)]
        target: Option<PathBuf>,
    },
    /// Query the code map.
    Query {
        /// Entities declared in this file.
        #[arg(long)]
        file: Option<String>,
        /// Entities whose name contains this text.
        #[arg(long)]
        contains: Option<String>,
        /// Who calls this entity (also lists unresolved references with the same name).
        #[arg(long)]
        calls: Option<String>,
        /// What this entity calls, including calls inside its callbacks.
        #[arg(long)]
        callees: Option<String>,
        /// Which files import this file.
        #[arg(long)]
        imports: Option<String>,
        /// Transitive callers of this entity.
        #[arg(long)]
        impact: Option<String>,
        /// Levels for --impact.
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Machine readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("{0}")]
    Replay(String),
    #[error(transparent)]
    Config(#[from] elo_ctx::config::ConfigError),
    #[error(transparent)]
    Bench(#[from] run::BenchError),
    #[error(transparent)]
    Init(#[from] init::InitError),
    #[error(transparent)]
    Store(#[from] elo_ctx::store::StoreError),
    #[error(transparent)]
    Import(#[from] elo_ctx::import::ImportError),
    #[error(transparent)]
    Write(#[from] elo_ctx::write::WriteError),
    #[error(transparent)]
    Recall(#[from] elo_ctx::recall::RecallError),
    #[error(transparent)]
    Context(#[from] elo_ctx::context::ContextError),
    #[error("{0}")]
    Usage(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Map(#[from] elo_ctx::map::MapError),
    #[error("cannot write to stdout: {0}")]
    Io(#[from] std::io::Error),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(code) => code,
        Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            if matches!(error, AppError::Internal(_)) {
                ExitCode::from(70)
            } else if matches!(
                error,
                AppError::Usage(_) | AppError::Context(elo_ctx::context::ContextError::Usage(_))
            ) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn execute(cli: Cli) -> Result<ExitCode, AppError> {
    match cli.command {
        Command::Mcp { root } => {
            let resolved = match root {
                Some(root) => root,
                None => resolve_root().map_err(|err| match err {
                    AppError::Usage(message) => {
                        AppError::Usage(format!("{message} or use `elo mcp --root <root>`"))
                    }
                    other => other,
                })?,
            };
            let root = resolved.as_path();
            // Validates the root without keeping a connection open during the session.
            drop(elo_ctx::context::open(root)?);
            elo_ctx::mcp::serve(root, std::io::stdin().lock(), std::io::stdout().lock())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Init => {
            let resolved = std::env::current_dir()?;
            let root = resolved.as_path();
            run_init(root)
        }
        Command::Doctor => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_doctor(root)
        }
        Command::Bench {
            replay,
            fixture,
            split,
            json,
        } => {
            let table = if let Some(log) = replay {
                elo_ctx::bench::replay::run(&log).map_err(AppError::Replay)?
            } else {
                run::run(&fixture, split, json.as_deref())?
            };
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(table.as_bytes())?;
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook {
            event,
            shadow,
            live,
            formato,
        } => adapted_hook(&event, shadow, live, formato),
        Command::Setup { tool, project } => {
            print!("{}", elo_ctx::setup::render(tool, project));
            Ok(ExitCode::SUCCESS)
        }
        Command::Export {
            format,
            output,
            scope,
            dry_run,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let report = elo_ctx::export::run(
                root,
                output.as_deref().unwrap_or(root),
                format,
                scope.as_deref(),
                dry_run,
            )
            .map_err(AppError::Usage)?;
            print!("{report}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Context {
            target,
            words,
            ids,
            budget,
            json,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_context(root, &target, &words, ids.as_deref(), budget, json)
        }
        Command::Add {
            kind,
            scope,
            title,
            body,
            triggers,
            importance,
            supersedes,
            expires,
            id,
            as_user,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_add(
                root,
                elo_ctx::write::NewEntry {
                    kind,
                    scope,
                    title,
                    body,
                    triggers: triggers
                        .into_iter()
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect(),
                    importance,
                    supersedes,
                    expires_at: expires.as_deref().map(parse_expiry).transpose()?,
                    id,
                    as_user,
                },
            )
        }
        Command::Supersede { old, new } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let mut store = elo_ctx::store::Store::open(root)?;
            match elo_ctx::write::supersede(&mut store, &old, &new)? {
                elo_ctx::write::Link::Apply => println!("{old} superseded by {new}"),
                elo_ctx::write::Link::AlreadyLinked => {
                    println!("warning: {old} was already superseded by {new}; nothing changed");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Promote { id } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let mut store = elo_ctx::store::Store::open(root)?;
            elo_ctx::write::promote(&mut store, &id)?;
            println!("{id} promoted to law");
            Ok(ExitCode::SUCCESS)
        }
        Command::Forget { id } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let mut store = elo_ctx::store::Store::open(root)?;
            elo_ctx::write::forget(&mut store, &id)?;
            println!("{id} archived (tombstone kept)");
            Ok(ExitCode::SUCCESS)
        }
        Command::Recall {
            query,
            history,
            scope,
            kind,
            limit,
            json,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let store = elo_ctx::store::Store::open_read_only(root)?;
            let hits = elo_ctx::recall::recall_now(
                &store,
                &elo_ctx::recall::Query {
                    text: query,
                    history,
                    scope,
                    kind,
                    limit,
                },
            )?;
            let mut out = std::io::stdout().lock();
            if json {
                writeln!(out, "{}", elo_ctx::recall::render_json(&hits))?;
            } else {
                out.write_all(elo_ctx::recall::render_text(&hits, history).as_bytes())?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Explain { id, last, session } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_explain(root, id.as_deref(), last, session)
        }
        Command::Shadow {
            command: ShadowCommand::Calibrate,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let store = elo_ctx::store::Store::open(root)?;
            let records = elo_ctx::shadow::read(&elo_ctx::store::data_dir(root));
            let buckets = elo_ctx::shadow::calibrate(&store.conn, &records)
                .map_err(elo_ctx::store::StoreError::from)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "top band | events | used | agent ignored")?;
            for (i, bucket) in buckets.iter().enumerate() {
                let percent = |n: usize| {
                    if bucket.events == 0 {
                        "n/a".into()
                    } else {
                        format!("{:.1}%", 100.0 * n as f64 / bucket.events as f64)
                    }
                };
                writeln!(
                    out,
                    "{:.1}–{:.1} | {} | {} | {}",
                    i as f64 / 10.0,
                    (i + 1) as f64 / 10.0,
                    bucket.events,
                    percent(bucket.used),
                    if bucket.reported == 0 {
                        "n/a".into()
                    } else {
                        format!(
                            "{:.1}%",
                            100.0 * bucket.ignored as f64 / bucket.reported as f64
                        )
                    }
                )?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Stats { since, by } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_stats(root, since, by)
        }
        Command::Recipe { cmd } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            let mut out = std::io::stdout().lock();
            match cmd {
                RecipeCmd::Add {
                    title,
                    steps,
                    keywords,
                    as_user,
                } => {
                    let mut store = elo_ctx::store::Store::open(root)?;
                    match elo_ctx::recipe::add_recipe(
                        &mut store, &title, &steps, &keywords, as_user,
                    ) {
                        Ok(id) => writeln!(out, "recipe saved: {id}")?,
                        Err(e) => return Err(AppError::Usage(e.to_string())),
                    }
                }
                RecipeCmd::List => {
                    let store = elo_ctx::store::Store::open_read_only(root)?;
                    for (id, title) in elo_ctx::recipe::list(&store)? {
                        writeln!(out, "{id}  {title}")?;
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Maintain { cmd, apply } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            match cmd.as_ref() {
                Some(MaintainCmd::Review {
                    apply: ids,
                    apply_nonlaw,
                }) => run_maintain_review(root, ids.as_deref(), *apply_nonlaw),
                _ => {
                    let mut store = elo_ctx::store::Store::open(root)?;
                    let now = elo_ctx::store::now_seconds();
                    let plan = elo_ctx::maintain::plan(&store, now)
                        .map_err(|e| AppError::Internal(e.to_string()))?;
                    let applied = if apply {
                        Some(
                            elo_ctx::maintain::apply(&mut store, &plan, now)
                                .map_err(|e| AppError::Internal(e.to_string()))?,
                        )
                    } else {
                        None
                    };
                    std::io::stdout()
                        .lock()
                        .write_all(elo_ctx::maintain::report(&plan, applied.as_ref()).as_bytes())?;
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        Command::Discover => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_discover(root)
        }
        Command::Update { check, version } => {
            let current_exe =
                std::env::current_exe().map_err(|e| AppError::Internal(e.to_string()))?;
            let current_version = env!("CARGO_PKG_VERSION");
            let version = version.as_ref().map(|v| {
                let v = v.trim();
                v.strip_prefix(['v', 'V']).unwrap_or(v).to_string()
            });
            if let Some(ref v) = version {
                if !elo_ctx::update::is_valid_version(v) {
                    return Err(AppError::Usage(format!("invalid version: {v}")));
                }
            }
            let opts = elo_ctx::update::UpdateOpts { check, version };
            let fetcher = elo_ctx::update::CurlFetcher;
            let result = elo_ctx::update::run(&opts, current_version, &current_exe, &fetcher)
                .map_err(|e| match e {
                    elo_ctx::update::Error::CurlNotFound => {
                        AppError::Usage("curl is required for `elo update`".into())
                    }
                    _ => AppError::Usage(e.to_string()),
                })?;
            let _ = result;
            Ok(ExitCode::SUCCESS)
        }
        Command::Map { cmd } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_map(root, cmd)
        }
        Command::Import {
            file,
            as_user,
            dry_run,
        } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_import(root, &file, as_user, dry_run)
        }
        Command::Good { id } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_feedback(root, &id, true)
        }
        Command::Bad { id } => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            run_feedback(root, &id, false)
        }
        Command::Project(c) => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            print_work(elo_ctx::work::cli::project(root, c))
        }
        Command::Task(c) => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            print_work(elo_ctx::work::cli::task(root, c))
        }
        Command::Milestone(c) => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            print_work(elo_ctx::work::cli::milestone(root, c))
        }
        Command::Handoff(c) => {
            let resolved = resolve_root()?;
            let root = resolved.as_path();
            print_work(elo_ctx::work::cli::handoff(root, c))
        }
    }
}

fn resolve_root() -> Result<PathBuf, AppError> {
    let cwd = std::env::current_dir()?;
    elo_ctx::hook::find_root(&cwd).ok_or_else(|| {
        AppError::Usage(format!(
            "No ELO root found from {}; run `elo init` in your workspace root",
            cwd.display()
        ))
    })
}

fn print_work(result: Result<String, elo_ctx::work::WorkError>) -> Result<ExitCode, AppError> {
    let text = result.map_err(|e| match e {
        elo_ctx::work::WorkError::Msg(m) => AppError::Usage(m),
        elo_ctx::work::WorkError::Db(d) => AppError::Internal(d.to_string()),
    })?;
    writeln!(std::io::stdout().lock(), "{text}")?;
    Ok(ExitCode::SUCCESS)
}

fn run_init(root: &Path) -> Result<ExitCode, AppError> {
    let outcome = init::init(root)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if outcome.config_written {
        writeln!(handle, "created {}", outcome.config_path.display())?;
    } else {
        writeln!(handle, "kept existing {}", outcome.config_path.display())?;
    }
    writeln!(handle, "initialized {}", outcome.data_dir.display())?;
    handle.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn run_doctor(root: &Path) -> Result<ExitCode, AppError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let healthy = doctor::run(root, &mut handle)?;
    handle.flush()?;
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `YYYY-MM-DD` as unix seconds at 00:00 UTC of that day.
fn parse_expiry(text: &str) -> Result<i64, AppError> {
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc().timestamp())
        .ok_or_else(|| AppError::Usage(format!("--expires {text:?} is not YYYY-MM-DD")))
}

fn run_add(root: &Path, entry: elo_ctx::write::NewEntry) -> Result<ExitCode, AppError> {
    let mut store = elo_ctx::store::Store::open(root)?;
    let kind = entry.kind.clone();
    let id = elo_ctx::write::add(&mut store, &entry)?;
    println!("added {kind} {id}");
    Ok(ExitCode::SUCCESS)
}

/// Placeholder for subcommands not shipped yet: never succeeds.
#[allow(dead_code)]
fn not_implemented() -> ExitCode {
    eprintln!("not implemented yet");
    ExitCode::from(2)
}

fn run_feedback(root: &Path, id: &str, good: bool) -> Result<ExitCode, AppError> {
    let store = elo_ctx::store::Store::open(root)?;
    store.record_feedback(id, good)?;
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "recorded {} for {id}",
        if good { "good" } else { "bad" }
    )?;
    Ok(ExitCode::SUCCESS)
}

fn run_stats(root: &Path, since: Option<i64>, by: By) -> Result<ExitCode, AppError> {
    use elo_ctx::stats::GroupBy;
    let store = elo_ctx::store::Store::open(root)?;
    let group = match by {
        By::Kind => GroupBy::Kind,
        By::Scope => GroupBy::Scope,
    };
    let st = store.stats(since, group)?;
    let config = current_config(root)?;
    let demoted = store.demoted_count(config.workspace.demote_bad_min)?;
    let pct = |v: Option<f64>| v.map_or("n/a".to_string(), |x| format!("{:.1}%", x * 100.0));
    let num = |v: Option<u64>| v.map_or("n/a".to_string(), |x| x.to_string());
    let mut out = std::io::stdout().lock();
    let records = elo_ctx::shadow::read(&elo_ctx::store::data_dir(root));
    let (abstain, delivered, skipped) =
        elo_ctx::shadow::percentages(&records, chrono::Utc::now().timestamp(), since);
    writeln!(out, "shadow: would abstain {} of prompts", pct(abstain))?;
    writeln!(
        out,
        "shadow: of those, {} delivered something full",
        pct(delivered)
    )?;
    if skipped > 0 {
        writeln!(
            out,
            "{skipped} shadow records skipped: invalid or future time"
        )?;
    }
    writeln!(out, "demoted            {}", demoted)?;
    let unused_count = if config.workspace.demote_unused_min == 0 {
        0
    } else {
        let unused = store.unused_windows(
            elo_ctx::store::now_seconds(),
            config.workspace.demote_unused_days,
        );
        let mut statement = store
            .conn
            .prepare("SELECT id, kind FROM items WHERE status='active'")
            .map_err(elo_ctx::store::StoreError::from)?;
        let count = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(elo_ctx::store::StoreError::from)?
            .filter_map(Result::ok)
            .filter(|(id, kind)| {
                use elo_ctx::bench::fixture::Kind;
                let kind = match kind.as_str() {
                    "rule" => Kind::Rule,
                    "decision" => Kind::Decision,
                    "note" => Kind::Note,
                    _ => return false,
                };
                unused
                    .get(id)
                    .is_some_and(|w| w.demoted(kind, config.workspace.demote_unused_min))
            })
            .count();
        count
    };
    writeln!(out, "demoted (unused)   {}", unused_count)?;
    writeln!(out, "events             {}", st.events)?;
    writeln!(out, "items full         {}", st.full)?;
    writeln!(out, "items as pointer   {}", st.pointers)?;
    writeln!(out, "laws delivered     {}", st.laws_full)?;
    writeln!(
        out,
        "bytes p50 / p95    {} / {}",
        num(st.bytes_p50),
        num(st.bytes_p95)
    )?;
    writeln!(
        out,
        "pointers/event p50 / p95  {} / {}",
        num(st.pointers_p50),
        num(st.pointers_p95)
    )?;
    writeln!(
        out,
        "repeated pointers  {} (across compaction/session-start boundaries this may over-count)",
        pct(st.repeated_pointers)
    )?;
    writeln!(out, "feedback good/bad  {} / {}", st.good, st.bad)?;
    writeln!(out, "used (good/full)   {}", pct(st.used_rate()))?;
    writeln!(out, "bad-report rate    {}", pct(st.bad_rate()))?;
    writeln!(
        out,
        "agent used/ignored {} / {}",
        st.agent_used, st.agent_ignored
    )?;
    let corroborated_pct = if st.agent_used > 0 {
        format!(
            "{:.1}%",
            st.agent_corroborated as f64 * 100.0 / st.agent_used as f64
        )
    } else {
        "n/a".to_string()
    };
    writeln!(out, "agent corroborated {corroborated_pct}")?;
    writeln!(out, "agent contradicted {}", st.agent_contradicted)?;
    let concord = if st.agent_agree + st.agent_disagree > 0 {
        format!(
            "{:.1}%",
            st.agent_agree as f64 * 100.0 / (st.agent_agree + st.agent_disagree) as f64
        )
    } else {
        "n/a".to_string()
    };
    writeln!(
        out,
        "agent concordance  {concord} ({} agree / {} disagree)",
        st.agent_agree, st.agent_disagree
    )?;
    if !st.groups.is_empty() {
        writeln!(
            out,
            "\n| group | full | pointers | good | bad | used (auto) | used÷delivered (auto) |\n|---|---:|---:|---:|---:|---:|---:|"
        )?;
        for (key, g) in &st.groups {
            writeln!(
                out,
                "| {key} | {} | {} | {} | {} | {} | {} |",
                g.full,
                g.pointers,
                g.good,
                g.bad,
                g.auto_used,
                pct((g.full > 0).then(|| g.auto_used as f64 / g.full as f64))
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_maintain_review(
    root: &Path,
    ids: Option<&[String]>,
    apply_nonlaw: bool,
) -> Result<ExitCode, AppError> {
    let mut store = elo_ctx::store::Store::open(root)?;
    // An id that does not parse is an operator typo: fail loudly instead of applying the rest.
    let parsed_ids: Option<Vec<i64>> = match ids {
        None => None,
        Some(list) => Some(
            list.iter()
                .map(|s| {
                    s.trim()
                        .parse::<i64>()
                        .map_err(|_| AppError::Usage(format!("invalid proposal id: {s:?}")))
                })
                .collect::<Result<_, _>>()?,
        ),
    };
    let proposals =
        elo_ctx::maintain::list_proposals(&store).map_err(|e| AppError::Internal(e.to_string()))?;
    if parsed_ids.is_none() && !apply_nonlaw {
        let mut out = std::io::stdout().lock();
        if proposals.is_empty() {
            writeln!(out, "no pending proposals")?;
        } else {
            for p in proposals {
                writeln!(
                    out,
                    "{} {} {} {}",
                    p.id,
                    p.op,
                    p.item_id.as_deref().unwrap_or(""),
                    p.payload
                )?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let now = elo_ctx::store::now_seconds();
    let applied = elo_ctx::maintain::apply_proposals(
        &mut store,
        parsed_ids.as_deref().unwrap_or(&[]),
        apply_nonlaw,
        now,
    )
    .map_err(|e| AppError::Internal(e.to_string()))?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "applied: {}", applied.applied.len())?;
    if !applied.skipped.is_empty() {
        writeln!(out, "skipped:")?;
        for (id, reason) in &applied.skipped {
            writeln!(out, "  {id}: {reason}")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_discover(root: &Path) -> Result<ExitCode, AppError> {
    let found = elo_ctx::discover::discover(root);
    let mut out = std::io::stdout().lock();
    writeln!(out, "projects ({}):", found.projects.len())?;
    for p in &found.projects {
        writeln!(out, "  {:<32} {}", p.name, p.root)?;
    }
    if !found.worktrees.is_empty() {
        writeln!(
            out,
            "git worktrees, copies of a project ({}):",
            found.worktrees.len()
        )?;
        for (wt, main) in &found.worktrees {
            writeln!(out, "  {wt} -> {}", main.display())?;
        }
    }
    if !found.nested.is_empty() {
        writeln!(
            out,
            "nested manifests inside projects: {}",
            found.nested.len()
        )?;
    }
    if found.single_root {
        writeln!(out, "single-project repository at the root")?;
    }
    Ok(ExitCode::SUCCESS)
}

fn run_map(root: &Path, cmd: MapCommand) -> Result<ExitCode, AppError> {
    match cmd {
        MapCommand::Sync { target } => {
            let report = elo_ctx::map::sync(root, target.as_deref())?;
            let mut out = std::io::stdout().lock();
            writeln!(
                out,
                "indexed {} · unchanged {} · removed {} · skipped {} · reanalyzed {}",
                report.indexed, report.unchanged, report.removed, report.skipped, report.resolved
            )?;
            Ok(ExitCode::SUCCESS)
        }
        MapCommand::Query {
            file,
            contains,
            calls,
            callees,
            imports,
            impact,
            depth,
            json,
        } => {
            let refs = if let Some(name) = calls {
                Some(elo_ctx::map::query_calls(root, &name)?)
            } else if let Some(name) = callees {
                Some(elo_ctx::map::query_callees(root, &name)?)
            } else if let Some(path) = imports {
                Some(elo_ctx::map::query_imports(root, &path)?)
            } else if let Some(name) = impact {
                Some(elo_ctx::map::query_impact(root, &name, depth)?)
            } else {
                None
            };
            if let Some(refs) = refs {
                let mut out = std::io::stdout().lock();
                elo_ctx::map::refs::print(&mut out, &refs, json)?;
                return Ok(ExitCode::SUCCESS);
            }
            let report = match (file, contains) {
                (Some(file), _) => elo_ctx::map::query_file(root, &file)?,
                (None, Some(name)) => elo_ctx::map::query_contains(root, &name)?,
                (None, None) => {
                    eprintln!("usage: elo map query --file <path> | --contains <name> | --calls <name> | --callees <name> | --imports <file> | --impact <name> [--depth N]");
                    return Ok(ExitCode::from(2));
                }
            };
            let mut out = std::io::stdout().lock();
            elo_ctx::map::query::print(&mut out, &report, json)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run_import(
    root: &Path,
    file: &Path,
    as_user: bool,
    dry_run: bool,
) -> Result<ExitCode, AppError> {
    let mut store = elo_ctx::store::Store::open(root)?;
    let r = elo_ctx::import::import(&mut store, file, as_user, dry_run)?;
    let mut out = std::io::stdout().lock();
    let mode = if dry_run {
        " (dry run, nothing written)"
    } else {
        ""
    };
    writeln!(out, "imported {}{mode}", r.imported)?;
    writeln!(
        out,
        "origin: {}",
        if as_user {
            "user (--as-user)"
        } else {
            "import"
        }
    )?;
    for (k, n) in &r.by_kind {
        writeln!(out, "  kind  {k:<10} {n}")?;
    }
    for (scope, n) in &r.by_scope {
        writeln!(out, "  scope {scope:<32} {n}")?;
    }
    if !r.downgraded.is_empty() {
        writeln!(out, "kept as rule instead of law ({}):", r.downgraded.len())?;
        for (id, why) in &r.downgraded {
            writeln!(out, "  {id}: {why}")?;
        }
    }
    if !r.protected.is_empty() {
        writeln!(
            out,
            "kept unchanged, written by the user (needs --as-user): {}",
            r.protected.join(", ")
        )?;
    }
    if !r.rejected.is_empty() {
        writeln!(out, "rejected ({}):", r.rejected.len())?;
        for (line, id, why) in &r.rejected {
            writeln!(out, "  line {line} {id}: {why}")?;
        }
    }
    Ok(if r.rejected.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Isolate hook failures while translating each protocol at the boundary.
fn adapted_hook(
    event: &str,
    shadow: bool,
    live: bool,
    format: elo_ctx::adapt::Format,
) -> Result<ExitCode, AppError> {
    use elo_ctx::adapt::{self, Format};
    use std::io::Read;
    if !shadow && !live {
        return Ok(ExitCode::SUCCESS);
    }
    let Some(event) = adapt::event(format, event) else {
        return Ok(ExitCode::SUCCESS);
    };
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| -> Option<ExitCode> {
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw).ok()?;
        if format == Format::Claude && event != "pre-tool-use" {
            return run_hook(event, shadow, live, &mut raw.as_bytes(), |text| {
                std::io::stdout().lock().write_all(text)
            })
            .ok();
        }
        let parsed = serde_json::from_str(&raw).ok()?;
        let mut value = if format == Format::Claude {
            parsed
        } else {
            adapt::input(format, parsed)?
        };
        value["defer_prompt"] =
            serde_json::json!(matches!(format, Format::Cursor | Format::Copilot));
        value["suppress_pre_context"] = serde_json::json!(matches!(
            format,
            Format::Gemini | Format::Cursor | Format::Copilot
        ));
        let stdin = value.to_string();
        if event == "pre-tool-use" {
            let guard = elo_ctx::guard::hook(&stdin)
                .and_then(|g| serde_json::from_str::<serde_json::Value>(&g).ok());
            let context = elo_ctx::hook::prepare(event, &stdin).ok().flatten();
            if !live {
                return Some(ExitCode::SUCCESS);
            }
            if let Some(mut guard) = guard {
                let decision = &guard["hookSpecificOutput"];
                let reason = decision["permissionDecisionReason"]
                    .as_str()
                    .unwrap_or("ELO: operation blocked");
                if decision["permissionDecision"] == "deny" {
                    if let Some(answer) = adapt::deny(format, reason) {
                        println!("{answer}");
                    } else {
                        eprintln!("{reason}");
                        return Some(ExitCode::from(2));
                    }
                } else if matches!(format, Format::Claude | Format::Codex) {
                    if let Some(context) = context {
                        guard["hookSpecificOutput"]["additionalContext"] =
                            serde_json::json!(context);
                    }
                    println!("{guard}");
                }
            } else if format == Format::Claude {
                if let Some(context) = context {
                    println!(
                        "{}",
                        serde_json::json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":context}})
                    );
                }
            } else if let Some(output) = adapt::context(format, event, context.as_deref()) {
                println!("{output}");
            }
            return Some(ExitCode::SUCCESS);
        }
        if event == "file-changed" {
            elo_ctx::events::file_changed(&stdin);
            return Some(ExitCode::SUCCESS);
        }
        if event == "post-tool-use" {
            let cwd = Path::new(value["cwd"].as_str()?);
            if let Some(root) = elo_ctx::hook::find_root(cwd) {
                for path in adapt::changed_paths(&value) {
                    elo_ctx::events::sync_changed_file(&root, cwd, Path::new(&path));
                }
            }
        }
        let context = elo_ctx::hook::prepare(event, &stdin).ok().flatten();
        if live {
            if let Some(output) = adapt::context(format, event, context.as_deref()) {
                println!("{output}");
            }
        }
        Some(ExitCode::SUCCESS)
    });
    std::panic::set_hook(previous);
    Ok(result.ok().flatten().unwrap_or(ExitCode::SUCCESS))
}

/// Hooks always exit 0; live output is released only after successful processing.
fn run_hook(
    event: &str,
    shadow: bool,
    live: bool,
    input: &mut impl std::io::Read,
    output: impl FnOnce(&[u8]) -> std::io::Result<()>,
) -> Result<ExitCode, AppError> {
    // Without either mode, hooks remain a silent no-op.
    if !shadow && !live {
        return Ok(ExitCode::SUCCESS);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hook_io(event, shadow, live, input, output);
    }));
    std::panic::set_hook(previous);
    Ok(ExitCode::SUCCESS)
}

fn hook_io(
    event: &str,
    shadow: bool,
    live: bool,
    input: &mut impl std::io::Read,
    output: impl FnOnce(&[u8]) -> std::io::Result<()>,
) {
    if !shadow && !live {
        return;
    }
    // Buffer the entire result, including the newline, before touching stdout.
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut stdin = String::new();
        input.read_to_string(&mut stdin).ok()?;
        if event == "file-changed" {
            elo_ctx::events::file_changed(&stdin);
            return None;
        }
        if event == "cwd-changed" {
            let block = elo_ctx::events::cwd_changed(&stdin)?;
            return live.then(|| format!("{block}\n"));
        }
        if event == "pre-tool-use" {
            // Guards (deny/ask) and the file-anchored rules answer in one JSON, only when live;
            // in shadow mode both are only recorded.
            let guard = elo_ctx::guard::hook(&stdin);
            let context = elo_ctx::hook::prepare(event, &stdin).ok().flatten();
            if !live || (guard.is_none() && context.is_none()) {
                return None;
            }
            let mut answer: serde_json::Value = guard
                .and_then(|g| serde_json::from_str(&g).ok())
                .unwrap_or_else(
                    || serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse"}}),
                );
            if let Some(context) = context {
                answer["hookSpecificOutput"]["additionalContext"] =
                    serde_json::Value::String(context);
            }
            return Some(format!("{answer}\n"));
        }
        let rendered = elo_ctx::hook::prepare(event, &stdin).ok().flatten()?;
        if !live || !matches!(event, "user-prompt-submit" | "session-start") || rendered.is_empty()
        {
            return None;
        }
        Some(format!("{rendered}\n"))
    }));
    if let Ok(Some(text)) = prepared {
        let _ = output(text.as_bytes());
    }
}

fn current_config(root: &Path) -> Result<elo_ctx::config::Config, AppError> {
    if root.join("elo.toml").is_file() {
        Ok(elo_ctx::config::Config::load(root)?)
    } else {
        Ok(elo_ctx::config::Config::default())
    }
}

fn run_explain(
    root: &Path,
    id: Option<&str>,
    last: bool,
    session: Option<String>,
) -> Result<ExitCode, AppError> {
    let mut out = std::io::stdout().lock();
    if last {
        out.write_all(elo_ctx::explain::last_report(root, session.as_deref()).as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    let id = id.ok_or_else(|| AppError::Usage("explain needs an item id or --last".into()))?;
    let store = elo_ctx::context::open(root)?;
    let config = current_config(root)?;
    let item = elo_ctx::live::load_item(&store, id)?;
    out.write_all(elo_ctx::explain::item_report(root, &config, &item).as_bytes())?;
    Ok(ExitCode::SUCCESS)
}

fn run_context(
    root: &Path,
    target: &str,
    words: &[String],
    ids: Option<&[String]>,
    budget: Option<u64>,
    json: bool,
) -> Result<ExitCode, AppError> {
    let config = current_config(root)?;
    let store = elo_ctx::context::open(root)?;
    let request = elo_ctx::context::Request {
        target,
        words,
        ids,
        budget: budget.unwrap_or(config.workspace.budget_bytes),
    };
    let outcome = elo_ctx::context::collect(&store, &config, &request)?;
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", elo_ctx::context::render_json(&outcome))?;
    } else {
        out.write_all(elo_ctx::context::render_text(&outcome, target).as_bytes())?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init::init(dir.path()).unwrap();
        let store = elo_ctx::store::Store::open(dir.path()).unwrap();
        store
            .insert_item(&elo_ctx::store::NewItem {
                id: "law",
                kind: "law",
                scope: "global",
                title: "Safety",
                body: "Always preserve user data.",
                origin: "user",
            })
            .unwrap();
        rusqlite::Connection::open(dir.path().join(".elo/elo.db"))
            .unwrap()
            .execute(
                r#"UPDATE items SET triggers = '{"keywords":[],"paths":[],"always":true}'"#,
                [],
            )
            .unwrap();
        dir
    }

    fn send(dir: &Path, event: &str, shadow: bool, live: bool) -> Vec<u8> {
        let input =
            serde_json::json!({"session_id":"test", "cwd":dir, "prompt":"help"}).to_string();
        let mut output = Vec::new();
        hook_io(event, shadow, live, &mut input.as_bytes(), |text| {
            output.write_all(text)
        });
        output
    }

    #[test]
    fn live_prints_exact_logged_block_and_overrides_shadow() {
        for shadow in [false, true] {
            let dir = workspace();
            let output = send(dir.path(), "user-prompt-submit", shadow, true);
            let log = fs::read_to_string(dir.path().join(".elo/shadow/log.jsonl")).unwrap();
            let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
            let rendered = record["rendered"].as_str().unwrap();
            assert!(rendered.contains("Always preserve user data."));
            assert_eq!(output, format!("{rendered}\n").as_bytes());
            assert!(dir.path().join(".elo/sessions/test.json").is_file());
        }
    }

    #[test]
    fn shadow_logs_without_printing_and_no_flags_do_nothing() {
        let dir = workspace();
        assert!(send(dir.path(), "user-prompt-submit", false, false).is_empty());
        assert!(!dir.path().join(".elo/sessions").exists());
        assert!(send(dir.path(), "user-prompt-submit", true, false).is_empty());
        assert!(dir.path().join(".elo/shadow/log.jsonl").is_file());
    }

    #[test]
    fn other_events_and_empty_selection_are_silent() {
        for event in ["post-tool-use", "session-start", "compaction"] {
            let dir = workspace();
            assert!(send(dir.path(), event, false, true).is_empty());
            assert!(dir.path().join(".elo/sessions/test.json").is_file());
        }
        let dir = tempfile::tempdir().unwrap();
        init::init(dir.path()).unwrap();
        assert!(send(dir.path(), "user-prompt-submit", false, true).is_empty());
    }

    struct BadReader(bool);
    impl std::io::Read for BadReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.0, "input panic");
            Err(std::io::Error::other("input error"))
        }
    }

    #[test]
    fn run_hook_restores_panic_hook_and_always_succeeds() {
        // Panic hooks are process-global: isolate this test from parallel tests.
        const CHILD: &str = "ELO_RUN_HOOK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "hook_tests::run_hook_restores_panic_hook_and_always_succeeds",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
            assert!(result.stderr.is_empty(), "{result:?}");
            return;
        }
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let checks = std::panic::catch_unwind(|| {
            for bytes in [b"not json".as_slice(), b"{}", b"\xff"] {
                let mut written = false;
                assert_eq!(
                    run_hook("user-prompt-submit", false, true, &mut &*bytes, |_| {
                        written = true;
                        Ok(())
                    })
                    .unwrap(),
                    ExitCode::SUCCESS
                );
                assert!(!written);
            }
            for panic in [false, true] {
                let mut written = false;
                assert_eq!(
                    run_hook(
                        "user-prompt-submit",
                        false,
                        true,
                        &mut BadReader(panic),
                        |_| {
                            written = true;
                            Ok(())
                        }
                    )
                    .unwrap(),
                    ExitCode::SUCCESS
                );
                assert!(!written);
            }
            // No-op must neither read input nor acquire the output sink.
            assert_eq!(
                run_hook(
                    "user-prompt-submit",
                    false,
                    false,
                    &mut BadReader(true),
                    |_| panic!("unexpected output")
                )
                .unwrap(),
                ExitCode::SUCCESS
            );
            for (shadow, live) in [(true, false), (false, true), (true, true)] {
                for failure in 0..3 {
                    let dir = workspace();
                    let input =
                        serde_json::json!({"session_id":"test", "cwd":dir.path(), "prompt":"help"})
                            .to_string();
                    let mut written = false;
                    assert_eq!(
                        run_hook(
                            "user-prompt-submit",
                            shadow,
                            live,
                            &mut input.as_bytes(),
                            |_| {
                                written = true;
                                match failure {
                                    0 => Ok(()),
                                    1 => Err(std::io::Error::other("output error")),
                                    _ => panic!("output panic"),
                                }
                            }
                        )
                        .unwrap(),
                        ExitCode::SUCCESS
                    );
                    assert_eq!(written, live);
                    assert!(dir.path().join(".elo/sessions/test.json").is_file());
                }
            }
            assert_eq!(calls.load(Ordering::SeqCst), 0, "hook was not suppressed");
            let _ = std::panic::catch_unwind(|| panic!("check restored hook"));
            assert_eq!(calls.load(Ordering::SeqCst), 1, "hook was not restored");
        });
        std::panic::set_hook(previous);
        assert!(checks.is_ok(), "entrypoint checks failed");
    }

    #[test]
    fn log_and_session_write_failures_do_not_release_context() {
        let dir = workspace();
        fs::create_dir_all(dir.path().join(".elo/shadow/log.jsonl")).unwrap();
        assert!(send(dir.path(), "user-prompt-submit", false, true).is_empty());

        let dir = workspace();
        // Load a valid existing session; only the final atomic write is blocked.
        send(dir.path(), "session-start", false, true);
        let session = dir.path().join(".elo/sessions/test.json");
        let before = fs::read_to_string(&session).unwrap();
        let seq = serde_json::from_str::<serde_json::Value>(&before).unwrap()["seq"]
            .as_u64()
            .unwrap();
        fs::create_dir(session.with_extension(format!("tmp-{}", std::process::id()))).unwrap();
        assert!(send(dir.path(), "user-prompt-submit", false, true).is_empty());
        let log = fs::read_to_string(dir.path().join(".elo/shadow/log.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["event_seq"], seq + 1);
        assert!(record["rendered"]
            .as_str()
            .unwrap()
            .contains("Always preserve user data."));
        assert_eq!(fs::read_to_string(session).unwrap(), before);
        assert!(
            !fs::read_to_string(dir.path().join(".elo/shadow/errors.log"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cli_accepts_both_flags() {
        let cli = Cli::try_parse_from(["elo", "hook", "user-prompt-submit", "--shadow", "--live"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Command::Hook {
                shadow: true,
                live: true,
                ..
            }
        ));
    }
}
