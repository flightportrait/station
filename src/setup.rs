//! Setup mode: no configuration file means stationd serves a browser
//! wizard on the LAN instead of failing. The user opens one URL (printed
//! with a QR code), clicks their antenna's position on a map, ticks
//! aggregators, and submits; stationd validates with the same sentences
//! as startup, writes station.toml, and continues into normal operation
//! in the same process. The terminal wizard (--init) remains for SSH.
//!
//! Unauthenticated on the LAN by design, like a router's first-run page:
//! it only exists while there is no configuration, and the surface it
//! writes is the same file the owner could edit anyway.

use crate::init;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const LISTEN: &str = "0.0.0.0:8654";
const MAPLIBRE_JS: &str = include_str!("vendor/maplibre-gl.js");
const MAPLIBRE_CSS: &str = include_str!("vendor/maplibre-gl.css");

/// What this machine offers the wizard: the Station radio if installed,
/// and the station key made once for this setup.
pub struct Setup {
    pub rx: Option<String>,
    pub station_key: String,
    /// Feeds carried over from a previous receiver, when the installer
    /// found one; the page starts from them.
    pub imported: Option<init::Imported>,
}

/// Serve the wizard until a valid configuration is written, then return.
pub async fn serve(
    config_path: &std::path::Path,
    radio: Option<&std::path::Path>,
    key: Option<&str>,
    imported: Option<&init::Imported>,
) -> anyhow::Result<()> {
    let l = TcpListener::bind(LISTEN).await.map_err(|e| {
        anyhow::anyhow!("setup mode cannot listen on {LISTEN}: {e} (is another stationd running?)")
    })?;
    let setup = Setup {
        rx: init::find_rx(radio),
        station_key: key
            .map(String::from)
            .or_else(|| imported.and_then(|i| i.station_key.clone()))
            .unwrap_or_else(init::station_uuid),
        imported: imported.cloned(),
    };
    announce();
    loop {
        let Ok((mut sock, _)) = l.accept().await else {
            continue;
        };
        let Some((head, body)) = read_request(&mut sock).await else {
            continue;
        };
        let (status, ctype, resp_body, done) = route(&head, &body, config_path, &setup);
        let resp = format!(
            "HTTP/1.0 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{resp_body}",
            resp_body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        if done {
            println!("stationd: configuration written; leaving setup mode.");
            return Ok(());
        }
    }
}

fn route(
    head: &str,
    body: &str,
    config_path: &std::path::Path,
    setup: &Setup,
) -> (&'static str, &'static str, String, bool) {
    let ok = "200 OK";
    if head.starts_with("GET / ") {
        return (ok, "text/html; charset=utf-8", SETUP_PAGE.into(), false);
    }
    if head.starts_with("GET /vendor/maplibre-gl.js") {
        return (ok, "application/javascript", MAPLIBRE_JS.into(), false);
    }
    if head.starts_with("GET /vendor/maplibre-gl.css") {
        return (ok, "text/css", MAPLIBRE_CSS.into(), false);
    }
    if head.starts_with("GET /setup/info") {
        return (ok, "application/json", info_json(setup), false);
    }
    if head.starts_with("POST /setup") {
        return match apply(body, config_path, setup) {
            Ok((resp, done)) => (ok, "application/json", resp, done),
            Err(e) => (
                "400 Bad Request",
                "application/json",
                serde_json::json!({ "problems": [e.to_string()] }).to_string(),
                false,
            ),
        };
    }
    // The status page's poll during the handover lands here harmlessly.
    (
        "404 Not Found",
        "application/json",
        serde_json::json!({ "setup": true }).to_string(),
        false,
    )
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 65536 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let clen: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_owned)
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if clen > 65536 {
        return None;
    }
    while buf.len() < header_end + clen {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
    Some((head, body))
}

fn info_json(setup: &Setup) -> String {
    serde_json::json!({
        "hostname": std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default(),
        "sdr": init::detect_rtlsdr(),
        "radio": setup.rx,
        "station_key": setup.station_key,
        // What a previous receiver on this machine fed, if the installer
        // found one: catalog entries to tick (by index) with their keys,
        // and the rest as extra cards.
        "imported": setup.imported.as_ref().map(|imp| serde_json::json!({
            "catalog": init::CATALOG.iter().enumerate().filter_map(|(i, a)| {
                imp.matching(a).map(|f| serde_json::json!({ "i": i, "uuid": f.uuid }))
            }).collect::<Vec<_>>(),
            "extras": imp.extras().iter().map(|f| serde_json::json!({
                "name": f.name, "adsb": f.adsb, "mlat": f.mlat, "uuid": f.uuid,
            })).collect::<Vec<_>>(),
        })),
        "catalog": init::CATALOG.iter().map(|a| serde_json::json!({
            "name": a.name, "adsb": a.adsb, "mlat": a.mlat,
            "note": a.note, "gives": a.gives, "url": a.url,
            "key_hint": a.key_hint,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

#[derive(Deserialize)]
struct Submission {
    name: String,
    lat: f64,
    lon: f64,
    alt_m: f64,
    input: SubmissionInput,
    feeds: Vec<SubmissionFeed>,
    /// The station key shown on the review card; the server's own when
    /// absent.
    #[serde(default)]
    station_key: String,
}

#[derive(Deserialize)]
struct SubmissionInput {
    mode: String,
    #[serde(default)]
    beast: String,
    #[serde(default)]
    readsb_path: String,
}

#[derive(Deserialize)]
struct SubmissionFeed {
    name: String,
    #[serde(default)]
    adsb: Option<String>,
    #[serde(default)]
    mlat: Option<String>,
    #[serde(default)]
    uuid: String,
}

/// One submission: build the same Answers the terminal wizard builds,
/// validate with the same sentences, write on success.
fn apply(body: &str, config_path: &std::path::Path, setup: &Setup) -> anyhow::Result<(String, bool)> {
    let sub: Submission = serde_json::from_str(body)?;
    let station_uuid = if sub.station_key.trim().len() == 36 {
        sub.station_key.trim().to_string()
    } else {
        setup.station_key.clone()
    };
    let (input_beast, readsb_json, readsb_prog, radio_prog) = if sub.input.mode == "sdr" {
        let json_dir = "state/readsb".to_string();
        // The Station radio when installed; readsb beside it as the
        // fallback, or alone when there is no radio.
        let radio_prog = setup
            .rx
            .as_deref()
            .map(|p| init::readsb_program(p, &json_dir, sub.lat, sub.lon));
        let readsb_path = if !sub.input.readsb_path.trim().is_empty() {
            Some(sub.input.readsb_path.trim().to_string())
        } else if radio_prog.is_none() {
            Some("/usr/local/bin/readsb".to_string())
        } else {
            ["/usr/local/bin/readsb", "/usr/bin/readsb"]
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .map(|p| p.to_string())
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| {
                        std::path::Path::new(&h).join("station/readsb").display().to_string()
                    })
                })
                .filter(|p| std::path::Path::new(p).exists())
        };
        let readsb_prog = readsb_path.map(|p| init::readsb_program(&p, &json_dir, sub.lat, sub.lon));
        (
            "127.0.0.1:30005".to_string(),
            Some(json_dir.clone()),
            readsb_prog,
            radio_prog,
        )
    } else {
        (sub.input.beast.trim().to_string(), None, None, None)
    };
    let non_empty = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let feeds = sub
        .feeds
        .into_iter()
        .map(|f| init::FeedChoice {
            name: f.name.trim().to_string(),
            adsb: non_empty(f.adsb),
            mlat: non_empty(f.mlat),
            uuid: f.uuid.trim().to_string(),
        })
        .collect();
    let (feeds, mut notes) = init::resolve_feeds(
        feeds,
        readsb_prog.is_some() || radio_prog.is_some(),
        &station_uuid,
    );
    notes.push(format!(
        "Station key {station_uuid}. Keep it; it marks these feeds as yours."
    ));
    if feeds.is_empty() {
        let mut problems =
            vec!["No usable feeds: pick at least one aggregator this station can send to.".into()];
        problems.extend(notes);
        return Ok((
            serde_json::json!({ "problems": problems }).to_string(),
            false,
        ));
    }
    let answers = init::Answers {
        name: sub.name.trim().to_string(),
        lat: sub.lat,
        lon: sub.lon,
        alt: format!("{}m", sub.alt_m),
        input_beast,
        readsb_json,
        readsb_prog,
        radio_prog,
        station_uuid,
        feeds,
        listen: Some(LISTEN.into()),
    };
    let out = init::render_toml(&answers);
    let problems = init::check_rendered(&out)?;
    if !problems.is_empty() {
        return Ok((
            serde_json::json!({ "problems": problems, "notes": notes }).to_string(),
            false,
        ));
    }
    std::fs::write(config_path, &out)?;
    Ok((
        serde_json::json!({ "ok": true, "notes": notes }).to_string(),
        true,
    ))
}

/// Print where to go, as a URL and as a QR code for a phone camera.
fn announce() {
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let ip = lan_ip();
    println!("stationd: no configuration yet, setup mode.");
    println!("stationd: finish in a browser on this network:");
    if !host.is_empty() {
        println!("stationd:   http://{host}.local:8654/");
    }
    if let Some(ip) = &ip {
        println!("stationd:   http://{ip}:8654/");
    }
    if let Some(ip) = ip {
        for line in qr_lines(&format!("http://{ip}:8654/")) {
            println!("  {line}");
        }
    }
}

/// The address this machine reaches the LAN with: the local end of a
/// UDP socket "connected" outward (no packet is sent).
fn lan_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

/// QR as terminal lines, two modules per character row. Light modules
/// are drawn as blocks so the code carries its own quiet zone and reads
/// correctly on dark terminals.
fn qr_lines(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return Vec::new();
    };
    let w = code.width();
    let colors = code.to_colors();
    let light = |x: i32, y: i32| {
        if x < 0 || y < 0 || x >= w as i32 || y >= w as i32 {
            return true;
        }
        colors[y as usize * w + x as usize] == qrcode::Color::Light
    };
    let mut lines = Vec::new();
    let mut y = -2i32;
    while y < w as i32 + 2 {
        let mut line = String::new();
        for x in -2..w as i32 + 2 {
            line.push(match (light(x, y), light(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
        y += 2;
    }
    lines
}

const SETUP_PAGE: &str = r##"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Station setup, FlightPortrait</title>
<link rel="stylesheet" href="/vendor/maplibre-gl.css">
<style>
  :root{
    --paper:#F5F1E6; --ink:#221F1A; --red:#B8402E;
    --blue:#2D5C9E; --yellow:#D9A51C; --green:#3D7A50;
    --line:rgba(34,31,26,.14); --quiet:rgba(34,31,26,.72);
    --card:#FAF7EE;
  }
  *{margin:0;padding:0;box-sizing:border-box}
  html,body{height:100%}
  body{background:var(--paper);color:var(--ink);
    font:17px/1.65 Palatino,'Palatino Linotype','Book Antiqua',Georgia,serif;
    -webkit-font-smoothing:antialiased}
  .caps{font-family:'Helvetica Neue',Arial,sans-serif;
    font-size:13px;letter-spacing:.1em}

  #bar{position:fixed;top:0;left:0;height:3px;background:var(--ink);
    width:0;transition:width .45s ease;z-index:9}
  .topbar{position:fixed;top:0;left:0;right:0;height:64px;z-index:5;
    display:flex;align-items:center;justify-content:space-between;
    padding:0 24px;background:var(--paper)}
  .wordmark{display:flex;align-items:center;gap:10px;font-size:17px}
  .wordmark svg{width:24px;height:24px;display:block}
  .wordmark .prod{color:var(--quiet)}
  #count{color:var(--quiet)}

  main{min-height:100%;display:flex;align-items:center;justify-content:center;
    padding:96px 24px 64px}
  .step{max-width:640px;width:100%;
    transition:opacity .3s ease,transform .3s ease}
  .step.out{opacity:0;transform:translateY(-16px)}
  .step.pre{opacity:0;transform:translateY(16px)}
  @media (prefers-reduced-motion: reduce){
    .step,#bar{transition:none}
  }
  .kicker{color:var(--red);margin-bottom:14px}
  h1{font-size:34px;line-height:1.15;font-weight:400;margin:0 0 12px}
  .sub{color:var(--quiet);max-width:52ch;margin-bottom:26px}
  input[type=text],input[type=number]{font:inherit;font-size:19px;width:100%;
    padding:10px 2px;border:0;border-bottom:1.5px solid var(--line);
    background:transparent;color:var(--ink);border-radius:0}
  input:focus{outline:none;border-bottom-color:var(--ink)}
  ::placeholder{color:rgba(34,31,26,.35)}
  .row{display:flex;gap:20px}
  .row>*{flex:1}
  #map{height:min(340px,44vh);border:1px solid var(--line);border-radius:6px;
    margin:14px 0;background:var(--card)}
  .pin{width:14px;height:14px;background:var(--red);border:2px solid var(--paper);
    border-radius:50%;box-shadow:0 0 4px rgba(34,31,26,.5)}
  .ghost{font:inherit;background:transparent;border:1px solid var(--line);
    border-radius:6px;padding:8px 16px;color:var(--ink);cursor:pointer}
  .ghost:hover{border-color:var(--ink)}
  .linky{background:none;border:0;font:inherit;color:var(--red);
    cursor:pointer;padding:0;text-decoration:underline}

  .actions{margin-top:30px;display:flex;align-items:center;gap:20px}
  .cta{display:inline-block;background:var(--ink);color:var(--paper);border:0;
    padding:14px 28px;font-family:'Helvetica Neue',Arial,sans-serif;
    font-size:13px;letter-spacing:.1em;cursor:pointer;border-radius:0;
    text-decoration:none}
  .cta:hover{background:var(--red)}
  .back{background:none;border:0;font:inherit;color:var(--quiet);
    cursor:pointer;padding:0}
  .back:hover{color:var(--ink)}
  .enter{color:var(--quiet);font-size:14px}
  @media (max-width:600px){.enter{display:none}}

  .opt{display:block;border:1px solid var(--line);border-radius:6px;
    background:var(--card);padding:16px 18px;margin:10px 0;cursor:pointer}
  #feeds{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin:0 0 12px}
  #feeds .opt{margin:0}
  @media (max-width:600px){#feeds{grid-template-columns:1fr}}
  .tile img{width:28px;height:28px;border-radius:6px;display:block}
  .opt:hover{border-color:var(--quiet)}
  .opt.on{border-color:var(--ink);box-shadow:inset 0 0 0 1px var(--ink)}
  .opt .t{display:flex;align-items:center;gap:10px}
  .opt .mark{color:var(--red);visibility:hidden}
  .opt.on .mark{visibility:visible}
  .opt .gives{color:var(--quiet);font-size:15px;display:block;margin-top:4px}
  .opt input[type=radio],.opt input[type=checkbox]{display:none}
  .tile{width:28px;height:28px;border-radius:6px;display:inline-flex;flex:none;
    align-items:center;justify-content:center;color:#F5F1E6;
    font-family:'Helvetica Neue',Arial,sans-serif;font-size:14px}
  .tile svg{width:28px;height:28px;display:block}
  .ext{margin-left:auto;color:var(--red);font-size:14px;text-decoration:none;flex:none}
  .ext:hover{text-decoration:underline}
  .keylink{font-size:14px;margin-top:6px;display:inline-block}
  .keyfield{margin-top:8px;display:none}
  .keyfield.open{display:block}
  #customfields{margin-top:10px;display:grid;gap:8px}
  #customfields[hidden]{display:none}
  label.small{display:flex;gap:8px;align-items:center;color:var(--quiet);
    margin-top:14px;cursor:pointer;font-size:15px}
  .finehint{color:var(--quiet);font-size:14px;margin-top:6px;max-width:52ch}

  .fieldlbl{display:block;margin-bottom:2px;color:var(--quiet);font-size:14px}
  .groundline{margin-bottom:20px}
  .groundline b{font-weight:600}
  #groundrow{margin-bottom:20px}
  #alttotal{margin-top:14px;color:var(--quiet)}
  #summary p{margin-bottom:8px}
  #problems div{border-left:3px solid var(--red);background:rgba(184,64,46,.07);
    padding:.45rem .9rem;margin:.6rem 0}
  #notes div,#notes7 div{border-left:3px solid var(--yellow);background:rgba(217,165,28,.09);
    padding:.45rem .9rem;margin:.6rem 0;font-size:15px}
  #notes7{text-align:left;max-width:560px;margin:0 auto 8px}
  .bigmark{width:56px;height:56px;margin-bottom:26px}
  .center{text-align:center}
  .center .sub{margin-left:auto;margin-right:auto}
  .center .actions{justify-content:center}
  #livecount{font-size:52px;line-height:1.1;margin:10px 0 2px}
  #livelbl{color:var(--quiet);font-size:15px;margin-bottom:8px}
</style>

<div id="bar"></div>
<div class="topbar">
  <span class="wordmark"><svg viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>
    FlightPortrait <span class="prod">· Station</span></span>
  <span id="count" class="caps"></span>
</div>

<main><div id="stage">

<section class="step center" data-step="0">
  <svg class="bigmark" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>
  <h1>Set up your station.</h1>
  <p class="sub">Follow this interactive setup to start watching the aircraft above you.
  This should only take 2 minutes.</p>
  <div class="actions"><button class="cta" data-next>BEGIN</button></div>
</section>

<section class="step" data-step="1" hidden>
  <p class="kicker caps">1 · Identity</p>
  <h1>What should this station be called?</h1>
  <p class="sub">MLAT servers identify it by this name.</p>
  <input type="text" id="name" placeholder="my-station" autocomplete="off">
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="2" hidden>
  <p class="kicker caps">2 · Position</p>
  <h1>Where is the antenna?</h1>
  <p class="sub">Zoom in and press on its location. It has to be precise so try
  your best to pinpoint its exact location.</p>
  <p><button id="locate" class="ghost">Center on my location</button></p>
  <div id="map"></div>
  <input type="hidden" id="lat">
  <input type="hidden" id="lon">
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="3" hidden>
  <p class="kicker caps">3 · Altitude</p>
  <h1>How high is the antenna?</h1>
  <p class="sub">Just tell us how far above the ground it sits.</p>
  <p id="groundline" class="groundline">Looking up the ground elevation…</p>
  <div id="groundrow" hidden>
    <span class="fieldlbl">ground elevation (m)</span>
    <input type="number" id="ground" step="any">
  </div>
  <div>
    <span class="fieldlbl">antenna above the ground (m)</span>
    <input type="number" id="mast" step="any" value="5">
  </div>
  <p id="alttotal"></p>
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="4" hidden>
  <p class="kicker caps">4 · Feeding</p>
  <h1>Who should hear about your sky?</h1>
  <p class="sub">FlightPortrait is one of many aggregators ensuring ADS-B data
  remains public and widely accessible. You can share your antenna feed with
  multiple aggregators at the same time. We selected a few we trust below.</p>
  <div id="feeds"></div>
  <div class="opt" id="customcard">
    <span class="t"><span class="tile" style="background:var(--line);color:var(--ink)">+</span><b>Somewhere else</b></span>
    <span class="gives">An MLAT server, a raw-data destination, or both.</span>
    <div id="customfields" hidden>
      <input type="text" id="cname" placeholder="name" autocomplete="off">
      <input type="text" id="cmlat" placeholder="MLAT server host:port (optional)" autocomplete="off">
      <input type="text" id="cadsb" placeholder="ADS-B destination host:port (optional)" autocomplete="off">
    </div>
  </div>
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="5" hidden>
  <p class="kicker caps">5 · Summary</p>
  <h1>Summary</h1>
  <div id="summary"></div>
  <div id="notes"></div>
  <div id="problems"></div>
  <p class="finehint">Receiver on another machine?
  <button type="button" class="linky" id="remotelink">Enter its address</button></p>
  <div id="remotebox" hidden>
    <input type="text" id="beast" placeholder="192.168.1.10:30005" autocomplete="off">
  </div>
  <div class="actions">
    <button class="cta" id="go">SET UP THE STATION</button>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step center" data-step="6" hidden>
  <svg class="bigmark" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>
  <h1 id="donehead">Starting the station…</h1>
  <p class="sub" id="donesub">Writing the configuration and starting the receiver.</p>
  <div id="livecount" hidden>–</div>
  <div id="livelbl" hidden>aircraft over you right now</div>
  <div id="notes7"></div>
  <div class="actions"><a class="cta" id="openpage" href="/" hidden>OPEN THE STATION PAGE</a></div>
</section>

</div></main>

<script src="/vendor/maplibre-gl.js"></script>
<script>
let map, pin, catalog = [], active = 0;
let sdrFound = false, radioPath = '', stationKey = '';
const steps = [...document.querySelectorAll('.step')];
const LAST_Q = 5;

function show(n, backwards) {
  const a = steps[active], b = steps[n];
  if (a === b) return;
  active = n;
  a.classList.add(backwards ? 'pre' : 'out');
  setTimeout(() => {
    a.hidden = true; a.classList.remove('out', 'pre');
    b.hidden = false; b.classList.add(backwards ? 'out' : 'pre');
    void b.offsetWidth; // style flush, so removing the class transitions
    b.classList.remove('out', 'pre');
    if (n === 2) initMap();
    if (n === 3) renderGround();
    if (n === 5) buildSummary();
    const first = b.querySelector('input[type=text],input[type=number]');
    if (first && n !== 2 && n !== 4 && n !== 5) first.focus();
  }, 300);
  document.getElementById('bar').style.width =
    (n === 0 ? 0 : Math.min(n, LAST_Q) / LAST_Q * 100) + '%';
  document.getElementById('count').textContent =
    (n >= 1 && n <= LAST_Q) ? n + ' / ' + LAST_Q : '';
}

function complain(msg) {
  const sub = steps[active].querySelector('.sub');
  if (!sub) return alert(msg);
  if (!sub.dataset.orig) sub.dataset.orig = sub.textContent;
  sub.style.color = 'var(--red)'; sub.textContent = msg;
  setTimeout(() => { sub.style.color = ''; sub.textContent = sub.dataset.orig; }, 2600);
}

function mode() { return document.getElementById('beast').value.trim() ? 'remote' : 'sdr'; }

function validate(n) {
  if (n === 1 && !document.getElementById('name').value.trim())
    return 'The station needs a name, anything you like.';
  if (n === 2) {
    const la = parseFloat(document.getElementById('lat').value);
    const lo = parseFloat(document.getElementById('lon').value);
    if (isNaN(la) || isNaN(lo)) return 'Click the antenna\'s spot on the map first.';
  }
  if (n === 3 && isNaN(parseFloat(document.getElementById('ground').value))) {
    elevState = 'manual'; renderGround();
    return 'The ground elevation is still unknown, type it in.';
  }
  if (n === 4 && !pickedFeeds().length)
    return 'Pick at least one, or the station tells no one.';
  return null;
}

function next() {
  const bad = validate(active);
  if (bad) return complain(bad);
  show(active + 1, false);
}
document.querySelectorAll('[data-next]').forEach(b => b.onclick = next);
document.querySelectorAll('[data-back]').forEach(b =>
  b.onclick = () => show(active - 1, true));
document.addEventListener('keydown', e => {
  if (e.key !== 'Enter' || active >= LAST_Q) return;
  if (e.target.closest && e.target.closest('#map')) return;
  e.preventDefault(); next();
});

// ---- receiver -------------------------------------------------------
// The dongle is found by itself; the only question is the footnote on the
// summary for a receiver that runs on another machine.
document.getElementById('remotelink').onclick = () => {
  document.getElementById('remotebox').hidden = false;
  document.getElementById('beast').focus();
};
document.getElementById('beast').addEventListener('input', () => { if (active === 5) buildSummary(); });
setInterval(() => {
  fetch('/setup/info').then(r => r.json()).then(d => {
    if (d.sdr !== sdrFound) { sdrFound = d.sdr; if (active === 5) buildSummary(); }
  }).catch(() => {});
}, 3000);

// ---- position ---------------------------------------------------------
let elevState = 'pending';
function setPos(lat, lon, zoomTo) {
  lat = +lat.toFixed(6); lon = +lon.toFixed(6);
  document.getElementById('lat').value = lat;
  document.getElementById('lon').value = lon;
  if (map) {
    if (!pin) {
      const el = document.createElement('div'); el.className = 'pin';
      pin = new maplibregl.Marker({ element: el, draggable: true })
        .setLngLat([lon, lat]).addTo(map);
      pin.on('dragend', () => { const p = pin.getLngLat(); setPos(p.lat, p.lng, false); });
    } else pin.setLngLat([lon, lat]);
    if (zoomTo) map.flyTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 15) });
  }
  elevState = 'pending';
  fetch('https://api.open-meteo.com/v1/elevation?latitude='+lat+'&longitude='+lon)
    .then(r => r.json()).then(d => {
      if (d.elevation && d.elevation.length) {
        document.getElementById('ground').value = Math.round(d.elevation[0]);
        elevState = 'ok';
      } else { elevState = 'failed'; }
      renderGround(); altTotal();
    }).catch(() => { elevState = 'failed'; renderGround(); });
}

// ---- paper skin -------------------------------------------------------
// Mirrors paperify() on the network map (site/network/index.html): the
// brand's basemap reskin: paper ground, warm sea, quiet ink line-work.
// When it changes there, change it here in the same commit.
const BASEMAP = 'https://tiles.openfreemap.org/styles/liberty';
const INKRGB = [34, 31, 26];
const colorCtx = document.createElement('canvas').getContext('2d');
function parseColor(value) {
  if (typeof value !== 'string' || value.indexOf('{') !== -1) return null;
  colorCtx.fillStyle = '#010203';
  colorCtx.fillStyle = value;
  if (colorCtx.fillStyle === '#010203' && value !== '#010203') return null;
  colorCtx.clearRect(0, 0, 1, 1);
  colorCtx.fillRect(0, 0, 1, 1);
  const d = colorCtx.getImageData(0, 0, 1, 1).data;
  return [d[0], d[1], d[2], d[3] / 255];
}
function inkAt(alpha) {
  return 'rgba(' + INKRGB[0] + ',' + INKRGB[1] + ',' + INKRGB[2] + ',' + alpha + ')';
}
function towardInk(rgba, maxInk) {
  const L = (0.2126 * rgba[0] + 0.7152 * rgba[1] + 0.0722 * rgba[2]) / 255;
  return inkAt(+(((1 - L) * maxInk * rgba[3])).toFixed(3));
}
function mapColors(value, fn) {
  const rgba = parseColor(value);
  if (rgba) return fn(rgba);
  if (Array.isArray(value)) return value.map(v => mapColors(v, fn));
  if (value && typeof value === 'object' && value.stops) {
    const copy = JSON.parse(JSON.stringify(value));
    copy.stops = copy.stops.map(s => [s[0], mapColors(s[1], fn)]);
    return copy;
  }
  return value;
}
function paint(layer, key, value) {
  layer.paint = layer.paint || {};
  layer.paint[key] = value;
}
function paperify(style) {
  style.layers = style.layers.filter(l =>
    !/^(poi|housenumber|road_shield|highway-shield|road_one_way|natural_earth)/.test(l.id));
  style.layers.forEach(l => {
    const id = l.id;
    if (l.type === 'background') {
      paint(l, 'background-color', '#F5F1E6');
    } else if (l.type === 'fill') {
      if (id === 'water') paint(l, 'fill-color', '#E9E2D0');
      else if (id === 'building') paint(l, 'fill-color', inkAt(0.07));
      else if (/^aeroway/.test(id)) paint(l, 'fill-color', inkAt(0.10));
      else {
        Object.keys(l.paint || {}).forEach(k => {
          if (/color/.test(k))
            l.paint[k] = mapColors(l.paint[k], rgba => towardInk(rgba, 0.10));
        });
      }
    } else if (l.type === 'line') {
      if (/casing/.test(id)) paint(l, 'line-color', '#F5F1E6');
      else if (/^waterway/.test(id)) paint(l, 'line-color', '#DCD3BC');
      else if (/^boundary/.test(id)) paint(l, 'line-color', inkAt(0.34));
      else if (/motorway|trunk_primary/.test(id)) paint(l, 'line-color', inkAt(0.38));
      else if (/rail/.test(id)) paint(l, 'line-color', inkAt(0.16));
      else if (/^aeroway/.test(id)) paint(l, 'line-color', inkAt(0.35));
      else paint(l, 'line-color', inkAt(0.20));
    } else if (l.type === 'symbol') {
      paint(l, 'text-color', inkAt(0.68));
      paint(l, 'text-halo-color', 'rgba(245,241,230,.85)');
      if (l.paint && l.paint['icon-color']) paint(l, 'icon-color', inkAt(0.5));
    }
  });
  return style;
}

function initMap() {
  if (map) { map.resize(); return; }
  if (initMap.started) return;
  initMap.started = true;
  fetch(BASEMAP).then(r => r.json()).then(style => {
    map = new maplibregl.Map({
      container: 'map',
      style: paperify(style),
      center: [0, 30],
      zoom: 1.2,
      attributionControl: { compact: true },
    });
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'bottom-right');
    map.on('click', e => setPos(e.lngLat.lat, e.lngLat.lng, false));
    const la = parseFloat(document.getElementById('lat').value);
    const lo = parseFloat(document.getElementById('lon').value);
    if (!isNaN(la) && !isNaN(lo)) setPos(la, lo, true);
  }).catch(() => {
    document.getElementById('map').textContent =
      'No map here. Is this machine online?';
  });
}

