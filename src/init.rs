//! Interactive setup: questions in, a valid station.toml out.
//!
//! The point is that a beginner never opens an editor or hunts for an
//! aggregator's hostname. Every answer is validated on the spot with
//! the same rules the daemon enforces at startup. On a terminal the
//! questions use arrow-key menus (dialoguer); piped input falls back
//! to plain numbered prompts so scripts and tests keep working.

use std::io::{BufRead, IsTerminal, Write};

pub struct Aggregator {
    pub name: &'static str,
    pub adsb: Option<&'static str>,
    pub mlat: Option<&'static str>,
    pub note: &'static str,
    /// What the feeder gets back — the other half of the pitch.
    pub gives: &'static str,
    /// The aggregator's own site, for the setup page's card link.
    pub url: &'static str,
    /// How a station key works for this network, in one plain clause.
    pub key_hint: &'static str,
}

/// Aggregators on offer. Feeding is non-exclusive; pick any.
pub const CATALOG: &[Aggregator] = &[
    Aggregator {
        name: "FlightPortrait",
        adsb: Some("feed.flightportrait.com:30004"),
        mlat: None,
        note: "ours — powers the art frames; MLAT when our solver goes public",
        gives: "your sky becomes daily posters on FlightPortrait frames",
        url: "https://flightportrait.com",
        key_hint: "arrives with a FlightPortrait frame; empty is fine without one",
    },
    Aggregator {
        name: "adsb.lol",
        adsb: Some("in.adsb.lol:30004"),
        mlat: Some("in.adsb.lol:31090"),
        note: "open data, no account needed",
        gives: "a public map and a free API of what you feed",
        url: "https://adsb.lol",
        key_hint:
            "optional — no account exists; generate one and keep it, their site treats it as yours",
    },
    Aggregator {
        name: "adsb.fi",
        adsb: Some("feed.adsb.fi:30004"),
        mlat: Some("feed.adsb.fi:31090"),
        note: "open data, no account needed",
        gives: "a public map and a free API of what you feed",
        url: "https://adsb.fi",
        key_hint:
            "optional — no account exists; generate one and keep it to mark the station as yours",
    },
    Aggregator {
        name: "adsb.win",
        adsb: Some("feed.adsb.win:30004"),
        mlat: Some("mlat.adsb.win:31090"),
        note: "open data, no account needed",
        gives: "a public map; UK-centred community",
        url: "https://adsb.win",
        key_hint: "only if the operator gave you one",
    },
];

pub struct FeedChoice {
    pub name: String,
    pub adsb: Option<String>,
    pub mlat: Option<String>,
    pub uuid: String,
}

pub struct Answers {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub alt: String,
    pub input_beast: String,
    pub readsb_json: Option<String>,
    /// readsb command line: the radio when `radio_prog` is None, else the fallback.
    pub readsb_prog: Option<String>,
    /// The Station radio (rx) command line, when rx is installed.
    pub radio_prog: Option<String>,
    /// The station key every feed carries.
    pub station_uuid: String,
    pub feeds: Vec<FeedChoice>,
    /// Status listen address to write; None keeps the config default.
    pub listen: Option<String>,
}

pub fn run(config_path: &std::path::Path, radio: Option<&std::path::Path>) -> anyhow::Result<()> {
    let fancy = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let rx = find_rx(radio);
    let answers = if fancy {
        gather_fancy(rx.as_deref())?
    } else {
        gather_plain(rx.as_deref())?
    };
    let Some(a) = answers else {
        println!("Nothing written.");
        return Ok(());
    };
    write_config(config_path, &a)
}

fn parse_position(v: &str) -> Option<(f64, f64)> {
    let parts: Vec<f64> = v
        .split([',', ' '])
        .filter(|p| !p.trim().is_empty())
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    match parts.as_slice() {
        [la, lo] if (-90.0..=90.0).contains(la) && (-180.0..=180.0).contains(lo) => {
            Some((*la, *lo))
        }
        _ => None,
    }
}

