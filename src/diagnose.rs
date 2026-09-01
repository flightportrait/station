//! The diagnostics rules: machine conditions in, sentences out.
//!
//! A healthy station produces an empty list — the page then shows data
//! and no prose. Every rule pairs one condition with one sentence and
//! one action a person can take. The MLAT rules use the per-server
//! stats push for triangulation: one server complaining is that
//! server's problem; every server complaining is this station's.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Problem,
    Warning,
}

#[derive(Serialize, Debug)]
pub struct Diagnostic {
    pub id: &'static str,
    pub severity: Severity,
    pub sentence: String,
    pub action: String,
}

/// Everything the rules look at, assembled by the status task.
pub struct View {
    pub readsb_configured: bool,
    /// Age of aircraft.json's own clock, seconds.
    pub readsb_age_s: Option<f64>,
    pub aircraft_now: u32,
    pub rate_now: Option<f64>,
    pub rate_baseline: Option<f64>,
    pub feeds: Vec<FeedView>,
    /// (child name, restarts) for children currently in backoff.
    pub failing_children: Vec<(String, u64)>,
}

pub struct FeedView {
    pub name: String,
    /// Age of this feed's stats file; None if it never appeared.
    pub stats_age_s: Option<f64>,
    pub bad_sync: bool,
    pub clock_resets: u64,
}

pub fn diagnose(v: &View) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for (name, restarts) in &v.failing_children {
        out.push(Diagnostic {
            id: "child-failing",
            severity: Severity::Problem,
            sentence: format!("{name} keeps stopping ({restarts} restarts)."),
            action: "Read its lines in the stationd journal; the last one \
                     before each exit usually names the cause."
                .into(),
        });
    }

    if v.readsb_configured {
        match v.readsb_age_s {
            Some(age) if age > 60.0 => out.push(Diagnostic {
                id: "input-stale",
                severity: Severity::Problem,
                sentence: format!("No data from the receiver for {:.0} seconds.", age),
                action: "Check the SDR dongle's USB connection and that readsb runs.".into(),
            }),
            Some(_) if v.aircraft_now == 0 && v.rate_now.is_some_and(|r| r < 1.0) => {
                out.push(Diagnostic {
                    id: "deaf",
                    severity: Severity::Warning,
                    sentence: "The radio is running but hears nothing.".into(),
                    action: "Check the antenna connection; move the antenna \
                             toward a window or higher."
                        .into(),
                })
            }
            _ => {}
        }
        if let (Some(now), Some(base)) = (v.rate_now, v.rate_baseline) {
            if base >= 60.0 && now < base * 0.25 {
                out.push(Diagnostic {
                    id: "rate-collapse",
                    severity: Severity::Warning,
                    sentence: format!(
                        "Hearing {:.0} messages a minute where {:.0} is normal \
                         for this station.",
                        now, base
                    ),
                    action: "Did the antenna move, or a cable loosen? Compare \
                             with the last place it worked."
                        .into(),
                });
            }
        }
    }

    let fresh: Vec<&FeedView> = v
        .feeds
        .iter()
        .filter(|f| f.stats_age_s.is_some_and(|a| a < 60.0))
        .collect();
    if v.feeds.len() >= 2 && !fresh.is_empty() && fresh.iter().all(|f| f.bad_sync) {
        out.push(Diagnostic {
            id: "position-suspect",
            severity: Severity::Problem,
            sentence: "Every MLAT server rejects this station's timing.".into(),
            action: "The configured position or altitude is probably wrong; \
                     a server-side problem would not affect all of them at once."
                .into(),
        });
    }
    if !fresh.is_empty() {
        for f in &v.feeds {
            if f.stats_age_s.is_none_or(|a| a > 120.0) {
                out.push(Diagnostic {
                    id: "feed-unreachable",
                    severity: Severity::Warning,
                    sentence: format!("{} is not answering.", f.name),
                    action: "Their side or your network; the other feeds are \
                             fine, so this station is."
                        .into(),
                });
            }
        }
    }
    let resets: u64 = v.feeds.iter().map(|f| f.clock_resets).max().unwrap_or(0);
    if resets >= 5 {
        out.push(Diagnostic {
            id: "clock-trouble",
            severity: Severity::Warning,
            sentence: format!("The receiver's clock reset {resets} times this session."),
            action: "Usually USB power or dongle heat: try a powered hub or \
                     a cooler spot."
                .into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> View {
        View {
            readsb_configured: true,
            readsb_age_s: Some(2.0),
            aircraft_now: 12,
            rate_now: Some(500.0),
            rate_baseline: Some(520.0),
            feeds: vec![
                FeedView {
                    name: "a".into(),
                    stats_age_s: Some(10.0),
                    bad_sync: false,
                    clock_resets: 0,
                },
                FeedView {
                    name: "b".into(),
                    stats_age_s: Some(12.0),
                    bad_sync: false,
                    clock_resets: 0,
                },
            ],
            failing_children: vec![],
        }
    }

    #[test]
    fn healthy_station_says_nothing() {
        assert!(diagnose(&healthy()).is_empty());
    }

    #[test]
    fn stale_input_fires() {
        let mut v = healthy();
        v.readsb_age_s = Some(300.0);
        let d = diagnose(&v);
        assert_eq!(d[0].id, "input-stale");
    }

    #[test]
    fn all_servers_bad_means_this_station() {
        let mut v = healthy();
        for f in &mut v.feeds {
            f.bad_sync = true;
        }
        assert!(diagnose(&v).iter().any(|d| d.id == "position-suspect"));
    }

    #[test]
    fn one_server_bad_is_not_our_position() {
        let mut v = healthy();
        v.feeds[0].bad_sync = true;
        assert!(!diagnose(&v).iter().any(|d| d.id == "position-suspect"));
    }

    #[test]
    fn one_stale_feed_is_named() {
        let mut v = healthy();
        v.feeds[1].stats_age_s = Some(600.0);
        let d = diagnose(&v);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "feed-unreachable");
        assert!(d[0].sentence.contains('b'));
    }

    #[test]
    fn rate_collapse_needs_a_baseline() {
        let mut v = healthy();
        v.rate_now = Some(50.0);
        v.rate_baseline = None;
        assert!(diagnose(&v).is_empty(), "no baseline, no judgement");
        v.rate_baseline = Some(500.0);
        assert!(diagnose(&v).iter().any(|d| d.id == "rate-collapse"));
    }

    #[test]
    fn crashing_child_is_reported() {
        let mut v = healthy();
        v.failing_children = vec![("mlatc".into(), 7)];
        assert!(diagnose(&v).iter().any(|d| d.id == "child-failing"));
    }
}
