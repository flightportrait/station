//! The station's numbers: a reading of readsb's JSON, a two-day ring of
//! per-minute samples, and the identity aggregates (today vs yesterday,
//! farthest heard, busiest hour). Persisted so a restart keeps the story.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// One reading of readsb's aircraft.json.
pub struct Snapshot {
    pub json_age_s: f64,
    pub aircraft: u32,
    pub with_position: u32,
    pub messages_total: u64,
    /// (icao, km) of the farthest aircraft with a position right now.
    pub farthest: Option<(String, f64)>,
    /// Hex addresses currently seen, for the daily distinct count.
    pub hexes: Vec<String>,
}

pub fn read_snapshot(dir: &Path, lat: f64, lon: f64) -> Option<Snapshot> {
    let path = dir.join("aircraft.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let now = v.get("now")?.as_f64()?;
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    let list = v.get("aircraft")?.as_array()?;
    let mut with_position = 0u32;
    let mut farthest: Option<(String, f64)> = None;
    let mut hexes = Vec::with_capacity(list.len());
    for a in list {
        if let Some(h) = a.get("hex").and_then(|h| h.as_str()) {
            hexes.push(h.to_string());
        }
        let (Some(alat), Some(alon)) = (
            a.get("lat").and_then(|x| x.as_f64()),
            a.get("lon").and_then(|x| x.as_f64()),
        ) else {
            continue;
        };
        with_position += 1;
        let km = haversine_km(lat, lon, alat, alon);
        if farthest.as_ref().is_none_or(|(_, best)| km > *best) {
            let hex = a
                .get("hex")
                .and_then(|h| h.as_str())
                .unwrap_or("?")
                .to_string();
            farthest = Some((hex, km));
        }
    }
    Some(Snapshot {
        json_age_s: (wall - now).max(0.0),
        aircraft: list.len() as u32,
        with_position,
        messages_total: v.get("messages").and_then(|m| m.as_u64()).unwrap_or(0),
        farthest,
        hexes,
    })
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Sample {
    pub unix: u64,
    pub msgs_per_min: u32,
    pub aircraft: u32,
    pub max_range_km: f32,
}

/// Everything persisted between restarts.
#[derive(Serialize, Deserialize, Default)]
pub struct Metrics {
    pub ring: Vec<Sample>,
    #[serde(skip)]
    last_messages_total: Option<u64>,
    pub day: u64,
    pub seen_today: HashSet<String>,
    pub seen_yesterday: usize,
    pub farthest_today: Option<(String, f64)>,
}

const RING_MAX: usize = 48 * 60;

impl Metrics {
    pub fn load(path: &Path) -> Metrics {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        let tmp = path.with_extension("tmp");
        if let Ok(f) = std::fs::File::create(&tmp) {
            if serde_json::to_writer(std::io::BufWriter::new(f), self).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Feed one snapshot; call about once a minute.
    pub fn tick(&mut self, unix: u64, snap: &Snapshot, hexes: impl Iterator<Item = String>) {
        let day = unix / 86400;
        if day != self.day {
            self.seen_yesterday = self.seen_today.len();
            self.seen_today.clear();
            self.farthest_today = None;
            self.day = day;
        }
        for h in hexes {
            self.seen_today.insert(h);
        }
        if let Some((hex, km)) = &snap.farthest {
            if self.farthest_today.as_ref().is_none_or(|(_, b)| km > b) {
                self.farthest_today = Some((hex.clone(), *km));
            }
        }
        let msgs = match self.last_messages_total {
            Some(prev) if snap.messages_total >= prev => (snap.messages_total - prev) as u32,
            _ => 0,
        };
        self.last_messages_total = Some(snap.messages_total);
        self.ring.push(Sample {
            unix,
            msgs_per_min: msgs,
            aircraft: snap.aircraft,
            max_range_km: snap
                .farthest
                .as_ref()
                .map(|(_, km)| *km as f32)
                .unwrap_or(0.0),
        });
        let excess = self.ring.len().saturating_sub(RING_MAX);
        if excess > 0 {
            self.ring.drain(..excess);
        }
    }

    /// Mean message rate over the last `mins` minutes, if that much exists.
    pub fn recent_rate(&self, mins: usize) -> Option<f64> {
        if self.ring.len() < mins || mins == 0 {
            return None;
        }
        let tail = &self.ring[self.ring.len() - mins..];
        Some(tail.iter().map(|s| s.msgs_per_min as f64).sum::<f64>() / mins as f64)
    }

    /// Median per-minute rate over the whole ring, as the station's own
    /// baseline. Meaningful once hours of history exist.
    pub fn baseline_rate(&self) -> Option<f64> {
        if self.ring.len() < 360 {
            return None;
        }
        let mut v: Vec<u32> = self.ring.iter().map(|s| s.msgs_per_min).collect();
        v.sort_unstable();
        Some(v[v.len() / 2] as f64)
    }

    /// (hour-of-day, messages) of today's busiest hour so far.
    pub fn busiest_hour(&self) -> Option<(u32, u64)> {
        let mut per_hour = [0u64; 24];
        let mut any = false;
        for s in &self.ring {
            if s.unix / 86400 == self.day {
                per_hour[((s.unix % 86400) / 3600) as usize] += s.msgs_per_min as u64;
                any = true;
            }
        }
        if !any {
            return None;
        }
        let (h, m) = per_hour
            .iter()
            .enumerate()
            .max_by_key(|(_, m)| **m)
            .expect("24 entries");
        Some((h as u32, *m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(msgs: u64, far: f64) -> Snapshot {
        Snapshot {
            json_age_s: 1.0,
            aircraft: 3,
            with_position: 3,
            messages_total: msgs,
            farthest: Some(("abc123".into(), far)),
            hexes: Vec::new(),
        }
    }

    #[test]
    fn rates_are_deltas_and_baseline_needs_history() {
        let mut m = Metrics::default();
        for i in 0..10u64 {
            m.tick(1000 + i * 60, &snap(i * 600, 100.0), std::iter::empty());
        }
        assert_eq!(m.recent_rate(5), Some(600.0));
        assert!(m.baseline_rate().is_none(), "6 h before a baseline exists");
    }

    #[test]
    fn day_rollover_moves_today_to_yesterday() {
        let mut m = Metrics::default();
        m.tick(
            86400 * 10,
            &snap(0, 50.0),
            ["a".into(), "b".into()].into_iter(),
        );
        assert_eq!(m.seen_today.len(), 2);
        m.tick(86400 * 11, &snap(10, 80.0), ["c".into()].into_iter());
        assert_eq!(m.seen_yesterday, 2);
        assert_eq!(m.seen_today.len(), 1);
        assert_eq!(m.farthest_today.as_ref().unwrap().1, 80.0);
    }

    #[test]
    fn ring_is_bounded() {
        let mut m = Metrics::default();
        for i in 0..(RING_MAX as u64 + 100) {
            m.tick(i * 60, &snap(i * 10, 10.0), std::iter::empty());
        }
        assert_eq!(m.ring.len(), RING_MAX);
    }
}