fn default_station_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The radio command line for a local dongle. readsb and rx take the same
/// flags, so one shape serves both; port 30004 takes MLAT results back in.
pub fn readsb_program(readsb_path: &str, json_dir: &str, lat: f64, lon: f64) -> String {
    format!(
        "{readsb_path} --device-type rtlsdr --gain auto --quiet \
         --net --net-bo-port 30005 --net-bi-port 30004 --write-json {json_dir} \
         --write-json-every 1 --lat {lat} --lon {lon}"
    )
}

/// Where MLAT results go: the local radio's Beast input.
pub const RESULTS_LOCAL: &str = "127.0.0.1:30004";

/// The Station radio, if installed: the path given, else `~/station/rx`,
/// else `rx` beside this stationd binary. Only an executable file counts.
pub fn find_rx(explicit: Option<&std::path::Path>) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p.to_path_buf());
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::Path::new(&home).join("station").join("rx"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("rx"));
        }
    }
    candidates.into_iter().find_map(|p| {
        let m = std::fs::metadata(&p).ok()?;
        (m.is_file() && m.permissions().mode() & 0o111 != 0).then(|| p.display().to_string())
    })
}

/// The station key: one UUID for every feed, so each network sees one
/// station. From the kernel's generator, else our own.
pub fn station_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| s.len() == 36)
        .unwrap_or_else(pseudo_uuid)
}

/// Uniform per-feed post-processing: drop ADS-B destinations the station
/// cannot serve (no local readsb), saying what to do instead; drop feeds
/// left with nothing. Returns the surviving feeds and the explanations.
pub fn resolve_feeds(
    mut feeds: Vec<FeedChoice>,
    local_readsb: bool,
    station_uuid: &str,
) -> (Vec<FeedChoice>, Vec<String>) {
    let mut notes = Vec::new();
    // One station, one key: every feed without a key of its own gets the
    // station's, so each network can tell this station is one station.
    for f in &mut feeds {
        if f.uuid.trim().is_empty() {
            f.uuid = station_uuid.to_string();
        }
    }
    if !local_readsb {
        for f in &mut feeds {
            if let Some(dest) = f.adsb.take() {
                let (host, port) = dest.rsplit_once(':').unwrap_or((dest.as_str(), "30004"));
                notes.push(format!(
                    "{} takes ADS-B from readsb, and yours runs elsewhere. Add this \
                     to that readsb instead:  --net-connector \
                     {host},{port},beast_reduce_plus_out",
                    f.name
                ));
            }
        }
    }
    feeds.retain(|f| {
        if f.adsb.is_none() && f.mlat.is_none() {
            notes.push(format!(
                "{} skipped: nothing this station could send it.",
                f.name
            ));
            false
        } else {
            true
        }
    });
    // Mixing keyed and unkeyed MLAT feeds is a config error; fill quietly.
    if feeds.iter().any(|f| f.mlat.is_some() && !f.uuid.is_empty()) {
        for f in &mut feeds {
            if f.mlat.is_some() && f.uuid.is_empty() {
                f.uuid = pseudo_uuid();
            }
        }
    }
    (feeds, notes)
}

// --- terminal flow ------------------------------------------------------

