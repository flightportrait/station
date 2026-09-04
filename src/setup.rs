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
}

/// Serve the wizard until a valid configuration is written, then return.
pub async fn serve(config_path: &std::path::Path, radio: Option<&std::path::Path>) -> anyhow::Result<()> {
    let l = TcpListener::bind(LISTEN).await.map_err(|e| {
        anyhow::anyhow!("setup mode cannot listen on {LISTEN}: {e} (is another stationd running?)")
    })?;
    let setup = Setup {
        rx: init::find_rx(radio),
        station_key: init::station_uuid(),
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
        "Station key {station_uuid} — keep it; it marks these feeds as yours."
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
    println!("stationd: no configuration yet — setup mode.");
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
<title>Station setup — FlightPortrait</title>
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
  <p class="sub">A few answers and this machine starts feeding.
  Everything is checked before anything is written.</p>
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
  <p class="sub">Zoom in and click its spot — rooftop precision matters,
  because MLAT places other people's aircraft with it.
  <button id="locate" class="ghost" hidden style="margin-left:8px">Use my location</button></p>
  <div id="map"></div>
  <div class="row">
    <input type="number" id="lat" step="any" placeholder="latitude">
    <input type="number" id="lon" step="any" placeholder="longitude">
  </div>
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="3" hidden>
  <p class="kicker caps">3 · Altitude</p>
  <h1>How high is the antenna?</h1>
  <p class="sub">Just the part you know — how far above the ground it sits.
  The ground's own elevation comes from the position.</p>
  <p id="groundline" class="groundline">Looking up the ground elevation…</p>
  <div id="groundrow" hidden>
    <span class="fieldlbl">ground elevation (m)</span>
    <input type="number" id="ground" step="any" placeholder="—">
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
  <p class="kicker caps">4 · Receiver</p>
  <h1>Your receiver.</h1>
  <p class="sub">Aircraft announce who and where they are by radio all day
  (the broadcasts are called ADS-B). A station hears them with a small USB
  radio — an RTL-SDR dongle — plugged into this machine, wired to an antenna.</p>
  <div class="opt" id="optsdr">
    <span class="t"><span class="mark">✓</span><b id="sdrtitle">Looking for an RTL-SDR dongle…</b></span>
    <span class="gives" id="sdrgives">none found yet — plug one in; this page notices by itself</span>
  </div>
  <div class="opt" id="optremote">
    <span class="t"><span class="mark">✓</span><b>My receiver runs on another machine</b></span>
    <span class="gives">unusual — the radio software (readsb) already runs
    on another machine; enter its address</span>
    <div id="remotebox" style="display:none;margin-top:10px">
      <span class="fieldlbl">its address (host:port — the data stream, usually port 30005)</span>
      <input type="text" id="beast" placeholder="192.168.1.10:30005" autocomplete="off"></div>
  </div>
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="5" hidden>
  <p class="kicker caps">5 · Feeding</p>
  <h1>Who should hear about your sky?</h1>
  <p class="sub">Aggregators combine thousands of stations into one live picture
  of the sky. Feeding is free and non-exclusive — sending to one costs the
  others nothing.</p>
  <div id="feeds"></div>
  <div class="opt" id="customcard">
    <span class="t"><span class="tile" style="background:var(--line);color:var(--ink)">+</span><b>Somewhere else</b></span>
    <span class="gives">any other network — an MLAT server, a raw-data destination, or both</span>
    <div id="customfields" hidden>
      <input type="text" id="cname" placeholder="name" autocomplete="off">
      <input type="text" id="cmlat" placeholder="MLAT server host:port (optional)" autocomplete="off">
      <input type="text" id="cadsb" placeholder="ADS-B destination host:port (optional)" autocomplete="off">
      <input type="text" id="ckey" placeholder="station key (optional)" autocomplete="off">
    </div>
  </div>
  <p class="finehint">A station key is a UUID that marks a feed as yours.
  Each card explains its own — and empty always works: you simply feed
  anonymously.</p>
  <div class="actions">
    <button class="cta" data-next>CONTINUE</button>
    <span class="enter">press Enter ↵</span>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step" data-step="6" hidden>
  <p class="kicker caps">6 · Review</p>
  <h1>The station, in short.</h1>
  <div id="summary"></div>
  <div id="notes"></div>
  <div id="problems"></div>
  <div class="actions">
    <button class="cta" id="go">SET UP THE STATION</button>
    <button class="back" data-back>Back</button>
  </div>
</section>

<section class="step center" data-step="7" hidden>
  <svg class="bigmark" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>
  <h1 id="donehead">Starting the station…</h1>
  <p class="sub" id="donesub">Writing the configuration and raising the receiver stack.</p>
  <div id="livecount" hidden>–</div>
  <div id="livelbl" hidden>aircraft over you right now</div>
  <div id="notes7"></div>
  <div class="actions"><a class="cta" id="openpage" href="/" hidden>OPEN THE STATION PAGE</a></div>
</section>

</div></main>

<script src="/vendor/maplibre-gl.js"></script>
<script>
let map, pin, catalog = [], active = 0;
let sdrFound = false, remoteChosen = false, radioPath = '', stationKey = '';
const steps = [...document.querySelectorAll('.step')];
const LAST_Q = 6;

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
    if (n === 6) buildSummary();
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

function mode() { return remoteChosen ? 'remote' : 'sdr'; }

function validate(n) {
  if (n === 1 && !document.getElementById('name').value.trim())
    return 'The station needs a name — anything you like.';
  if (n === 2) {
    const la = parseFloat(document.getElementById('lat').value);
    const lo = parseFloat(document.getElementById('lon').value);
    if (isNaN(la) || isNaN(lo)) return 'Click the antenna\'s spot on the map first.';
  }
  if (n === 3 && isNaN(parseFloat(document.getElementById('ground').value))) {
    elevState = 'manual'; renderGround();
    return 'The ground elevation is still unknown — type it in.';
  }
  if (n === 4) {
    if (mode() === 'remote') {
      if (!document.getElementById('beast').value.includes(':'))
        return 'That address needs host:port, like 192.168.1.10:30005.';
    } else if (!sdrFound) {
      return 'No dongle found yet — plug it in, or pick the second card.';
    }
  }
  if (n === 5 && !pickedFeeds().length)
    return 'Pick at least one — or the station tells no one.';
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

// ---- receiver cards ---------------------------------------------------
function renderReceiver() {
  document.getElementById('optsdr').classList.toggle('on', mode() === 'sdr');
  document.getElementById('optremote').classList.toggle('on', mode() === 'remote');
  document.getElementById('remotebox').style.display =
    mode() === 'remote' ? 'block' : 'none';
  document.getElementById('sdrtitle').textContent = sdrFound
    ? 'RTL-SDR dongle found on this machine'
    : 'Looking for an RTL-SDR dongle…';
  document.getElementById('sdrgives').textContent = sdrFound
    ? (radioPath
        ? 'stationd runs the Station radio for it and feeds from there'
        : 'stationd runs the radio software (readsb) for it and feeds from there')
    : 'none found yet — plug one in; this page notices by itself';
}
document.getElementById('optsdr').addEventListener('click', () => {
  remoteChosen = false; renderReceiver();
});
document.getElementById('optremote').addEventListener('click', e => {
  remoteChosen = true; renderReceiver();
  if (e.target.id !== 'beast') document.getElementById('beast').focus();
});
setInterval(() => {
  fetch('/setup/info').then(r => r.json()).then(d => {
    if (d.sdr !== sdrFound) { sdrFound = d.sdr; renderReceiver(); }
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
// brand's basemap reskin — paper ground, warm sea, quiet ink line-work.
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
      'No map here (offline?) — type coordinates below.';
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

if (window.isSecureContext && navigator.geolocation) {
  const b = document.getElementById('locate');
  b.hidden = false;
  b.onclick = () => navigator.geolocation.getCurrentPosition(
    p => setPos(p.coords.latitude, p.coords.longitude, true),
    () => { b.textContent = 'Location unavailable — click the map'; });
}

// ---- feeds ------------------------------------------------------------
const FPMARK = '<svg viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#221f1a"/><path d="M10.5 36Q9 35 10.74 34.54L27.13 30.23Q28 30 28.42 29.21L35.58 15.79Q36 15 36.88 15.18L40.12 15.82Q41 16 40.75 16.87L37.25 29.13Q37 30 37.87 30.25L51 34C54 35 55 37 54.3 38.4Q54 39 53 38.94L36.9 38.05Q36 38 35.46 38.72L27.54 49.28Q27 50 26.13 49.78L23.87 49.22Q23 49 23.35 48.17L27.65 37.83Q28 37 27.11 37.14L15.89 38.86Q15 39 14.25 38.5Z" fill="#f5f1e6" transform="rotate(29 32 32)"/></svg>';
const TILECOLORS = ['var(--blue)', 'var(--green)', 'var(--yellow)', 'var(--red)'];
function tileFor(a, i) {
  if (a.name === 'FlightPortrait') return '<span class="tile">' + FPMARK + '</span>';
  const letter = a.name.replace(/^adsb\./, '').charAt(0).toUpperCase();
  return '<span class="tile" style="background:' + TILECOLORS[i % TILECOLORS.length] + '">' + letter + '</span>';
}

function pickedFeeds() {
  const out = [];
  document.querySelectorAll('#feeds input[type=checkbox]').forEach(c => {
    if (!c.checked) return;
    const a = catalog[+c.dataset.i];
    out.push({ name: a.name, adsb: a.adsb, mlat: a.mlat,
      uuid: (document.getElementById('key' + c.dataset.i) || {value:''}).value.trim() });
  });
  if (document.getElementById('customcard').classList.contains('on')) {
    const name = document.getElementById('cname').value.trim();
    const cm = document.getElementById('cmlat').value.trim();
    const ca = document.getElementById('cadsb').value.trim();
    if (name && (cm || ca))
      out.push({ name, adsb: ca || null, mlat: cm || null,
        uuid: document.getElementById('ckey').value.trim() });
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
  remoteChosen = false;
  renderReceiver();
  if (d.hostname && !document.getElementById('name').value)
    document.getElementById('name').value = d.hostname;
  document.getElementById('feeds').innerHTML = d.catalog.map((a, i) => {
    const what = a.adsb && a.mlat ? 'ADS-B + MLAT' : (a.adsb ? 'ADS-B' : 'MLAT');
    return '<label class="opt on"><input type="checkbox" data-i="'+i+'" checked>'
      + '<span class="t">' + tileFor(a, i) + '<b>'+a.name+'</b>'
      + '<a class="ext" href="'+a.url+'" target="_blank" rel="noopener" onclick="event.stopPropagation()">site ↗</a></span>'
      + '<span class="gives">'+what+' — '+a.gives+'</span>'
      + '<button type="button" class="linky keylink" data-i="'+i+'">add a station key</button>'
      + '<div class="keyfield" id="kf'+i+'">'
      + '<input type="text" id="key'+i+'" placeholder="station key (a UUID)" autocomplete="off">'
      + '<p class="finehint">'+a.key_hint+' · <button type="button" class="linky genkey" data-i="'+i+'">generate one</button></p>'
      + '</div></label>';
  }).join('');
  document.querySelectorAll('#feeds input[type=checkbox]').forEach(c =>
    c.addEventListener('change', () =>
      c.closest('.opt').classList.toggle('on', c.checked)));
  document.querySelectorAll('.keylink').forEach(b => b.addEventListener('click', e => {
    e.stopPropagation(); e.preventDefault();
    const f = document.getElementById('kf' + b.dataset.i);
    f.classList.toggle('open');
    if (f.classList.contains('open')) document.getElementById('key' + b.dataset.i).focus();
  }));
  document.querySelectorAll('.genkey').forEach(b => b.addEventListener('click', e => {
    e.stopPropagation(); e.preventDefault();
    const inp = document.getElementById('key' + b.dataset.i);
    inp.value = (crypto.randomUUID && crypto.randomUUID()) || '';
  }));
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
  if (stationKey) p('Station key ' + stationKey + ' — keep it; it marks these feeds as yours.');
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
  } catch (e) { return showProblems(['The station did not answer — is it still running?'], []); }
  if (!d.ok) return showProblems(d.problems || [], d.notes || []);
  fill('notes7', d.notes || []);
  show(7, false);
  const poll = setInterval(async () => {
    try {
      const s = await (await fetch('/status.json')).json();
      if (!s.station) return;
      document.getElementById('donehead').textContent = 'Your station is live.';
      document.getElementById('donesub').textContent =
        'It kept your answers, started the receiver stack, and is feeding.';
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
