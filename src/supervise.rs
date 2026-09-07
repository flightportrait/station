//! Child supervision: spawn, watch, restart with backoff, report.
//!
//! Children write their output through stationd with a name prefix, so
//! one journal tells the whole station's story. A child that stays up
//! for a minute earns its backoff reset.
//!
//! The radio slot has a fallback: when the Station radio (rx) keeps
//! crashing or hears nothing for a quarter hour, readsb takes its place
//! with the same arguments, and the station keeps feeding. A crash
//! fallback is retried once an hour; a silence fallback is not, since
//! readsb hearing aircraft where the radio did not is the evidence the
//! radio is at fault.

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

/// Why the station fell back from its radio to readsb.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FallbackReason {
    Crashes,
    Silence,
}

/// The fallback decision, as a pure function of what happened.
/// `exits_10min` is how many times the radio exited in the last ten
/// minutes; `silent_s` how long the receiver JSON has shown no new
/// messages while the radio was up.
pub fn fallback_reason(exits_10min: usize, silent_s: Option<f64>) -> Option<FallbackReason> {
    if exits_10min >= 3 {
        return Some(FallbackReason::Crashes);
    }
    if silent_s.is_some_and(|s| s >= 900.0) {
        return Some(FallbackReason::Silence);
    }
    None
}

/// Whether to try the radio again, `since_fallback_s` after falling back.
pub fn may_retry(reason: FallbackReason, since_fallback_s: f64) -> bool {
    reason == FallbackReason::Crashes && since_fallback_s >= 3600.0
}

/// What runs in the radio slot right now, and why.
#[derive(Clone, serde::Serialize)]
pub struct RadioState {
    /// "radio" or "readsb".
    pub running: &'static str,
    pub fallback: Option<FallbackReason>,
    pub fell_back_unix: Option<u64>,
}

pub type RadioShared = Arc<Mutex<RadioState>>;

/// Progress of the receiver JSON, fed by the status sampler: the moment
/// the message counter last grew.
pub struct RadioHealth {
    pub last_progress: Mutex<Instant>,
}