fn gather_fancy(rx: Option<&str>) -> anyhow::Result<Option<Answers>> {
    use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect, Select};
    let th = ColorfulTheme::default();
    let station_uuid = station_uuid();

    println!("Station setup — a few questions, then a working station.\n");

    let name: String = Input::with_theme(&th)
        .with_prompt("Station name (MLAT servers identify you by this)")
        .default(default_station_name())
        .validate_with(|s: &String| {
            if s.trim().is_empty() {
                Err("a name, any name")
            } else {
                Ok(())
            }
        })
        .interact_text()?;

    println!();
    println!("The antenna's position. Right-click your house in Google Maps;");
    println!("the first menu entry is the two numbers — paste them here as one.");
    println!("MLAT places other people's aircraft with them, so closer is better.");
    let pos: String = Input::with_theme(&th)
        .with_prompt("Position (lat, lon)")
        .validate_with(|s: &String| match parse_position(s) {
            Some(_) => Ok(()),
            None => Err("two numbers, like: 48.85824, 2.29444"),
        })
        .interact_text()?;
    let (lat, lon) = parse_position(&pos).expect("validated");

    let alt: String = Input::with_theme(&th)
        .with_prompt("Antenna altitude (\"65m\" or \"213ft\")")
        .validate_with(|s: &String| match crate::config::parse_alt(s) {
            Some(a) if (-1000.0..=10000.0).contains(&a) => Ok(()),
            _ => Err("a number with m or ft, like 12m"),
        })
        .interact_text()?;

    println!();
    let sdr = detect_rtlsdr();
    let sdr_label = if sdr {
        "This machine's SDR dongle — one is plugged in right now"
    } else {
        "This machine's SDR dongle (stationd will run readsb)"
    };
    let input_choice = Select::with_theme(&th)
        .with_prompt("Where do the radio signals come from?")
        .items(&[
            "A readsb already running on another machine (host:port of its data stream)",
            sdr_label,
        ])
        .default(if sdr { 1 } else { 0 })
        .interact()?;
    let (input_beast, readsb_json, readsb_prog, radio_prog) = if input_choice == 1 {
        let json_dir = "state/readsb".to_string();
        let radio_prog = rx.map(|p| readsb_program(p, &json_dir, lat, lon));
        if let Some(p) = rx {
            println!("  The Station radio is installed at {p}; readsb stays as its fallback.");
        }
        let readsb: String = Input::with_theme(&th)
            .with_prompt(if rx.is_some() {
                "Path to the readsb binary (Enter = none, no fallback)"
            } else {
                "Path to the readsb binary"
            })
            .default(if rx.is_some() { String::new() } else { "/usr/local/bin/readsb".into() })
            .allow_empty(rx.is_some())
            .interact_text()?;
        let readsb_prog = Some(readsb.trim())
            .filter(|s| !s.is_empty())
            .map(|p| readsb_program(p, &json_dir, lat, lon));
        (
            "127.0.0.1:30005".to_string(),
            Some(json_dir.clone()),
            readsb_prog,
            radio_prog,
        )
    } else {
        let src: String = Input::with_theme(&th)
            .with_prompt("Beast source (host:port)")
            .default("127.0.0.1:30005".into())
            .validate_with(|s: &String| {
                if s.contains(':') {
                    Ok(())
                } else {
                    Err("host:port, like 192.168.1.10:30005")
                }
            })
            .interact_text()?;
        (src, None, None, None)
    };

    println!();
    println!("Aggregators. Feeding is non-exclusive — space toggles, Enter confirms.");
    let items: Vec<String> = CATALOG
        .iter()
        .map(|a| {
            let what = match (a.adsb, a.mlat) {
                (Some(_), Some(_)) => "ADS-B + MLAT",
                (Some(_), None) => "ADS-B",
                _ => "MLAT",
            };
            format!("{}  ({what}) — {}", a.name, a.note)
        })
        .collect();
    let picked = loop {
        let p = MultiSelect::with_theme(&th)
            .with_prompt("Feed")
            .items(&items)
            .defaults(&vec![true; items.len()])
            .interact()?;
        if !p.is_empty() {
            break p;
        }
        println!("  At least one, or the station tells no one.");
    };

    let mut feeds: Vec<FeedChoice> = Vec::new();
    for i in picked {
        let a = &CATALOG[i];
        let uuid: String = Input::with_theme(&th)
            .with_prompt(format!(
                "Station key for {}, if they gave you one (Enter = none)",
                a.name
            ))
            .allow_empty(true)
            .interact_text()?;
        feeds.push(FeedChoice {
            name: a.name.to_string(),
            adsb: a.adsb.map(String::from),
            mlat: a.mlat.map(String::from),
            uuid: uuid.trim().to_string(),
        });
    }
    while Confirm::with_theme(&th)
        .with_prompt("Add an aggregator not on the list?")
        .default(false)
        .interact()?
    {
        if let Some(f) = ask_custom_feed(&th)? {
            feeds.push(f);
        }
    }

    let (feeds, notes) = resolve_feeds(
        feeds,
        readsb_prog.is_some() || radio_prog.is_some(),
        &station_uuid,
    );
    for n in &notes {
        println!("  note: {n}");
    }
    if feeds.is_empty() {
        println!("No usable feeds; nothing to set up.");
        return Ok(None);
    }

    let a = Answers {
        name,
        lat,
        lon,
        alt,
        input_beast,
        readsb_json,
        readsb_prog,
        radio_prog,
        station_uuid,
        feeds,
        listen: None,
    };
    print_summary(&a);
    if !Confirm::with_theme(&th)
        .with_prompt("Write this configuration")
        .default(true)
        .interact()?
    {
        return Ok(None);
    }
    Ok(Some(a))
}

