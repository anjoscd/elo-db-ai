//! Replay only explicitly supplied JSONL. No hook invocation or shadow log writes.
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};

use elo_ctx::{config::Config, guard::validate};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Call {
    command: String,
    #[serde(default)]
    run_in_background: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let show = args.iter().any(|a| a == "--mostrar-comandos");
    args.retain(|a| a != "--mostrar-comandos");
    if args.len() != 2 {
        return Err("uso: replay_guards entrada.jsonl elo.toml [--mostrar-comandos]".into());
    }
    let config: Config = toml::from_str(&std::fs::read_to_string(&args[1])?)?;
    let problems = validate(&config.guards);
    if !problems.is_empty() {
        return Err(problems.join("; ").into());
    }
    let mut counts = vec![0usize; config.guards.len()];
    let mut examples = vec![Vec::new(); config.guards.len()];
    for (line_number, line) in BufReader::new(File::open(&args[0])?).lines().enumerate() {
        let line = line?;
        // Do not echo malformed input (which may contain private text).
        let call: Call = serde_json::from_str(&line)
            .map_err(|_| format!("JSONL inválido na linha {}", line_number + 1))?;
        let input = json!({"command": call.command, "run_in_background": call.run_in_background});
        for (i, guard) in config.guards.iter().enumerate() {
            if guard.matches("Bash", &input) {
                counts[i] += 1;
                if examples[i].len() < 5 {
                    let mut example = json!({"bytes": call.command.len()});
                    if show {
                        example["command"] =
                            json!(call.command.chars().take(100).collect::<String>());
                    }
                    examples[i].push(example);
                }
            }
        }
    }
    for (i, guard) in config.guards.iter().enumerate() {
        println!(
            "{}",
            json!({"guard": guard.id, "count": counts[i], "examples": examples[i]})
        );
    }
    Ok(())
}