// ---- altitude ---------------------------------------------------------
function renderGround() {
  const line = document.getElementById('groundline');
  const row = document.getElementById('groundrow');
  const v = document.getElementById('ground').value;
  if (elevState === 'ok' && v !== '') {
    line.hidden = false; row.hidden = true;
    line.innerHTML = '';
    line.append('The ground there is about ');
    const b = document.createElement('b'); b.textContent = v; line.append(b);
    line.append(' m above sea level. ');
    const fix = document.createElement('button');
    fix.className = 'linky'; fix.type = 'button'; fix.textContent = 'Not right?';
    fix.onclick = () => { elevState = 'manual'; renderGround(); };
    line.append(fix);
  } else if (elevState === 'pending') {
    line.hidden = false; row.hidden = true;
    line.textContent = 'Looking up the ground elevation…';
  } else {
    line.hidden = true; row.hidden = false;
  }
  altTotal();
}
function altTotal() {
  const g = parseFloat(document.getElementById('ground').value);
  const m = parseFloat(document.getElementById('mast').value);
  document.getElementById('alttotal').textContent =
    (isNaN(g) || isNaN(m)) ? '' : 'Antenna altitude: ' + (g + m).toFixed(0) + ' m above sea level.';
}
document.getElementById('ground').addEventListener('input', altTotal);
document.getElementById('mast').addEventListener('input', altTotal);
for (const id of ['lat','lon']) document.getElementById(id).addEventListener('change', () => {
  const la = parseFloat(document.getElementById('lat').value);
  const lo = parseFloat(document.getElementById('lon').value);
  if (!isNaN(la) && !isNaN(lo)) setPos(la, lo, true);
});

