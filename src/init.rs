//! Interactive setup: questions in, a valid station.toml out.
//!
//! The point is that a beginner never opens an editor or hunts for an
//! aggregator's hostname. Every answer is validated on the spot with
//! the same rules the daemon enforces at startup.

use std::io::{BufRead, Write};

/// Aggregators offered by number. Feeding is non-exclusive; pick any.
const CATALOG: &[(&str, &str)] = &[
    ("adsb.lol", "in.adsb.lol:31090"),
    ("adsb.fi", "feed.adsb.fi:31090"),
    ("adsb.win", "mlat.adsb.win:31090"),
];

pub fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
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

    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let name = ask(
        "Station name (MLAT servers identify you by this)",
        &hostname,
    );

    println!("\nThe antenna's position. Right-click your house in Google Maps;");
    println!("the first menu entry is the two numbers — paste them here as one.");
    println!("MLAT places other people's aircraft with them, so closer is better.");
    let (lat, lon) = loop {
        let v = ask("Position (\"lat, lon\")", "");
        let parts: Vec<f64> = v
            .split([',', ' '])
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        match parts.as_slice() {
            [la, lo] if (-90.0..=90.0).contains(la) && (-180.0..=180.0).contains(lo) => {
                break (*la, *lo)
            }
            _ => println!("  Two numbers, like: 48.85824, 2.29444"),
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
    println!("\nWhere do Mode S frames come from?");
    println!("  1. A readsb already running somewhere (host:port of its Beast output)");
    if sdr {
        println!("  2. This machine's SDR dongle — one is plugged in right now");
    } else {
        println!("  2. This machine's SDR dongle (stationd will run readsb)");
    }
    let default_input = if sdr { "2" } else { "1" };
    let choice = ask("Input", default_input);
    let (input_beast, readsb_json, readsb_prog) = if choice.trim() == "2" {
        let readsb = ask("Path to the readsb binary", "/usr/local/bin/readsb");
        let json_dir = "state/readsb".to_string();
        (
            "127.0.0.1:30005".to_string(),
            Some(json_dir.clone()),
            Some(format!(
                "{readsb} --device-type rtlsdr --gain auto --quiet \
                 --net --net-bo-port 30005 --write-json {json_dir} \
                 --write-json-every 1 --lat {lat} --lon {lon}"
            )),
        )
    } else {
        let src = loop {
            let v = ask("Beast source (host:port)", "127.0.0.1:30005");
            if v.contains(':') {
                break v;
            }
            println!("  host:port, like 192.168.1.10:30005.");
        };
        (src, None, None)
    };

    println!("\nMLAT feeds. Feeding is non-exclusive; add as many as you like.");
    for (i, (n, host)) in CATALOG.iter().enumerate() {
        println!("  {}. {n} ({host})", i + 1);
    }
    println!("  {}. somewhere else", CATALOG.len() + 1);
    let mut feeds: Vec<(String, String, String)> = Vec::new();
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
        let (fname, host) = match c.parse::<usize>() {
            Ok(i) if i >= 1 && i <= CATALOG.len() => {
                let (n, h) = CATALOG[i - 1];
                (n.to_string(), h.to_string())
            }
            Ok(i) if i == CATALOG.len() + 1 => {
                let n = ask("  Feed name", "");
                let h = ask("  MLAT server (host:port)", "");
                (n, h)
            }
            _ => continue,
        };
        let uuid = ask(
            &format!("  Station key for {fname}, if they gave you one (Enter = none)"),
            "",
        );
        feeds.push((fname, host, uuid));
    }
    // Mixing keyed and unkeyed feeds is a config error; fill quietly.
    if feeds.iter().any(|f| !f.2.is_empty()) {
        for f in &mut feeds {
            if f.2.is_empty() {
                f.2 = pseudo_uuid();
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        "[station]\nname = \"{name}\"\nlat = {lat}\nlon = {lon}\nalt = \"{alt}\"\n\n[input]\nbeast = \"{input_beast}\"\n"
    ));
    if let Some(j) = &readsb_json {
        out.push_str(&format!("readsb_json = \"{j}\"\n"));
    }
    for (n, h, u) in &feeds {
        out.push_str(&format!("\n[[feed]]\nname = \"{n}\"\nmlat = \"{h}\"\n"));
        if !u.is_empty() {
            out.push_str(&format!("uuid = \"{u}\"\n"));
        }
    }
    if let Some(r) = &readsb_prog {
        out.push_str(&format!("\n[programs]\nreadsb = \"{r}\"\n"));
    }

    println!("\nThe station, in short:");
    println!("  {name} at {lat}, {lon}, antenna at {alt}");
    let input_desc = if readsb_prog.is_some() {
        "this machine's SDR"
    } else {
        &input_beast
    };
    println!("  frames from {input_desc}");
    for (n, h, _) in &feeds {
        println!("  feeding {n} ({h})");
    }
    let go = ask("Write this configuration", "yes");
    if !go.eq_ignore_ascii_case("yes") && !go.eq_ignore_ascii_case("y") {
        println!("Nothing written.");
        return Ok(());
    }

    let cfg: crate::config::Config = toml::from_str(&out)?;
    let problems = cfg.problems();
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
fn detect_rtlsdr() -> bool {
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

/// Random UUID-shaped identifier from the system generator.
fn pseudo_uuid() -> String {
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
