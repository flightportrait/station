//! stationd — the Station feeder runtime, v0.
//!
//! Reads one validated configuration file, supervises the receiver stack
//! (mlatc always; readsb when configured), and serves /status.json.
//! Refuses to start with a broken configuration and says why in
//! sentences. See station.example.toml.

mod config;
mod diagnose;
mod init;
mod model;
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
    /// Interactive setup: write the configuration by answering questions.
    #[arg(long)]
    init: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.init {
        return init::run(&cli.config);
    }
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

    let metrics_path = cli.state_dir.join("metrics.json");
    let shared = Arc::new(status::Shared {
        metrics: Mutex::new(model::Metrics::load(&metrics_path)),
        snapshot: Mutex::new(None),
    });
    // Sampler: read readsb's JSON every 15 s; fold a sample into the ring
    // once a minute; persist every 5 minutes.
    if let Some(dir) = cfg.input.readsb_json.clone() {
        let shared = shared.clone();
        let (lat, lon) = (cfg.station.lat, cfg.station.lon);
        let mpath = metrics_path.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            let mut last_minute = 0u64;
            let mut last_save = 0u64;
            loop {
                tick.tick().await;
                let Some(snap) = model::read_snapshot(&dir, lat, lon) else {
                    *shared.snapshot.lock().unwrap() = None;
                    continue;
                };
                let unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let minute = unix / 60;
                {
                    let mut m = shared.metrics.lock().unwrap();
                    if minute != last_minute {
                        last_minute = minute;
                        let hexes = snap.hexes.clone();
                        m.tick(unix, &snap, hexes.into_iter());
                    }
                    if unix.saturating_sub(last_save) >= 300 {
                        last_save = unix;
                        m.save(&mpath);
                    }
                }
                *shared.snapshot.lock().unwrap() = Some((std::time::Instant::now(), snap));
            }
        });
    }

    let n_feeds = cfg.feeds.len();
    let server = status::StatusServer {
        station_name: cfg.station.name.clone(),
        started_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        children: statuses,
        stats_dir: cli.state_dir.clone(),
        feeds: cfg
            .feeds
            .iter()
            .map(|f| (f.name.clone(), status::stats_file_for(&f.mlat, n_feeds)))
            .collect(),
        readsb_configured: cfg.input.readsb_json.is_some(),
        shared: shared.clone(),
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
    shared.metrics.lock().unwrap().save(&metrics_path);
    let _ = shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    Ok(())
}