fn ask_custom_feed(th: &dialoguer::theme::ColorfulTheme) -> anyhow::Result<Option<FeedChoice>> {
    use dialoguer::Input;
    let name: String = Input::with_theme(th)
        .with_prompt("  Feed name")
        .interact_text()?;
    let mlat: String = Input::with_theme(th)
        .with_prompt("  MLAT server (host:port, Enter = none)")
        .allow_empty(true)
        .validate_with(|s: &String| {
            if s.trim().is_empty() || s.contains(':') {
                Ok(())
            } else {
                Err("host:port, or nothing")
            }
        })
        .interact_text()?;
    let adsb: String = Input::with_theme(th)
        .with_prompt("  ADS-B / Beast destination (host:port, Enter = none)")
        .allow_empty(true)
        .validate_with(|s: &String| {
            if s.trim().is_empty() || s.contains(':') {
                Ok(())
            } else {
                Err("host:port, or nothing")
            }
        })
        .interact_text()?;
    if mlat.trim().is_empty() && adsb.trim().is_empty() {
        println!("  Neither an MLAT server nor an ADS-B destination; skipped.");
        return Ok(None);
    }
    let uuid: String = Input::with_theme(th)
        .with_prompt(format!(
            "  Station key for {name}, if they gave you one (Enter = none)"
        ))
        .allow_empty(true)
        .interact_text()?;
    Ok(Some(FeedChoice {
        name,
        adsb: Some(adsb.trim().to_string()).filter(|s| !s.is_empty()),
        mlat: Some(mlat.trim().to_string()).filter(|s| !s.is_empty()),
        uuid: uuid.trim().to_string(),
    }))
}

// --- piped flow ---------------------------------------------------------

