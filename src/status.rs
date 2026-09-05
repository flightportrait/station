//! The status surface: one JSON document and one page reading it.
//!
//! The page shows data; sentences appear only when a diagnostic fires.
//! Served over plain HTTP with no framework and no external assets.

use crate::diagnose::{diagnose, FeedView, View};
use crate::model::{Metrics, Snapshot};
use crate::supervise::StatusMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Shared {
    pub metrics: Mutex<Metrics>,
    pub snapshot: Mutex<Option<(Instant, Snapshot)>>,
}

pub struct StatusServer {
    pub station_name: String,
    pub started_unix: u64,
    pub children: StatusMap,
    pub stats_dir: PathBuf,
    /// Every configured feed: name, ADS-B destination, MLAT stats file.
    pub feeds: Vec<FeedSpec>,
    pub readsb_configured: bool,
    pub shared: Arc<Shared>,
    /// What runs in the radio slot, when stationd runs a radio with a fallback.
    pub radio: Option<crate::supervise::RadioShared>,
}

pub struct FeedSpec {
    pub name: String,
    pub adsb: Option<String>,
    pub mlat_stats_file: Option<String>,
}

/// Whether some process on this machine holds an established TCP
/// connection to `addr` (host:port): readsb's connector to an aggregator,
/// read from /proc/net/tcp and tcp6. Resolution is cached ten minutes.
fn adsb_connected(addr: &str) -> Option<bool> {
    use std::collections::HashMap;
    use std::net::{SocketAddr, ToSocketAddrs};
    static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<SocketAddr>)>>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap();
    let cache = c.get_or_insert_with(HashMap::new);
    let fresh = cache
        .get(addr)
        .filter(|(t, _)| t.elapsed().as_secs() < 600)
        .map(|(_, v)| v.clone());
    let targets = match fresh {
        Some(v) => v,
        None => {
            let v: Vec<SocketAddr> = addr.to_socket_addrs().map(|i| i.collect()).unwrap_or_default();
            cache.insert(addr.to_string(), (Instant::now(), v.clone()));
            v
        }
    };
    if targets.is_empty() {
        return Some(false);
    }
    let established = established_remotes();
    Some(targets.iter().any(|t| established.contains(t)))
}

/// Remote ends of every established TCP connection on this machine.
fn established_remotes() -> Vec<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let mut out = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || f[3] != "01" {
                continue;
            }
            let Some((h, p)) = f[2].split_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(p, 16) else { continue };
            let ip = if v6 {
                if h.len() != 32 {
                    continue;
                }
                let mut b = [0u8; 16];
                for (i, chunk) in h.as_bytes().chunks(8).enumerate() {
                    let Ok(w) = u32::from_str_radix(std::str::from_utf8(chunk).unwrap_or("x"), 16) else { continue };
                    b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
                let ip6 = Ipv6Addr::from(b);
                match ip6.to_ipv4_mapped() {
                    Some(v4) => IpAddr::V4(v4),
                    None => IpAddr::V6(ip6),
                }
            } else {
                let Ok(w) = u32::from_str_radix(h, 16) else { continue };
                IpAddr::V4(Ipv4Addr::from(w.to_le_bytes()))
            };
            out.push(SocketAddr::new(ip, port));
        }
    }
    out
}

/// The stats file name mlatc uses for a server address; must mirror
/// mlatc's own naming.
pub fn stats_file_for(addr: &str, n_feeds: usize) -> String {
    if n_feeds == 1 {
        return "mlat-stats.json".into();
    }
    let tag: String = addr
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("mlat-stats-{tag}.json")
}