// Centre the map, never place the pin: the person still has to press the
// exact spot. Precise browser location only exists on https; on the LAN's
// plain http the fallback is the city from the network address.
{
  const b = document.getElementById('locate');
  const centre = (lat, lon, zoom) => { if (map) map.flyTo({ center: [lon, lat], zoom }); };
  const coarse = () => fetch('https://ipwho.is/').then(r => r.json()).then(d => {
    if (!d.success) throw 0;
    centre(d.latitude, d.longitude, 15);
  }).catch(() => { b.textContent = 'Location unavailable, find it on the map'; });
  b.onclick = () => {
    if (window.isSecureContext && navigator.geolocation) {
      navigator.geolocation.getCurrentPosition(
        p => centre(p.coords.latitude, p.coords.longitude, 18), coarse,
        { enableHighAccuracy: true, timeout: 8000 });
    } else coarse();
  };
}

// ---- feeds ------------------------------------------------------------
const FPMARK = '<svg viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>';
const LOGOS = {"adsb.lol": "data:image/png;base64,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", "adsb.fi": "data:image/png;base64,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", "adsb.win": "data:image/png;base64,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"};
const TILECOLORS = ['var(--blue)', 'var(--green)', 'var(--yellow)', 'var(--red)'];
function tileFor(a, i) {
  if (a.name === 'FlightPortrait') return '<span class="tile">' + FPMARK + '</span>';
  if (LOGOS[a.name]) return '<span class="tile"><img src="' + LOGOS[a.name] + '" alt=""></span>';
  const letter = a.name.replace(/^adsb\./, '').charAt(0).toUpperCase();
  return '<span class="tile" style="background:' + TILECOLORS[i % TILECOLORS.length] + '">' + letter + '</span>';
}

