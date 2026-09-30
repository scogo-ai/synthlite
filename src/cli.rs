use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::error::Exit;
use crate::gate::{self, GateOpts};
use crate::generate::{self, GenerateOpts};
use crate::push::{self, PushOpts};
use crate::USAGE;

// Top-level `synthlite`, `--help`, and `-h` print `usage.txt` in `run`
// before clap sees them, so clap's help flag only ever serves a subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "synthlite",
    disable_version_flag = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Send each prompt to the teacher once and append the replies to <OUT>/rows.jsonl (rerun to resume)
    Generate(GenerateArgs),
    /// Filter <OUT>/rows.jsonl into data/train.jsonl and data/validation.jsonl with a manifest and card
    Gate(GateArgs),
    /// Upload the gated dataset in <OUT> to a private Hugging Face dataset repo (needs HF_TOKEN)
    Push(PushArgs),
}

#[derive(Args, Debug)]
struct CommonArgs {
    /// Run directory: rows, state, and the gated dataset
    #[arg(long, value_name = "DIR", default_value = "./out")]
    out: PathBuf,
    /// Config file [default: ./synthlite.toml when it exists]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct GenerateArgs {
    /// Prompts file: .jsonl (one JSON object per line) or .txt (one prompt per line)
    #[arg(value_name = "INPUT")]
    input: PathBuf,
    #[command(flatten)]
    common: CommonArgs,
    /// Require an existing run in --out; never start a new one
    #[arg(long)]
    resume: bool,
    /// Check the input and config, print the plan, and exit without sending requests
    #[arg(long)]
    dry_run: bool,
    /// Send prompts that failed in an earlier run again
    #[arg(long)]
    retry_failed: bool,
    /// Retry failed prompts up to 3 more rounds until all finish, then say what still fails
    #[arg(long)]
    retry_until_finish: bool,
    /// Use keys parked by an earlier run (unauthorized, repeated 429s) again
    #[arg(long)]
    resume_parked_keys: bool,
    /// Decision-trace rows (see docs/decision-traces.md)
    #[arg(long)]
    detailed: bool,
    /// Stop after this many requests in this run
    #[arg(long, value_name = "N")]
    max_requests: Option<u64>,
    /// Stop after this many new rows in this run
    #[arg(long, value_name = "N")]
    max_rows: Option<u64>,
    /// Seconds between progress lines; 0 turns them off
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    progress_interval: u64,
}

#[derive(Args, Debug)]
struct GateArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Debug)]
struct PushArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Private Hugging Face dataset repo, as owner/name (required; no default)
    #[arg(long, value_name = "OWNER/NAME")]
    hf_repo: Option<String>,
    /// Upload and tag again even when the repo already has this dataset
    #[arg(long)]
    hf_republish: bool,
}

pub fn run() -> std::result::Result<(), Exit> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("synthlite {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.is_empty() || is_help(&args) {
        eprint!("{USAGE}");
        return Err(Exit {
            code: 2,
            message: String::new(),
        });
    }
    if args[0] != "generate" && args[0] != "gate" && args[0] != "push" {
        args.insert(0, "generate".into());
    }
    let mut full = vec!["synthlite".to_string()];
    full.append(&mut args);
    let cli = match Cli::try_parse_from(full) {
        Ok(cli) => cli,
        Err(err) if err.kind() == clap::error::ErrorKind::DisplayHelp => {
            // `synthlite <subcommand> --help`: clap prints it to stdout.
            err.print().map_err(|err| Exit {
                code: 1,
                message: err.to_string(),
            })?;
            return Ok(());
        }
        Err(err) => return Err(clap_exit(err)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| Exit {
            code: 1,
            message: err.to_string(),
        })?;
    runtime.block_on(dispatch(cli.cmd)).map_err(Exit::from)
}

async fn dispatch(cmd: Command) -> crate::error::Result<()> {
    match cmd {
        Command::Generate(args) => {
            generate::run_until_finish(GenerateOpts {
                input: args.input,
                out: args.common.out,
                config: args.common.config,
                resume: args.resume,
                dry_run: args.dry_run,
                retry_failed: args.retry_failed,
                retry_until_finish: args.retry_until_finish,
                resume_parked_keys: args.resume_parked_keys,
                detailed: args.detailed,
                max_requests: args.max_requests,
                max_rows: args.max_rows,
                progress_interval: (args.progress_interval > 0)
                    .then(|| std::time::Duration::from_secs(args.progress_interval)),
            })
            .await
        }
        Command::Gate(args) => gate::run(GateOpts {
            out: args.common.out,
            config: args.common.config,
            pretty_name_fallback: "synthlite".into(),
        }),
        Command::Push(args) => {
            push::run(PushOpts {
                out: args.common.out,
                config: args.common.config,
                hf_repo: args.hf_repo,
                hf_republish: args.hf_republish,
            })
            .await
        }
    }
}

fn is_help(args: &[String]) -> bool {
    args.len() == 1 && (args[0] == "--help" || args[0] == "-h")
}

/// Keeps every clap line before the usage trailer, so a missing argument is named.
fn clap_exit(err: clap::Error) -> Exit {
    let text = err.to_string();
    let message = text
        .lines()
        .take_while(|line| !line.starts_with("Usage:") && !line.starts_with("For more information"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let message = message.trim_start_matches("error: ").trim();
    Exit {
        code: 2,
        message: if message.is_empty() {
            "invalid arguments".into()
        } else {
            message.to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_error(args: &[&str]) -> Exit {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("expected a parse error"),
            Err(err) => clap_exit(err),
        }
    }

    #[test]
    fn missing_argument_is_named() {
        let exit = parse_error(&["synthlite", "generate", "--out", "x"]);
        assert_eq!(exit.code, 2);
        assert!(
            exit.message.contains("required arguments"),
            "{}",
            exit.message
        );
        assert!(exit.message.contains("INPUT"), "{}", exit.message);
        assert!(!exit.message.contains("Usage"), "{}", exit.message);
    }

    #[test]
    fn unknown_flag_is_named() {
        let exit = parse_error(&["synthlite", "gate", "--bogus"]);
        assert!(exit.message.contains("--bogus"), "{}", exit.message);
        assert!(!exit.message.starts_with("error:"), "{}", exit.message);
    }
}
