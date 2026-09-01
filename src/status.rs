//! The status surface: one JSON document answering the feeder's real
//! questions. Served over plain HTTP with no framework; the page comes
//! later, the data comes first.

use crate::supervise::StatusMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct StatusServer {
    pub station_name: String,
    pub started_unix: u64,
    pub children: StatusMap,
    pub stats_dir: PathBuf,
    pub feeds: Vec<String>,
}

impl StatusServer {
    pub async fn run(self, listen: String) -> anyhow::Result<()> {
        let l = TcpListener::bind(&listen).await?;
        println!("stationd: status on http://{listen}/status.json");
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                continue;
            };
            let body = self.render();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await; // request; any GET gets the status
                let resp = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    }

    fn render(&self) -> String {
        let children = self.children.lock().unwrap().clone();
        // Per-server MLAT stats files, written by mlatc from the servers'
        // stats pushes.
        let mut mlat = serde_json::Map::new();
        if let Ok(dir) = std::fs::read_dir(&self.stats_dir) {
            for e in dir.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("mlat-stats") && name.ends_with(".json") {
                    if let Ok(v) = std::fs::read_to_string(e.path()) {
                        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&v) {
                            mlat.insert(name, j);
                        }
                    }
                }
            }
        }
        serde_json::json!({
            "station": self.station_name,
            "started_unix": self.started_unix,
            "children": children,
            "feeds": self.feeds,
            "mlat": mlat,
        })
        .to_string()
    }
}