fn gather_plain(rx: Option<&str>) -> anyhow::Result<Option<Answers>> {
    let station_uuid = station_uuid();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ask = |prompt: &str, default: &str| -> String {
        if default.is_empty() {
            print!("{prompt}: ");
        } else {
            print!("{prompt} [{default}]: ");
        }
        std::io::stdout().flush().ok();
        match lines.next() {
            Some(Ok(l)) if !l.trim().is_empty() => l.trim().to_string(),
            _ => default.to_string(),
        }
    };

    println!("Station setup. Enter accepts the [default].\n");

    let name = ask(
        "Station name (MLAT servers identify you by this)",
        &default_station_name(),
    );

    println!("\nThe antenna's position. Right-click your house in Google Maps;");
    println!("the first menu entry is the two numbers — paste them here as one.");
    println!("MLAT places other people's aircraft with them, so closer is better.");
    let (lat, lon) = loop {
        let v = ask("Position (\"lat, lon\")", "");
        match parse_position(&v) {
            Some(p) => break p,
            None => println!("  Two numbers, like: 48.85824, 2.29444"),
        }
    };
    let alt = loop {
        let v = ask("Antenna altitude (\"65m\" or \"213ft\")", "");
        match crate::config::parse_alt(&v) {
            Some(a) if (-1000.0..=10000.0).contains(&a) => break v,
            _ => println!("  A number with m or ft, like 12m."),
        }
    };

    let sdr = detect_rtlsdr();
    println!("\nWhere do the radio signals come from?");
    println!("  1. A readsb already running on another machine (host:port of its data stream)");
    if sdr {
        println!("  2. This machine's SDR dongle — one is plugged in right now");
    } else {
        println!("  2. This machine's SDR dongle (stationd will run readsb)");
    }
    let default_input = if sdr { "2" } else { "1" };
    let choice = ask("Input", default_input);
    let (input_beast, readsb_json, readsb_prog, radio_prog) = if choice.trim() == "2" {
        let json_dir = "state/readsb".to_string();
        let radio_prog = rx.map(|p| readsb_program(p, &json_dir, lat, lon));
        let readsb = if let Some(p) = rx {
            println!("  The Station radio is installed at {p}; readsb stays as its fallback.");
            ask("Path to the readsb binary (Enter = none, no fallback)", "")
        } else {
            ask("Path to the readsb binary", "/usr/local/bin/readsb")
        };
        let readsb_prog = Some(readsb.trim())
            .filter(|s| !s.is_empty())
            .map(|p| readsb_program(p, &json_dir, lat, lon));
        (
            "127.0.0.1:30005".to_string(),
            Some(json_dir.clone()),
            readsb_prog,
            radio_prog,
        )
    } else {
        let src = loop {
            let v = ask("Beast source (host:port)", "127.0.0.1:30005");
            if v.contains(':') {
                break v;
            }
            println!("  host:port, like 192.168.1.10:30005.");
        };
        (src, None, None, None)
    };

    println!("\nAggregators. Feeding is non-exclusive; add as many as you like.");
    for (i, a) in CATALOG.iter().enumerate() {
        let what = match (a.adsb, a.mlat) {
            (Some(_), Some(_)) => "ADS-B + MLAT",
            (Some(_), None) => "ADS-B",
            _ => "MLAT",
        };
        println!("  {}. {} ({what}) — {}", i + 1, a.name, a.note);
    }
    println!("  {}. somewhere else", CATALOG.len() + 1);
    let mut feeds: Vec<FeedChoice> = Vec::new();
    loop {
        let done_hint = if feeds.is_empty() {
            ""
        } else {
            ", Enter = done"
        };
        let c = ask(&format!("Add a feed (number{done_hint})"), "");
        if c.is_empty() {
            if feeds.is_empty() {
                println!("  At least one feed, or the station tells no one.");
                continue;
            }
            break;
        }
        let (fname, adsb, mlat) = match c.parse::<usize>() {
            Ok(i) if i >= 1 && i <= CATALOG.len() => {
                let a = &CATALOG[i - 1];
                (
                    a.name.to_string(),
                    a.adsb.map(String::from),
                    a.mlat.map(String::from),
                )
            }
            Ok(i) if i == CATALOG.len() + 1 => {
                let n = ask("  Feed name", "");
                let m = ask("  MLAT server (host:port, Enter = none)", "");
                let d = ask("  ADS-B / Beast destination (host:port, Enter = none)", "");
                (
                    n,
                    Some(d).filter(|s| !s.is_empty()),
                    Some(m).filter(|s| !s.is_empty()),
                )
            }
            _ => continue,
        };
        let uuid = ask(
            &format!("  Station key for {fname}, if they gave you one (Enter = none)"),
            "",
        );
        feeds.push(FeedChoice {
            name: fname,
            adsb,
            mlat,
            uuid,
        });
    }

    let (feeds, notes) = resolve_feeds(
        feeds,
        readsb_prog.is_some() || radio_prog.is_some(),
        &station_uuid,
    );
    for n in &notes {
        println!("  note: {n}");
    }
    if feeds.is_empty() {
        println!("No usable feeds; nothing to set up.");
        return Ok(None);
    }

    let a = Answers {
        name,
        lat,
        lon,
        alt,
        input_beast,
        readsb_json,
        readsb_prog,
        radio_prog,
        station_uuid,
        feeds,
        listen: None,
    };
    print_summary(&a);
    let go = ask("Write this configuration", "yes");
    if !go.eq_ignore_ascii_case("yes") && !go.eq_ignore_ascii_case("y") {
        return Ok(None);
    }
    Ok(Some(a))
}

