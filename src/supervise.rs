//! Child supervision: spawn, watch, restart with backoff, report.
//!
//! Children write their output through stationd with a name prefix, so
//! one journal tells the whole station's story. A child that stays up
//! for a minute earns its backoff reset.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

pub struct ChildSpec {
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct ChildStatus {
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u64,
    pub up_since_unix: Option<u64>,
    pub last_exit: Option<String>,
}

pub type StatusMap = Arc<Mutex<HashMap<String, ChildStatus>>>;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Supervise one child until shutdown flips. Restart backoff doubles from
/// 1 s to 30 s and resets after a minute of healthy uptime.
pub fn spawn(spec: ChildSpec, statuses: StatusMap, mut shutdown: watch::Receiver<bool>) {
    statuses
        .lock()
        .unwrap()
        .insert(spec.name.clone(), ChildStatus::default());
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            if *shutdown.borrow() {
                return;
            }
            let started = Instant::now();
            let mut cmd = Command::new(&spec.cmd);
            cmd.args(&spec.args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            match cmd.spawn() {
                Ok(mut child) => {
                    set(&statuses, &spec.name, |st| {
                        st.state = "up".into();
                        st.pid = child.id();
                        st.up_since_unix = Some(unix_now());
                    });
                    if let Some(out) = child.stdout.take() {
                        prefix_pipe(spec.name.clone(), out);
                    }
                    if let Some(err) = child.stderr.take() {
                        prefix_pipe(spec.name.clone(), err);
                    }
                    let exit = tokio::select! {
                        r = child.wait() => r,
                        _ = shutdown.changed() => {
                            let _ = child.kill().await;
                            set(&statuses, &spec.name, |st| {
                                st.state = "stopped".into();
                                st.pid = None;
                            });
                            return;
                        }
                    };
                    let desc = match exit {
                        Ok(s) => s.to_string(),
                        Err(e) => format!("wait failed: {e}"),
                    };
                    println!("stationd: [{}] exited: {desc}", spec.name);
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    set(&statuses, &spec.name, |st| {
                        st.state = "backoff".into();
                        st.pid = None;
                        st.up_since_unix = None;
                        st.restarts += 1;
                        st.last_exit = Some(desc);
                    });
                }
                Err(e) => {
                    println!("stationd: [{}] cannot start {}: {e}", spec.name, spec.cmd);
                    set(&statuses, &spec.name, |st| {
                        st.state = "backoff".into();
                        st.restarts += 1;
                        st.last_exit = Some(e.to_string());
                    });
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.changed() => return,
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

fn set(map: &StatusMap, name: &str, f: impl FnOnce(&mut ChildStatus)) {
    if let Some(st) = map.lock().unwrap().get_mut(name) {
        f(st);
    }
}

fn prefix_pipe(name: String, stream: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            println!("[{name}] {l}");
        }
    });
}
