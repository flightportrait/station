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
    /// (feed name, expected stats file name).
    pub feeds: Vec<(String, String)>,
    pub readsb_configured: bool,
    pub shared: Arc<Shared>,
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
            .map(|(name, file)| {
                let path = self.stats_dir.join(file);
                let age = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs_f64());
                let v: Option<serde_json::Value> = std::fs::read_to_string(&path)
                    .ok()
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
        let view = View {
            readsb_configured: self.readsb_configured,
            readsb_age_s: readsb_age,
            aircraft_now,
            rate_now: m.recent_rate(10),
            rate_baseline: m.baseline_rate(),
            minutes_of_history: m.ring.len(),
            feeds,
            failing_children: failing,
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
  const rows = [['feed', 'sync', 'last stats']];
  for (const f of d.feeds) {
    const sync = f.stats_age_s == null ? '–' : (f.bad_sync ? 'rejected' : 'good');
    const age = f.stats_age_s == null ? '–' : Math.round(f.stats_age_s) + ' s';
    rows.push([f.name, sync, age]);
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