// --- shared tail --------------------------------------------------------

fn print_summary(a: &Answers) {
    println!("\nThe station, in short:");
    println!("  {} at {}, {}, antenna at {}", a.name, a.lat, a.lon, a.alt);
    let input_desc = if a.radio_prog.is_some() {
        "this machine's SDR, through the Station radio"
    } else if a.readsb_prog.is_some() {
        "this machine's SDR, through readsb"
    } else {
        &a.input_beast
    };
    println!("  frames from {input_desc}");
    println!("  station key {} — keep it; it marks the feeds as yours", a.station_uuid);
    for f in &a.feeds {
        let mut what = Vec::new();
        if let Some(d) = &f.adsb {
            what.push(format!("ADS-B to {d}"));
        }
        if let Some(m) = &f.mlat {
            what.push(format!("MLAT to {m}"));
        }
        println!("  feeding {} ({})", f.name, what.join(", "));
    }
}

/// The station.toml a set of answers means, byte for byte.
pub fn render_toml(a: &Answers) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[station]\nname = \"{}\"\nlat = {}\nlon = {}\nalt = \"{}\"\n\n[input]\nbeast = \"{}\"\n",
        a.name, a.lat, a.lon, a.alt, a.input_beast
    ));
    if let Some(j) = &a.readsb_json {
        out.push_str(&format!("readsb_json = \"{j}\"\n"));
    }
    for f in &a.feeds {
        out.push_str(&format!("\n[[feed]]\nname = \"{}\"\n", f.name));
        if let Some(d) = &f.adsb {
            out.push_str(&format!("adsb = \"{d}\"\n"));
        }
        if let Some(m) = &f.mlat {
            out.push_str(&format!("mlat = \"{m}\"\n"));
        }
        if !f.uuid.is_empty() {
            out.push_str(&format!("uuid = \"{}\"\n", f.uuid));
        }
    }
    if a.radio_prog.is_some() || a.readsb_prog.is_some() {
        // MLAT results come back into the local radio's Beast input.
        out.push_str(&format!("\n[results]\nbeast_connect = \"{RESULTS_LOCAL}\"\n"));
    }
    if let Some(l) = &a.listen {
        out.push_str(&format!("\n[status]\nlisten = \"{l}\"\n"));
    }
    if a.radio_prog.is_some() || a.readsb_prog.is_some() {
        out.push_str("\n[programs]\n");
        if let Some(r) = &a.radio_prog {
            out.push_str(&format!("radio = \"{r}\"\n"));
        }
        if let Some(r) = &a.readsb_prog {
            out.push_str(&format!("readsb = \"{r}\"\n"));
        }
    }
    out
}

/// Validation problems in a rendered configuration (a parse failure is
/// an error — the renderer produced garbage, which is a bug here).
pub fn check_rendered(out: &str) -> anyhow::Result<Vec<String>> {
    let cfg: crate::config::Config = toml::from_str(out)?;
    Ok(cfg.problems())
}