let extras = [];
function pickedFeeds() {
  const out = [];
  document.querySelectorAll('#feeds input[type=checkbox]').forEach(c => {
    if (!c.checked) return;
    if (c.dataset.x !== undefined) {
      const f = extras[+c.dataset.x];
      out.push({ name: f.name, adsb: f.adsb, mlat: f.mlat, uuid: f.uuid || '' });
      return;
    }
    const a = catalog[+c.dataset.i];
    out.push({ name: a.name, adsb: a.adsb, mlat: a.mlat, uuid: '' });
  });
  if (document.getElementById('customcard').classList.contains('on')) {
    const name = document.getElementById('cname').value.trim();
    const cm = document.getElementById('cmlat').value.trim();
    const ca = document.getElementById('cadsb').value.trim();
    if (name && (cm || ca))
      out.push({ name, adsb: ca || null, mlat: cm || null, uuid: '' });
  }
  return out;
}
document.getElementById('customcard').addEventListener('click', e => {
  if (e.target.closest('#customfields')) return;
  const card = document.getElementById('customcard');
  card.classList.toggle('on');
  document.getElementById('customfields').hidden = !card.classList.contains('on');
  if (card.classList.contains('on')) document.getElementById('cname').focus();
});

fetch('/setup/info').then(r => r.json()).then(d => {
  catalog = d.catalog;
  sdrFound = d.sdr;
  radioPath = d.radio || '';
  stationKey = d.station_key || '';
  if (d.hostname && !document.getElementById('name').value)
    document.getElementById('name').value = d.hostname;
  document.getElementById('feeds').innerHTML = d.catalog.map((a, i) => {
    const what = a.adsb && a.mlat ? 'ADS-B + MLAT' : (a.adsb ? 'ADS-B' : 'MLAT');
    return '<label class="opt on"><input type="checkbox" data-i="'+i+'" checked>'
      + '<span class="t">' + tileFor(a, i) + '<b>'+a.name+'</b>'
      + '<a class="ext" href="'+a.url+'" target="_blank" rel="noopener" onclick="event.stopPropagation()">site ↗</a></span>'
      + '<span class="gives">'+what+'. '+a.gives.charAt(0).toUpperCase()+a.gives.slice(1)+'.</span>'
      + '</label>';
  }).join('');
  // A previous receiver's feeds: tick only those, carry their keys, and
  // add the ones not on the list as cards of their own.
  // (The installer's file holds only a key on a fresh machine: nothing to
  // carry over, every card stays ticked.)
  if (d.imported && (d.imported.catalog.length || (d.imported.extras || []).length)) {
    const ticked = {};
    d.imported.catalog.forEach(m => { ticked[m.i] = m.uuid || ''; });
    document.querySelectorAll('#feeds input[type=checkbox]').forEach(c => {
      const on = c.dataset.i in ticked;
      c.checked = on; c.closest('.opt').classList.toggle('on', on);
    });
    extras = d.imported.extras || [];
    document.getElementById('feeds').insertAdjacentHTML('beforeend', extras.map((f, x) => {
      const what = f.adsb && f.mlat ? 'ADS-B + MLAT' : (f.adsb ? 'ADS-B' : 'MLAT');
      const where = [f.adsb, f.mlat].filter(Boolean).join(', ');
      return '<label class="opt on"><input type="checkbox" data-x="'+x+'" checked>'
        + '<span class="t"><span class="tile" style="background:var(--line);color:var(--ink)">'
        + f.name.charAt(0).toUpperCase() + '</span><b>'+f.name+'</b></span>'
        + '<span class="gives">'+what+'. Kept from your previous receiver: '+where+'.</span>'
        + '</label>';
    }).join(''));
  }
  document.querySelectorAll('#feeds input[type=checkbox]').forEach(c =>
    c.addEventListener('change', () =>
      c.closest('.opt').classList.toggle('on', c.checked)));
});

