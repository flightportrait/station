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
const LEAFLET_JS: &str = include_str!("vendor/leaflet.js");
const LEAFLET_CSS: &str = include_str!("vendor/leaflet.css");

/// Serve the wizard until a valid configuration is written, then return.
pub async fn serve(config_path: &std::path::Path) -> anyhow::Result<()> {
    let l = TcpListener::bind(LISTEN).await.map_err(|e| {
        anyhow::anyhow!("setup mode cannot listen on {LISTEN}: {e} (is another stationd running?)")
    })?;
    announce();
    loop {
        let Ok((mut sock, _)) = l.accept().await else {
            continue;
        };
        let Some((head, body)) = read_request(&mut sock).await else {
            continue;
        };
        let (status, ctype, resp_body, done) = route(&head, &body, config_path);
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
) -> (&'static str, &'static str, String, bool) {
    let ok = "200 OK";
    if head.starts_with("GET / ") {
        return (ok, "text/html; charset=utf-8", SETUP_PAGE.into(), false);
    }
    if head.starts_with("GET /vendor/leaflet.js") {
        return (ok, "application/javascript", LEAFLET_JS.into(), false);
    }
    if head.starts_with("GET /vendor/leaflet.css") {
        return (ok, "text/css", LEAFLET_CSS.into(), false);
    }
    if head.starts_with("GET /setup/info") {
        return (ok, "application/json", info_json(), false);
    }
    if head.starts_with("POST /setup") {
        return match apply(body, config_path) {
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

fn info_json() -> String {
    serde_json::json!({
        "hostname": std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default(),
        "sdr": init::detect_rtlsdr(),
        "catalog": init::CATALOG.iter().map(|a| serde_json::json!({
            "name": a.name, "adsb": a.adsb, "mlat": a.mlat,
            "note": a.note, "gives": a.gives,
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
fn apply(body: &str, config_path: &std::path::Path) -> anyhow::Result<(String, bool)> {
    let sub: Submission = serde_json::from_str(body)?;
    let (input_beast, readsb_json, readsb_prog) = if sub.input.mode == "sdr" {
        let path = if sub.input.readsb_path.trim().is_empty() {
            "/usr/local/bin/readsb"
        } else {
            sub.input.readsb_path.trim()
        };
        let json_dir = "state/readsb".to_string();
        (
            "127.0.0.1:30005".to_string(),
            Some(json_dir.clone()),
            Some(init::readsb_program(path, &json_dir, sub.lat, sub.lon)),
        )
    } else {
        (sub.input.beast.trim().to_string(), None, None)
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
    let (feeds, notes) = init::resolve_feeds(feeds, readsb_prog.is_some());
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
<title>Station setup</title>
<link rel="stylesheet" href="/vendor/leaflet.css">
<style>
  body { font: 16px/1.5 system-ui, sans-serif; margin: 2rem auto; max-width: 44rem; padding: 0 1rem; }
  h1 { font-size: 1.3rem; margin: 0 0 .3rem; }
  .sub { color: #666; margin: 0 0 1.5rem; }
  section { margin: 1.6rem 0; }
  h2 { font-size: 1rem; margin: 0 0 .4rem; }
  .hint { color: #666; font-size: .88rem; margin: .2rem 0 .6rem; }
  input[type=text], input[type=number] { font: inherit; padding: .35rem .5rem; border: 1px solid #bbb; border-radius: 4px; width: 100%; box-sizing: border-box; }
  .row { display: flex; gap: .6rem; }
  .row > * { flex: 1; }
  #map { height: 320px; border: 1px solid #ccc; border-radius: 6px; margin: .5rem 0; }
  .pin { width: 14px; height: 14px; background: #c33; border: 2px solid #fff; border-radius: 50%; box-shadow: 0 0 4px rgba(0,0,0,.5); }
  button { font: inherit; padding: .45rem .9rem; border: 1px solid #999; border-radius: 6px; background: #f5f5f5; cursor: pointer; }
  button.primary { background: #1a63c9; border-color: #1a63c9; color: #fff; font-weight: 600; padding: .6rem 1.4rem; }
  label.feed { display: block; padding: .5rem .7rem; border: 1px solid #ddd; border-radius: 6px; margin: .4rem 0; cursor: pointer; }
  label.feed b { font-weight: 600; }
  label.feed .gives { color: #666; font-size: .86rem; display: block; margin-left: 1.55rem; }
  label.opt { display: block; margin: .3rem 0; }
  #problems { margin: 1rem 0; }
  #problems div { border-left: 3px solid #c33; padding: .4rem .8rem; margin: .5rem 0; background: #fee; }
  #notes div { border-left: 3px solid #c90; padding: .4rem .8rem; margin: .5rem 0; background: #fec; font-size: .92rem; }
  #done { border-left: 3px solid #2a7; padding: .6rem .8rem; background: #efe; }
  .keyfield { margin: .2rem 0 .2rem 1.55rem; display: none; }
  .showkeys .keyfield { display: block; }
</style>
<h1>Station setup</h1>
<p class="sub">A few answers and this machine starts feeding. Everything checked before anything is written.</p>

<section>
  <h2>1 · Name</h2>
  <p class="hint">MLAT servers identify this station by it.</p>
  <input type="text" id="name" placeholder="my-station">
</section>

<section>
  <h2>2 · Antenna position</h2>
  <p class="hint">Zoom in and click the antenna's spot — rooftop precision matters, because
  MLAT places other people's aircraft with it.</p>
  <button id="locate" hidden>Use my location</button>
  <div id="map"></div>
  <div class="row">
    <input type="number" id="lat" step="any" placeholder="latitude">
    <input type="number" id="lon" step="any" placeholder="longitude">
  </div>
</section>

<section>
  <h2>3 · Antenna altitude</h2>
  <p class="hint">Ground elevation fills in automatically from the position; add how high
  the antenna sits above the ground.</p>
  <div class="row">
    <div><input type="number" id="ground" step="any" placeholder="ground elevation (m)"></div>
    <div><input type="number" id="mast" step="any" placeholder="antenna above ground (m)" value="5"></div>
  </div>
  <p class="hint" id="alttotal"></p>
</section>

<section>
  <h2>4 · Where do Mode S frames come from?</h2>
  <label class="opt"><input type="radio" name="mode" value="sdr" id="modesdr"> <span id="sdrlabel">This machine's SDR dongle (stationd runs readsb)</span></label>
  <label class="opt"><input type="radio" name="mode" value="remote" id="moderemote"> A readsb already running somewhere</label>
  <div id="remotebox" style="display:none; margin-left:1.55rem">
    <input type="text" id="beast" placeholder="host:port of its Beast output, like 192.168.1.10:30005">
  </div>
</section>

<section>
  <h2>5 · Who to feed</h2>
  <p class="hint">Non-exclusive — feeding one costs the others nothing.</p>
  <div id="feeds"></div>
  <label class="opt"><input type="checkbox" id="havekeys"> I have station keys from an aggregator</label>
</section>

<div id="notes"></div>
<div id="problems"></div>
<button class="primary" id="go">Set up the station</button>
<div id="done" hidden></div>

<script src="/vendor/leaflet.js"></script>
<script>
let map, pin, catalog = [];

function setPos(lat, lon, zoomTo) {
  lat = +lat.toFixed(6); lon = +lon.toFixed(6);
  document.getElementById('lat').value = lat;
  document.getElementById('lon').value = lon;
  if (map) {
    if (!pin) pin = L.marker([lat, lon], {icon: L.divIcon({className:'', html:'<div class="pin"></div>', iconSize:[14,14], iconAnchor:[7,7]}), draggable: true})
      .addTo(map).on('dragend', () => { const p = pin.getLatLng(); setPos(p.lat, p.lng, false); });
    pin.setLatLng([lat, lon]);
    if (zoomTo) map.setView([lat, lon], Math.max(map.getZoom(), 17));
  }
  fetch('https://api.open-meteo.com/v1/elevation?latitude='+lat+'&longitude='+lon)
    .then(r => r.json()).then(d => {
      if (d.elevation && d.elevation.length) {
        document.getElementById('ground').value = Math.round(d.elevation[0]);
        altTotal();
      }
    }).catch(() => {});
}

function altTotal() {
  const g = parseFloat(document.getElementById('ground').value);
  const m = parseFloat(document.getElementById('mast').value);
  document.getElementById('alttotal').textContent =
    (isNaN(g) || isNaN(m)) ? '' : 'Antenna altitude: ' + (g + m).toFixed(0) + ' m above sea level';
}
document.getElementById('ground').addEventListener('input', altTotal);
document.getElementById('mast').addEventListener('input', altTotal);
for (const id of ['lat','lon']) document.getElementById(id).addEventListener('change', () => {
  const la = parseFloat(document.getElementById('lat').value), lo = parseFloat(document.getElementById('lon').value);
  if (!isNaN(la) && !isNaN(lo)) setPos(la, lo, true);
});

try {
  map = L.map('map').setView([30, 0], 2);
  L.tileLayer('https://tile.openstreetmap.org/{z}/{x}/{y}.png',
    { maxZoom: 19, attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>' }).addTo(map);
  map.on('click', e => setPos(e.latlng.lat, e.latlng.lng, false));
} catch (e) { document.getElementById('map').textContent = 'No map here (offline?) — type coordinates below.'; }

if (window.isSecureContext && navigator.geolocation) {
  const b = document.getElementById('locate');
  b.hidden = false;
  b.onclick = () => navigator.geolocation.getCurrentPosition(
    p => setPos(p.coords.latitude, p.coords.longitude, true),
    () => { b.textContent = 'Location unavailable — click the map instead'; });
}

document.getElementById('modesdr').addEventListener('change', modeBox);
document.getElementById('moderemote').addEventListener('change', modeBox);
function modeBox() {
  document.getElementById('remotebox').style.display =
    document.getElementById('moderemote').checked ? '' : 'none';
}
document.getElementById('havekeys').addEventListener('change', e =>
  document.getElementById('feeds').classList.toggle('showkeys', e.target.checked));

fetch('/setup/info').then(r => r.json()).then(d => {
  catalog = d.catalog;
  if (d.hostname && !document.getElementById('name').value)
    document.getElementById('name').value = d.hostname;
  if (d.sdr) document.getElementById('sdrlabel').textContent =
    "This machine's SDR dongle — one is plugged in right now";
  (d.sdr ? document.getElementById('modesdr') : document.getElementById('moderemote')).checked = true;
  modeBox();
  document.getElementById('feeds').innerHTML = d.catalog.map((a, i) => {
    const what = a.adsb && a.mlat ? 'ADS-B + MLAT' : (a.adsb ? 'ADS-B' : 'MLAT');
    return '<label class="feed"><input type="checkbox" data-i="'+i+'" checked> <b>'+a.name+'</b>'
      + ' <span class="gives">'+what+' — '+a.gives+'</span>'
      + '<div class="keyfield"><input type="text" id="key'+i+'" placeholder="station key for '+a.name+' (leave empty if none)"></div></label>';
  }).join('');
});

document.getElementById('go').onclick = async () => {
  const feeds = [];
  document.querySelectorAll('#feeds input[type=checkbox]').forEach(c => {
    if (!c.checked) return;
    const a = catalog[+c.dataset.i];
    feeds.push({ name: a.name, adsb: a.adsb, mlat: a.mlat,
      uuid: (document.getElementById('key' + c.dataset.i) || {value:''}).value.trim() });
  });
  const g = parseFloat(document.getElementById('ground').value);
  const m = parseFloat(document.getElementById('mast').value);
  const body = {
    name: document.getElementById('name').value.trim(),
    lat: parseFloat(document.getElementById('lat').value),
    lon: parseFloat(document.getElementById('lon').value),
    alt_m: (isNaN(g) ? 0 : g) + (isNaN(m) ? 0 : m),
    input: { mode: document.getElementById('modesdr').checked ? 'sdr' : 'remote',
             beast: document.getElementById('beast').value.trim() },
    feeds,
  };
  const local = [];
  if (!body.name) local.push('The station needs a name.');
  if (isNaN(body.lat) || isNaN(body.lon)) local.push('Click the antenna\'s position on the map (or type coordinates).');
  if (isNaN(g)) local.push('Ground elevation is empty — set the position, or type it.');
  if (!feeds.length) local.push('Pick at least one aggregator to feed.');
  if (body.input.mode === 'remote' && !body.input.beast.includes(':'))
    local.push('The Beast source needs host:port, like 192.168.1.10:30005.');
  if (local.length) return showProblems(local, []);

  let d;
  try {
    d = await (await fetch('/setup', { method: 'POST', body: JSON.stringify(body) })).json();
  } catch (e) { return showProblems(['The station did not answer — is it still running?'], []); }
  if (!d.ok) return showProblems(d.problems || [], d.notes || []);
  const notes = d.notes || [];
  showProblems([], notes);
  const done = document.getElementById('done');
  done.hidden = false;
  done.textContent = 'Configuration written — the station is starting…';
  document.getElementById('go').disabled = true;
  // With notes worth reading, wait for a click instead of yanking the
  // page away; otherwise go to the station page as soon as it answers.
  const poll = setInterval(async () => {
    try {
      const s = await (await fetch('/status.json')).json();
      if (!s.station) return;
      clearInterval(poll);
      if (notes.length) {
        done.innerHTML = '';
        done.append('The station is up. Read the notes above, then: ');
        const a = document.createElement('a');
        a.href = '/'; a.textContent = 'open its page';
        done.append(a);
      } else {
        location.href = '/';
      }
    } catch (e) {}
  }, 2000);
};

function showProblems(problems, notes) {
  document.getElementById('problems').innerHTML = problems.map(p => '<div></div>').join('');
  document.querySelectorAll('#problems div').forEach((el, i) => el.textContent = problems[i]);
  document.getElementById('notes').innerHTML = notes.map(p => '<div></div>').join('');
  document.querySelectorAll('#notes div').forEach((el, i) => el.textContent = notes[i]);
}
</script>
"##;
