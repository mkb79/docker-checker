mod checker;
mod docker;
mod error;
mod registry;
mod types;

pub(crate) static VERBOSITY: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// Emit a log line at the given verbosity level (always available, even in release builds).
/// Level 2 (-vv): high-level operational info. Level 3 (-vvv): full HTTP/auth detail.
macro_rules! log_v {
    ($level:expr, $($arg:tt)*) => {{
        if $crate::VERBOSITY.load(std::sync::atomic::Ordering::Relaxed) >= $level {
            eprintln!("\x1b[2m[debug] {}\x1b[0m", format_args!($($arg)*));
        }
    }};
}
pub(crate) use log_v;

use clap::builder::ArgAction;
use clap::Parser;
use colored::Colorize;
use std::sync::atomic::Ordering;

use crate::docker::DockerClient;
use crate::types::{CheckResult, ItemState};

#[derive(Parser)]
#[command(
    name = "docker-checker",
    about = "Checks running Docker containers for image updates",
    version
)]
struct Cli {
    /// Maximum number of concurrent registry checks
    #[arg(short, long, default_value_t = 10)]
    concurrent: usize,

    /// Increase output verbosity: -v shows up-to-date items, -vv adds operational info,
    /// -vvv adds full HTTP/auth detail
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Also check stopped containers and locally stored images without a container
    #[arg(short, long)]
    all: bool,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    VERBOSITY.store(cli.verbose, Ordering::Relaxed);

    if cli.no_color {
        colored::control::set_override(false);
    }

    let docker = match DockerClient::connect() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    let mut items = match docker.list_containers(cli.all).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
    };

    if cli.all {
        match docker.list_standalone_images().await {
            Ok(images) => {
                // Collect image_refs already covered by containers to avoid duplicate checks
                let container_refs: std::collections::HashSet<String> =
                    items.iter().map(|i| i.image_ref.clone()).collect();
                let mut seen = std::collections::HashSet::new();
                for img in images {
                    if container_refs.contains(&img.image_ref) {
                        continue;
                    }
                    if seen.insert(img.image_ref.clone()) {
                        items.push(img);
                    }
                }
            }
            Err(e) => eprintln!("{} listing standalone images: {}", "warning:".yellow(), e),
        }
    }

    let container_refs: std::collections::HashSet<String> = items
        .iter()
        .filter(|i| i.state != ItemState::Image)
        .map(|i| i.image_ref.clone())
        .collect();

    let local_digests = docker.local_digests().await;

    if items.is_empty() {
        println!("{}", "Nothing to check.".yellow());
        return;
    }

    let running = items.iter().filter(|i| i.state == ItemState::Running).count();
    let stopped = items.iter().filter(|i| i.state == ItemState::Stopped).count();
    let images = items.iter().filter(|i| i.state == ItemState::Image).count();

    if cli.all {
        println!(
            "Checking {} item{} ({} running, {} stopped, {} standalone images)...\n",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            running, stopped, images,
        );
    } else {
        println!(
            "Checking {} running container{}...\n",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
        );
    }

    let mut results = checker::check_all(items, local_digests, cli.concurrent).await;
    results.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));

    let mut up_to_date = 0usize;
    let mut updates = 0usize;
    let mut restart_only = 0usize;
    let mut local_only = 0usize;
    let mut errors = 0usize;

    for result in &results {
        match result {
            CheckResult::UpToDate { name, image, state } => {
                up_to_date += 1;
                if cli.verbose >= 1 {
                    println!(
                        "{} {}{} — {}",
                        "[✓]".green().bold(),
                        fmt_name(name, state),
                        format!(" ({})", image),
                        "up to date".green()
                    );
                }
            }
            CheckResult::UpdateAvailable { name, image, local, remote, info_url, already_pulled, state } => {
                updates += 1;
                if *already_pulled {
                    restart_only += 1;
                    println!(
                        "{} {}{} — {}",
                        "[↻]".cyan().bold(),
                        fmt_name(name, state),
                        format!(" ({})", image),
                        "UPDATE ALREADY PULLED — restart required".cyan().bold()
                    );
                } else {
                    println!(
                        "{} {}{} — {}",
                        "[!]".yellow().bold(),
                        fmt_name(name, state),
                        format!(" ({})", image),
                        "UPDATE AVAILABLE".yellow().bold()
                    );
                }
                println!("      local:  {}", local.dimmed());
                println!("      remote: {}", remote.dimmed());
                println!("      info:   {}", info_url.cyan());
            }
            CheckResult::LocalOnly { name, image, state } => {
                local_only += 1;
                if cli.verbose >= 1 {
                    println!(
                        "{} {}{} — {}",
                        "[?]".dimmed(),
                        fmt_name(name, state),
                        format!(" ({})", image),
                        "locally built, no registry digest".dimmed()
                    );
                }
            }
            CheckResult::Error { name, image, reason, state } => {
                errors += 1;
                println!(
                    "{} {}{} — {} {}",
                    "[✗]".red().bold(),
                    fmt_name(name, state),
                    format!(" ({})", image),
                    "error:".red(),
                    reason
                );
            }
        }
    }

    let total = up_to_date + updates + local_only + errors;
    println!(
        "\n{}: {} checked | {} update{} available ({} restart-only) | {} error{} | {} local-only",
        "Summary".bold(),
        total,
        updates,
        if updates == 1 { "" } else { "s" },
        restart_only,
        errors,
        if errors == 1 { "" } else { "s" },
        local_only
    );

    let pulled_digests: std::collections::HashSet<String> = results
        .iter()
        .filter_map(|r| match r {
            CheckResult::UpdateAvailable { already_pulled: true, remote, .. } => Some(remote.clone()),
            _ => None,
        })
        .collect();

    let pruneable = docker.pruneable_info(&container_refs, &pulled_digests).await;

    if pruneable.dangling_count > 0 || pruneable.unused_count > 0 {
        println!();
        if pruneable.dangling_count > 0 {
            println!(
                "  {} dangling image{} ({})",
                pruneable.dangling_count,
                if pruneable.dangling_count == 1 { "" } else { "s" },
                format!("{} virtual", format_bytes(pruneable.dangling_bytes)).dimmed(),
            );
        }
        if pruneable.unused_count > 0 {
            println!(
                "  {} unused image{} ({})",
                pruneable.unused_count,
                if pruneable.unused_count == 1 { "" } else { "s" },
                format!("{} virtual", format_bytes(pruneable.unused_bytes)).dimmed(),
            );
        }
    }

    if updates > 0 {
        std::process::exit(2);
    }
}

fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    const KB: u64 = 1_000;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn fmt_name(name: &str, state: &ItemState) -> String {
    match state {
        ItemState::Running => name.bold().to_string(),
        ItemState::Stopped => format!("{} {}", name.bold(), "[stopped]".dimmed()),
        ItemState::Image => format!("{} {}", name.bold(), "[image]".dimmed()),
    }
}