fn write_config(config_path: &std::path::Path, a: &Answers) -> anyhow::Result<()> {
    let out = render_toml(a);
    let problems = check_rendered(&out)?;
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("  - {p}");
        }
        anyhow::bail!("the generated configuration has problems; please report this");
    }
    if config_path.exists() {
        let backup = config_path.with_extension("toml.old");
        std::fs::rename(config_path, &backup)?;
        println!("\nExisting configuration kept at {}.", backup.display());
    }
    std::fs::write(config_path, &out)?;
    println!("Wrote {}.", config_path.display());
    if std::path::Path::new("/etc/systemd/system/stationd.service").exists() {
        println!("Apply it:  sudo systemctl restart stationd");
    } else {
        println!(
            "Start the station:  stationd --config {}",
            config_path.display()
        );
        println!("(scripts/install.sh sets it up as a service that survives reboots.)");
    }
    Ok(())
}

/// An RTL-SDR on the USB bus, detected without any tooling: the known
/// vendor:product pairs in sysfs.
pub fn detect_rtlsdr() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return false;
    };
    for e in dir.flatten() {
        let p = e.path();
        let vid = std::fs::read_to_string(p.join("idVendor")).unwrap_or_default();
        let pid = std::fs::read_to_string(p.join("idProduct")).unwrap_or_default();
        if vid.trim() == "0bda" && matches!(pid.trim(), "2838" | "2832") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeds() -> Vec<FeedChoice> {
        vec![
            FeedChoice {
                name: "fp".into(),
                adsb: Some("feed.example:30004".into()),
                mlat: None,
                uuid: String::new(),
            },
            FeedChoice {
                name: "lol".into(),
                adsb: Some("in.example:30004".into()),
                mlat: Some("in.example:31090".into()),
                uuid: String::new(),
            },
        ]
    }

    #[test]
    fn every_feed_gets_the_station_key() {
        let (f, _) = resolve_feeds(feeds(), true, "k-1");
        assert!(f.iter().all(|x| x.uuid == "k-1"));
    }

    #[test]
    fn rendered_config_with_the_radio_is_valid_and_complete() {
        let (feeds, _) = resolve_feeds(feeds(), true, "k-1");
        let a = Answers {
            name: "t".into(),
            lat: 1.3,
            lon: 103.8,
            alt: "65m".into(),
            input_beast: "127.0.0.1:30005".into(),
            readsb_json: Some("state/readsb".into()),
            readsb_prog: Some(readsb_program("/bin/sh", "state/readsb", 1.3, 103.8)),
            radio_prog: Some(readsb_program("/bin/sh", "state/readsb", 1.3, 103.8)),
            station_uuid: "k-1".into(),
            feeds,
            listen: None,
        };
        let out = render_toml(&a);
        assert!(out.contains("radio = \"/bin/sh --device-type rtlsdr"), "{out}");
        assert!(out.contains("readsb = \"/bin/sh"), "{out}");
        assert!(out.contains("--net-bi-port 30004"), "{out}");
        assert!(out.contains("[results]\nbeast_connect = \"127.0.0.1:30004\""), "{out}");
        assert!(check_rendered(&out).unwrap().is_empty(), "{:?}", check_rendered(&out));
    }

    #[test]
    fn remote_receiver_writes_no_results_or_programs() {
        let (feeds, _) = resolve_feeds(feeds(), false, "k-1");
        let a = Answers {
            name: "t".into(),
            lat: 1.3,
            lon: 103.8,
            alt: "65m".into(),
            input_beast: "10.0.0.2:30005".into(),
            readsb_json: None,
            readsb_prog: None,
            radio_prog: None,
            station_uuid: "k-1".into(),
            feeds,
            listen: None,
        };
        let out = render_toml(&a);
        assert!(!out.contains("[results]") && !out.contains("[programs]"), "{out}");
    }
}

/// Random UUID-shaped identifier from the system generator.
pub fn pseudo_uuid() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        return "00000000-0000-4000-8000-000000000000".into();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].join(""),
        h[4..6].join(""),
        h[6..8].join(""),
        h[8..10].join(""),
        h[10..16].join("")
    )
}
