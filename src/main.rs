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
mod setup;
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
    // No configuration file at all is not an error: it means first run.
    // The browser wizard writes one, and this process carries on with it.
    if !cli.check && !cli.config.exists() {
        setup::serve(&cli.config).await?;
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

    if cfg.feeds.iter().any(|f| f.mlat.is_some()) {
        supervise::spawn(
            supervise::ChildSpec {
                name: "mlatc".into(),
                cmd: cfg.programs.mlatc.clone(),
                args: cfg.mlatc_args(&cli.state_dir),
            },
            statuses.clone(),
            shutdown_rx.clone(),
        );
    }
    // The radio slot: rx when configured, readsb as its fallback (or alone).
    let spec_for = |name: &str, line: &str| -> Result<supervise::ChildSpec> {
        let mut parts = line.split_whitespace().map(String::from);
        let cmd = parts.next().with_context(|| format!("programs.{name} is empty"))?;
        let mut args: Vec<String> = parts.collect();
        args.extend(cfg.readsb_feed_args());
        Ok(supervise::ChildSpec {
            name: name.into(),
            cmd,
            args,
        })
    };
    let radio_health = supervise::RadioHealth::new();
    let radio_state: Option<supervise::RadioShared> = match (&cfg.programs.radio, &cfg.programs.readsb) {
        (Some(radio), fallback) => {
            let state = Arc::new(Mutex::new(supervise::RadioState {
                running: "radio",
                fallback: None,
                fell_back_unix: None,
            }));
            supervise::spawn_radio(
                spec_for("radio", radio)?,
                fallback.as_deref().map(|r| spec_for("readsb", r)).transpose()?,
                statuses.clone(),
                shutdown_rx.clone(),
                radio_health.clone(),
                state.clone(),
            );
            Some(state)
        }
        (None, Some(readsb)) => {
            supervise::spawn(spec_for("readsb", readsb)?, statuses.clone(), shutdown_rx.clone());
            None
        }
        (None, None) => None,
    };

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
        let health = radio_health.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            let mut last_minute = 0u64;
            let mut last_save = 0u64;
            let mut last_messages: Option<u64> = None;
            loop {
                tick.tick().await;
                let Some(snap) = model::read_snapshot(&dir, lat, lon) else {
                    *shared.snapshot.lock().unwrap() = None;
                    continue;
                };
                // The radio is alive when its message counter grows.
                if last_messages.is_none_or(|m| snap.messages_total > m) {
                    health.progressed();
                }
                last_messages = Some(snap.messages_total);
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

    let n_mlat = cfg.feeds.iter().filter(|f| f.mlat.is_some()).count();
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
            .filter_map(|f| {
                let mlat = f.mlat.as_ref()?;
                Some((f.name.clone(), status::stats_file_for(mlat, n_mlat)))
            })
            .collect(),
        readsb_configured: cfg.input.readsb_json.is_some(),
        shared: shared.clone(),
        radio: radio_state,
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
