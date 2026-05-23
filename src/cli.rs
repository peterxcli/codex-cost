use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::app::App;
use crate::cache::CacheStore;
use crate::pricing::Pricing;
use crate::ui::run_tui;
use crate::worker::{index_lock_path, IndexLock, IndexWorkerMode};

const APP_NAME: &str = "ccost";

#[derive(Debug, Default)]
pub(crate) struct Args {
    pub(crate) sessions: Option<PathBuf>,
    pub(crate) pricing: Option<PathBuf>,
    pub(crate) no_web_cost: bool,
    pub(crate) read_only_index: bool,
    pub(crate) force_index: bool,
}
pub fn run() -> Result<()> {
    let args = Args::parse()?;
    let sessions_dir = args.sessions.clone().unwrap_or_else(default_sessions_dir);
    let cache_dir = CacheStore::new(sessions_dir.clone())
        .cache_dir()
        .to_path_buf();
    let index_worker_mode = choose_index_worker_mode(&args, &cache_dir)?;
    let pricing = Pricing::load(args.pricing.as_deref())?;
    let app = App::new(sessions_dir, pricing, !args.no_web_cost, index_worker_mode)?;
    run_tui(app)
}

impl Args {
    pub(crate) fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Args::default();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("{} {}", APP_NAME, env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--sessions" => {
                    let Some(value) = args.next() else {
                        bail!("--sessions requires a path");
                    };
                    parsed.sessions = Some(expand_tilde(&value));
                }
                "--pricing" => {
                    let Some(value) = args.next() else {
                        bail!("--pricing requires a path");
                    };
                    parsed.pricing = Some(expand_tilde(&value));
                }
                "--no-web-cost" => {
                    parsed.no_web_cost = true;
                }
                "--read-only-index" => {
                    parsed.read_only_index = true;
                }
                "--force-index" => {
                    parsed.force_index = true;
                }
                other if other.starts_with("--sessions=") => {
                    parsed.sessions = Some(expand_tilde(&other["--sessions=".len()..]));
                }
                other if other.starts_with("--pricing=") => {
                    parsed.pricing = Some(expand_tilde(&other["--pricing=".len()..]));
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(parsed)
    }
}

pub(crate) fn print_help() {
    println!(
        "{} {}\n\nUSAGE:\n    {} [--sessions PATH] [--pricing PATH] [--no-web-cost]\n\nOPTIONS:\n    --sessions PATH      Codex or Claude Code session directory containing JSONL files\n    --pricing PATH       Optional pricing JSON override\n    --no-web-cost        Disable web-search call cost in estimates\n    --read-only-index    Open without writing the persisted search cache\n    --force-index        Write the cache even when index.lock is held; can corrupt cache data\n    -h, --help           Print help\n    -V, --version        Print version",
        APP_NAME,
        env!("CARGO_PKG_VERSION"),
        APP_NAME
    );
}

pub(crate) fn choose_index_worker_mode(args: &Args, cache_dir: &Path) -> Result<IndexWorkerMode> {
    if args.read_only_index && args.force_index {
        bail!("--read-only-index and --force-index cannot be used together");
    }
    if args.read_only_index {
        return Ok(IndexWorkerMode::ReadOnly);
    }
    if args.force_index {
        return Ok(IndexWorkerMode::Force);
    }

    match IndexLock::try_acquire(cache_dir)? {
        Some(lock) => Ok(IndexWorkerMode::UseLock(lock)),
        None => prompt_locked_index_mode(cache_dir),
    }
}

pub(crate) fn prompt_locked_index_mode(cache_dir: &Path) -> Result<IndexWorkerMode> {
    let lock_path = index_lock_path(cache_dir);
    eprintln!(
        "\nAnother {} instance is already holding the search index lock:\n  {}",
        APP_NAME,
        lock_path.display()
    );
    if let Ok(owner) = fs::read_to_string(&lock_path) {
        let owner = owner.trim();
        if !owner.is_empty() {
            eprintln!("Lock owner: {owner}");
        }
    }
    eprintln!(
        "Use read-only mode to browse the current cached index. Force writing only if you have verified that no other ccost instance is running; forcing while another writer is active can corrupt the persisted cache."
    );

    loop {
        eprint!("Choose [r]ead-only, [f]orce write, or [q]uit: ");
        io::stderr().flush()?;
        let mut choice = String::new();
        if io::stdin().read_line(&mut choice)? == 0 {
            eprintln!("No input received; opening read-only.");
            return Ok(IndexWorkerMode::ReadOnly);
        }
        match choice.trim().to_ascii_lowercase().as_str() {
            "" | "r" | "read-only" | "readonly" => return Ok(IndexWorkerMode::ReadOnly),
            "q" | "quit" => std::process::exit(0),
            "f" | "force" => {
                eprint!("Type FORCE to confirm cache writes without the lock: ");
                io::stderr().flush()?;
                let mut confirm = String::new();
                if io::stdin().read_line(&mut confirm)? == 0 {
                    return Ok(IndexWorkerMode::ReadOnly);
                }
                if confirm.trim() == "FORCE" {
                    return Ok(IndexWorkerMode::Force);
                }
                eprintln!("Confirmation did not match; choose again.");
            }
            _ => eprintln!("Please enter r, f, or q."),
        }
    }
}

pub(crate) fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return dirs_next::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

pub(crate) fn default_sessions_dir() -> PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(codex_home).join("sessions");
    }
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}