impl StatusServer {
    pub async fn run(self, listen: String) -> anyhow::Result<()> {
        let l = TcpListener::bind(&listen).await?;
        println!("stationd: status on http://{listen}/");
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                continue;
            };
            let json = self.render();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let wants_page = req.starts_with("GET / ");
                let (ctype, body) = if wants_page {
                    ("text/html; charset=utf-8", PAGE.to_string())
                } else {
                    ("application/json", json)
                };
                let resp = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    }

    fn feed_views(&self) -> Vec<FeedView> {
        self.feeds
            .iter()
            .map(|spec| {
                let name = &spec.name;
                let path = spec.mlat_stats_file.as_ref().map(|f| self.stats_dir.join(f));
                let age = path
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs_f64());
                let v: Option<serde_json::Value> = path
                    .as_ref()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|t| serde_json::from_str(&t).ok());
                let get = |k: &str| {
                    v.as_ref()
                        .and_then(|j| j.get(k))
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0)
                };
                FeedView {
                    name: name.clone(),
                    stats_age_s: age,
                    bad_sync: get("bad_sync_timeout") > 0.0,
                    clock_resets: get("clock_resets") as u64,
                    adsb_connected: spec.adsb.as_deref().and_then(adsb_connected),
                    has_mlat: spec.mlat_stats_file.is_some(),
                }
            })
            .collect()
    }

    fn render(&self) -> String {
        let children = self.children.lock().unwrap().clone();
        let failing: Vec<(String, u64)> = children
            .iter()
            .filter(|(_, st)| st.state == "backoff" && st.restarts >= 3)
            .map(|(n, st)| (n.clone(), st.restarts))
            .collect();
        let snap = self.shared.snapshot.lock().unwrap();
        let (readsb_age, aircraft_now, with_position, farthest_now) = match &*snap {
            Some((read_at, s)) => (
                Some(s.json_age_s + read_at.elapsed().as_secs_f64()),
                s.aircraft,
                s.with_position,
                s.farthest.clone(),
            ),
            None => (None, 0, 0, None),
        };
        drop(snap);
        let m = self.shared.metrics.lock().unwrap();
        let feeds = self.feed_views();
        let radio = self.radio.as_ref().map(|r| r.lock().unwrap().clone());
        let view = View {
            readsb_configured: self.readsb_configured,
            readsb_age_s: readsb_age,
            aircraft_now,
            rate_now: m.recent_rate(10),
            rate_baseline: m.baseline_rate(),
            feeds,
            failing_children: failing,
            radio_fallback: radio.as_ref().and_then(|r| r.fallback),
        };
        let diagnostics = diagnose(&view);
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        serde_json::json!({
            "station": self.station_name,
            "now_unix": unix,
            "started_unix": self.started_unix,
            "children": children,
            "radio": radio,
            "receiver": {
                "configured": self.readsb_configured,
                "json_age_s": view.readsb_age_s,
                "aircraft": aircraft_now,
                "with_position": with_position,
                "messages_per_min": m.recent_rate(1),
                "farthest_now": farthest_now,
            },
            "today": {
                "aircraft": m.seen_today.len(),
                "aircraft_yesterday": m.seen_yesterday,
                "farthest": m.farthest_today,
                "busiest_hour": m.busiest_hour(),
            },
            "feeds": view.feeds.iter().map(|f| serde_json::json!({
                "name": f.name,
                "stats_age_s": f.stats_age_s,
                "bad_sync": f.bad_sync,
                "clock_resets": f.clock_resets,
                "mlat": f.has_mlat,
                "adsb_connected": f.adsb_connected,
            })).collect::<Vec<_>>(),
            "history_minutes": m.ring.len(),
            "diagnostics": diagnostics,
        })
        .to_string()
    }
}

/// The page: structure now, art direction later. Self-contained, reads
/// /status.json, shows sentences only when diagnostics exist.
const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Station</title>
<style>
  body { font: 16px/1.45 system-ui, sans-serif; margin: 2rem auto; max-width: 44rem; padding: 0 1rem; }
  h1 { font-size: 1.2rem; margin: 0 0 1.5rem; }
  .nums { display: grid; grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr)); gap: 1rem; }
  .num b { display: block; font-size: 1.6rem; }
  .num span { color: #666; font-size: .85rem; }
  table { border-collapse: collapse; margin-top: 1.5rem; width: 100%; }
  td, th { text-align: left; padding: .3rem .6rem .3rem 0; border-bottom: 1px solid #ddd; font-size: .95rem; }
  #diag { margin-top: 1.5rem; }
  #diag div { border-left: 3px solid #c33; padding: .4rem .8rem; margin: .5rem 0; background: #fee; }
  #diag div.warning { border-color: #c90; background: #fec; }
  #diag .act { color: #555; font-size: .9rem; }
  footer { margin-top: 2rem; color: #999; font-size: .8rem; }
</style>
<h1 id="name">station</h1>
<div class="nums">
  <div class="num"><b id="aircraft">–</b><span>aircraft now</span></div>
  <div class="num"><b id="rate">–</b><span>messages / min</span></div>
  <div class="num"><b id="today">–</b><span id="todaylbl">aircraft today</span></div>
  <div class="num"><b id="far">–</b><span id="farlbl">farthest today</span></div>
</div>
<table id="feeds"></table>
<div id="diag"></div>
<footer id="foot"></footer>
<script>
async function tick() {
  let d;
  try { d = await (await fetch('/status.json')).json(); } catch { return; }
  document.getElementById('name').textContent = d.station;
  document.title = d.station;
  const set = (id, v) => document.getElementById(id).textContent = v;
  set('aircraft', d.receiver.aircraft ?? '–');
  set('rate', d.receiver.messages_per_min != null ? Math.round(d.receiver.messages_per_min) : '–');
  set('today', d.today.aircraft || '–');
  if (d.today.aircraft_yesterday)
    set('todaylbl', 'aircraft today · ' + d.today.aircraft_yesterday + ' yesterday');
  if (d.today.farthest)
    set('far', Math.round(d.today.farthest[1]) + ' km');
  const rows = [['feed', 'ADS-B', 'MLAT']];
  for (const f of d.feeds) {
    const adsb = f.adsb_connected == null ? '–' : (f.adsb_connected ? 'connected' : 'not connected');
    const mlat = f.stats_age_s == null ? (f.mlat ? 'no sync yet' : '–')
      : (f.bad_sync ? 'rejected' : 'good') + ' · ' + Math.round(f.stats_age_s) + ' s ago';
    rows.push([f.name, adsb, mlat]);
  }
  document.getElementById('feeds').innerHTML =
    rows.map((r, i) => '<tr>' + r.map(c => i ? '<td>'+c+'</td>' : '<th>'+c+'</th>').join('') + '</tr>').join('');
  document.getElementById('diag').innerHTML = (d.diagnostics || []).map(x =>
    '<div class="'+x.severity+'"><div>'+x.sentence+'</div><div class="act">'+x.action+'</div></div>').join('');
  const up = Math.floor((d.now_unix - d.started_unix) / 60);
  document.getElementById('foot').textContent = 'up ' + up + ' min · ' + d.history_minutes + ' min of history';
}
tick(); setInterval(tick, 5000);
</script>
"#;