impl RadioHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(RadioHealth {
            last_progress: Mutex::new(Instant::now()),
        })
    }
    pub fn progressed(&self) {
        *self.last_progress.lock().unwrap() = Instant::now();
    }
    pub fn silent_s(&self) -> f64 {
        self.last_progress.lock().unwrap().elapsed().as_secs_f64()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

enum Outcome {
    /// The child exited; how long it ran.
    Exited(Duration),
    /// The silence watch fired while the child was up (child killed).
    Silent,
    Shutdown,
}

/// Run one child to completion, or until shutdown, or until the silence
/// watch (radio only) fires. Status and journal are kept as it goes.
async fn run_child(
    spec: &ChildSpec,
    statuses: &StatusMap,
    shutdown: &mut watch::Receiver<bool>,
    silence: Option<(&Arc<RadioHealth>, f64)>,
) -> Outcome {
    let started = Instant::now();
    let mut cmd = Command::new(&spec.cmd);
    cmd.args(&spec.args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("stationd: [{}] cannot start {}: {e}", spec.name, spec.cmd);
            set(statuses, &spec.name, |st| {
                st.state = "backoff".into();
                st.restarts += 1;
                st.last_exit = Some(e.to_string());
            });
            return Outcome::Exited(Duration::ZERO);
        }
    };
    set(statuses, &spec.name, |st| {
        st.state = "up".into();
        st.pid = child.id();
        st.up_since_unix = Some(unix_now());
    });
    if let Some(h) = silence.map(|(h, _)| h) {
        h.progressed();
    }
    if let Some(out) = child.stdout.take() {
        prefix_pipe(spec.name.clone(), out);
    }
    if let Some(err) = child.stderr.take() {
        prefix_pipe(spec.name.clone(), err);
    }
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.tick().await;
    let exit = loop {
        tokio::select! {
            r = child.wait() => break r,
            _ = shutdown.changed() => {
                let _ = child.kill().await;
                set(statuses, &spec.name, |st| {
                    st.state = "stopped".into();
                    st.pid = None;
                });
                return Outcome::Shutdown;
            }
            _ = tick.tick() => {
                if let Some((h, limit)) = silence {
                    if h.silent_s() >= limit {
                        let _ = child.kill().await;
                        set(statuses, &spec.name, |st| {
                            st.state = "stopped".into();
                            st.pid = None;
                            st.up_since_unix = None;
                            st.last_exit = Some("stopped: heard nothing".into());
                        });
                        return Outcome::Silent;
                    }
                }
            }
        }
    };
    let desc = match exit {
        Ok(s) => s.to_string(),
        Err(e) => format!("wait failed: {e}"),
    };
    println!("stationd: [{}] exited: {desc}", spec.name);
    set(statuses, &spec.name, |st| {
        st.state = "backoff".into();
        st.pid = None;
        st.up_since_unix = None;
        st.restarts += 1;
        st.last_exit = Some(desc.clone());
    });
    Outcome::Exited(started.elapsed())
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
            match run_child(&spec, &statuses, &mut shutdown, None).await {
                Outcome::Shutdown => return,
                Outcome::Silent => {}
                Outcome::Exited(ran) => {
                    if ran > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
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

/// Supervise the radio slot: `radio` with `readsb` as its fallback. With
/// no fallback this is `spawn` under another name.
pub fn spawn_radio(
    radio: ChildSpec,
    fallback: Option<ChildSpec>,
    statuses: StatusMap,
    mut shutdown: watch::Receiver<bool>,
    health: Arc<RadioHealth>,
    state: RadioShared,
) {
    statuses
        .lock()
        .unwrap()
        .insert(radio.name.clone(), ChildStatus::default());
    if let Some(f) = &fallback {
        statuses
            .lock()
            .unwrap()
            .insert(f.name.clone(), ChildStatus::default());
    }
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        let mut exits: Vec<Instant> = Vec::new();
        loop {
            if *shutdown.borrow() {
                return;
            }
            // The radio, watched for silence only when a fallback exists.
            let silence = fallback.as_ref().map(|_| (&health, 900.0));
            let outcome = run_child(&radio, &statuses, &mut shutdown, silence).await;
            let reason = match outcome {
                Outcome::Shutdown => return,
                Outcome::Silent => Some(FallbackReason::Silence),
                Outcome::Exited(ran) => {
                    if ran > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    let now = Instant::now();
                    exits.push(now);
                    exits.retain(|t| now.duration_since(*t) < Duration::from_secs(600));
                    if fallback.is_some() {
                        fallback_reason(exits.len(), None)
                    } else {
                        None
                    }
                }
            };
            if let (Some(reason), Some(fb)) = (reason, fallback.as_ref()) {
                let why = match reason {
                    FallbackReason::Crashes => "keeps stopping",
                    FallbackReason::Silence => "heard nothing for 15 minutes",
                };
                println!("stationd: the radio {why}; running readsb instead");
                let fell_back = Instant::now();
                *state.lock().unwrap() = RadioState {
                    running: "readsb",
                    fallback: Some(reason),
                    fell_back_unix: Some(unix_now()),
                };
                exits.clear();
                let mut fb_backoff = Duration::from_secs(1);
                loop {
                    if *shutdown.borrow() {
                        return;
                    }
                    let retry_at = fell_back + Duration::from_secs(3600);
                    let ran_fb = tokio::select! {
                        o = run_child(fb, &statuses, &mut shutdown, None) => o,
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retry_at)),
                            if may_retry(reason, 3600.0) => Outcome::Silent,
                    };
                    match ran_fb {
                        Outcome::Shutdown => return,
                        Outcome::Silent => break, // the retry hour is up
                        Outcome::Exited(ran) => {
                            if ran > Duration::from_secs(60) {
                                fb_backoff = Duration::from_secs(1);
                            }
                        }
                    }
                    if may_retry(reason, fell_back.elapsed().as_secs_f64()) {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(fb_backoff) => {}
                        _ = shutdown.changed() => return,
                    }
                    fb_backoff = (fb_backoff * 2).min(Duration::from_secs(30));
                }
                println!("stationd: trying the radio again");
                *state.lock().unwrap() = RadioState {
                    running: "radio",
                    fallback: None,
                    fell_back_unix: None,
                };
                backoff = Duration::from_secs(1);
                continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_crashes_in_ten_minutes_fall_back() {
        assert_eq!(fallback_reason(2, None), None);
        assert_eq!(fallback_reason(3, None), Some(FallbackReason::Crashes));
    }

    #[test]
    fn a_quarter_hour_of_silence_falls_back() {
        assert_eq!(fallback_reason(0, Some(899.0)), None);
        assert_eq!(
            fallback_reason(0, Some(900.0)),
            Some(FallbackReason::Silence)
        );
    }

    #[test]
    fn crashes_win_over_silence_in_the_reason() {
        assert_eq!(
            fallback_reason(3, Some(1000.0)),
            Some(FallbackReason::Crashes)
        );
    }

    #[test]
    fn only_crash_fallbacks_are_retried_and_only_hourly() {
        assert!(!may_retry(FallbackReason::Crashes, 3599.0));
        assert!(may_retry(FallbackReason::Crashes, 3600.0));
        assert!(!may_retry(FallbackReason::Silence, 86400.0));
    }
}
