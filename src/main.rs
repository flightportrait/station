//! stationd — the Station feeder runtime, v0.
//!
//! Reads one validated configuration file, supervises the receiver stack
//! (mlatc always; readsb when configured), and serves /status.json.
//! Refuses to start with a broken configuration and says why in
//! sentences. See station.example.toml.

mod config;
mod status;
mod supervise;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "stationd", version, about)]
struct Cli {
    /// Configuration file.
    #[arg(long, default_value = "station.toml")]
    config: std::path::PathBuf,
    /// Directory for runtime state (MLAT stats files).
    #[arg(long, default_value = "state")]
    state_dir: std::path::PathBuf,
    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.config)
        .with_context(|| format!("cannot read {}", cli.config.display()))?;
    let cfg: config::Config = toml::from_str(&text)
        .with_context(|| format!("{} does not parse", cli.config.display()))?;
    let problems = cfg.problems();
    if !problems.is_empty() {
        eprintln!("stationd: {} refuses to start:", cli.config.display());
        for p in &problems {
            eprintln!("  - {p}");
        }
        std::process::exit(2);
    }
    if cli.check {
        println!("stationd: {} is valid.", cli.config.display());
        return Ok(());
    }
    std::fs::create_dir_all(&cli.state_dir)
        .with_context(|| format!("cannot create {}", cli.state_dir.display()))?;

    let statuses: supervise::StatusMap = Arc::new(Mutex::new(Default::default()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    supervise::spawn(
        supervise::ChildSpec {
            name: "mlatc".into(),
            cmd: cfg.programs.mlatc.clone(),
            args: cfg.mlatc_args(&cli.state_dir),
        },
        statuses.clone(),
        shutdown_rx.clone(),
    );
    if let Some(readsb) = &cfg.programs.readsb {
        let mut parts = readsb.split_whitespace().map(String::from);
        let cmd = parts.next().context("programs.readsb is empty")?;
        supervise::spawn(
            supervise::ChildSpec {
                name: "readsb".into(),
                cmd,
                args: parts.collect(),
            },
            statuses.clone(),
            shutdown_rx.clone(),
        );
    }

    let server = status::StatusServer {
        station_name: cfg.station.name.clone(),
        started_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        children: statuses,
        stats_dir: cli.state_dir.clone(),
        feeds: cfg.feeds.iter().map(|f| f.name.clone()).collect(),
    };
    let listen = cfg.status.listen.clone();
    tokio::spawn(server.run(listen));

    // SIGTERM is how systemd and docker stop a service; treating only
    // Ctrl-C as shutdown leaks the children (found in the S1 drill).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    println!("stationd: shutting down");
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(())
}
