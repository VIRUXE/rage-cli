// `rage update`: ask GitHub whether a newer release exists, or download and
// install one in place of the running binary.

use anyhow::{Context, Result};
use std::io::{IsTerminal, Write};

use crate::update;
use crate::utils::json_string;

#[derive(clap::Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: UpdateCommand,
}

#[derive(clap::Subcommand)]
pub enum UpdateCommand {
    /// Ask GitHub whether a newer release exists
    Check {
        /// Print one JSON object instead of text
        #[arg(long)]
        json: bool,
    },

    /// Download the latest release and replace this binary
    Install {
        /// Skip the y/N confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Reinstall even when already on the latest release
        #[arg(long)]
        force: bool,
    },
}

pub fn run(args: &UpdateArgs) -> Result<()> {
    match &args.command {
        UpdateCommand::Check { json } => run_check(*json),
        UpdateCommand::Install { yes, force } => run_install(*yes, *force),
    }
}

fn run_check(json: bool) -> Result<()> {
    let (release, new_etag) = update::fetch_latest(None)?;
    let release = release.context("GitHub returned no releases for this repository")?;

    let latest_tuple = update::parse_version(&release.tag)
        .with_context(|| format!("could not parse the release tag '{}'", release.tag))?;
    let latest = format!("{}.{}.{}", latest_tuple.0, latest_tuple.1, latest_tuple.2);
    let current = update::current_version();
    let available = update::is_newer(&latest, current);
    let url = format!("https://github.com/{}/releases/tag/{}", update::repo_slug(), release.tag);

    // An explicit check resets the daily background-check clock too.
    update::record_check(&latest, new_etag.as_deref());

    if json {
        println!(
            "{{\"current\":{},\"latest\":{},\"update_available\":{},\"url\":{}}}",
            json_string(current), json_string(&latest), available, json_string(&url),
        );
    } else if available {
        println!("rpf {current} -> {latest} is available");
        println!("{url}");
        println!("Run `rage update install` to upgrade.");
    } else {
        println!("rpf {current} is the latest release");
    }
    Ok(())
}

fn run_install(yes: bool, force: bool) -> Result<()> {
    let Some(plan) = update::plan_update(force)? else {
        println!("rpf {} is already the latest release", update::current_version());
        return Ok(());
    };

    if !yes && std::io::stderr().is_terminal() {
        eprint!("Update rpf {} -> {}? [y/N] ", plan.current, plan.latest);
        let _ = std::io::stderr().flush();

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).context("reading the confirmation")?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Update cancelled.");
            return Ok(());
        }
    }

    eprintln!("downloading {}", plan.asset);
    update::install(&plan)?;
    println!("rage updated to {}", plan.latest);
    Ok(())
}