// ---- review + submit --------------------------------------------------
function buildSummary() {
  const g = parseFloat(document.getElementById('ground').value);
  const m = parseFloat(document.getElementById('mast').value);
  const feeds = pickedFeeds();
  const src = mode() === 'sdr' ? 'the SDR dongle on this machine'
    : document.getElementById('beast').value.trim();
  document.getElementById('summary').innerHTML = '';
  const p = (t) => { const e = document.createElement('p');
    e.textContent = t; document.getElementById('summary').append(e); };
  p(document.getElementById('name').value.trim() + ' at '
    + document.getElementById('lat').value + ', ' + document.getElementById('lon').value
    + ', antenna at ' + ((isNaN(g)?0:g)+(isNaN(m)?0:m)).toFixed(0) + ' m.');
  p('Frames from ' + src + '.');
  p('Feeding ' + feeds.map(f => f.name).join(', ') + '.');
  if (stationKey) p('Station key ' + stationKey + '. Keep it; it marks these feeds as yours.');
  showProblems(mode() === 'sdr' && !sdrFound
    ? ['No RTL-SDR dongle found on this machine. Plug it in; this page notices by itself.'] : [], []);
}

document.getElementById('go').onclick = async () => {
  const g = parseFloat(document.getElementById('ground').value);
  const m = parseFloat(document.getElementById('mast').value);
  const body = {
    name: document.getElementById('name').value.trim(),
    lat: parseFloat(document.getElementById('lat').value),
    lon: parseFloat(document.getElementById('lon').value),
    alt_m: (isNaN(g) ? 0 : g) + (isNaN(m) ? 0 : m),
    input: { mode: mode(), beast: document.getElementById('beast').value.trim() },
    feeds: pickedFeeds(),
    station_key: stationKey,
  };
  let d;
  try {
    d = await (await fetch('/setup', { method: 'POST', body: JSON.stringify(body) })).json();
  } catch (e) { return showProblems(['The station did not answer. Is it still running?'], []); }
  if (!d.ok) return showProblems(d.problems || [], d.notes || []);
  fill('notes7', d.notes || []);
  show(7, false);
  const poll = setInterval(async () => {
    try {
      const s = await (await fetch('/status.json')).json();
      if (!s.station) return;
      document.getElementById('donehead').textContent = 'Your station is live.';
      document.getElementById('donesub').textContent =
        'It is feeding.';
      document.getElementById('openpage').hidden = false;
      const n = s.receiver && s.receiver.configured ? s.receiver.aircraft : null;
      if (n != null) {
        document.getElementById('livecount').hidden = false;
        document.getElementById('livelbl').hidden = false;
        document.getElementById('livecount').textContent = n;
      }
    } catch (e) {}
  }, 2000);
};

function fill(id, items) {
  document.getElementById(id).innerHTML = items.map(() => '<div></div>').join('');
  document.querySelectorAll('#' + id + ' div').forEach((el, i) => el.textContent = items[i]);
}
function showProblems(problems, notes) { fill('problems', problems); fill('notes', notes); }
</script>
"##;
