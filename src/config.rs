//! Configuration: one file, validated hard. Every problem becomes a
//! sentence a person can act on, and stationd refuses to start until the
//! file is clean. Silent degradation is a bug by definition.

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub station: Station,
    pub input: Input,
    #[serde(default, rename = "feed")]
    pub feeds: Vec<Feed>,
    #[serde(default)]
    pub results: Results,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub programs: Programs,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Station {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub alt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub beast: String,
    /// readsb's JSON output directory (--write-json). Enables aircraft
    /// counts, range, history, and most diagnostics.
    #[serde(default)]
    pub readsb_json: Option<std::path::PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub name: String,
    /// ADS-B destination (Beast over TCP, fed by readsb when stationd runs it).
    #[serde(default)]
    pub adsb: Option<String>,
    /// MLAT server (mlatc protocol).
    #[serde(default)]
    pub mlat: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Results {
    #[serde(default)]
    pub beast_connect: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub listen: String,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            listen: "127.0.0.1:8654".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Programs {
    #[serde(default = "default_mlatc")]
    pub mlatc: String,
    /// readsb command line. With `radio` set it is the fallback; alone it
    /// is the radio.
    #[serde(default)]
    pub readsb: Option<String>,
    /// The Station radio (rx) command line. It takes readsb's flags, so it
    /// runs in readsb's slot with the same feed arguments.
    #[serde(default)]
    pub radio: Option<String>,
}

fn default_mlatc() -> String {
    "mlatc".into()
}

impl Default for Programs {
    fn default() -> Self {
        Programs {
            mlatc: default_mlatc(),
            readsb: None,
            radio: None,
        }
    }
}

/// Altitude with unit suffixes: bare number or "m" = meters, "ft" = feet.
pub fn parse_alt(s: &str) -> Option<f64> {
    let t = s.trim();
    let (num, scale) = if let Some(n) = t.strip_suffix("ft") {
        (n, 0.3048)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 1.0)
    } else {
        (t, 1.0)
    };
    num.trim().parse::<f64>().ok().map(|v| v * scale)
}

impl Config {
    /// Every problem in the file, as sentences. Empty means start.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if self.station.name.trim().is_empty() {
            p.push(
                "station.name is empty: give the station a name; MLAT servers \
                 identify it by this."
                    .into(),
            );
        }
        if !(-90.0..=90.0).contains(&self.station.lat) {
            p.push(format!(
                "station.lat is {}: latitude must be between -90 and 90.",
                self.station.lat
            ));
        }
        if !(-180.0..=180.0).contains(&self.station.lon) {
            p.push(format!(
                "station.lon is {}: longitude must be between -180 and 180.",
                self.station.lon
            ));
        }
        match parse_alt(&self.station.alt) {
            None => p.push(format!(
                "station.alt is \"{}\": write meters (\"65m\") or feet (\"213ft\").",
                self.station.alt
            )),
            Some(a) if !(-1000.0..=10000.0).contains(&a) => p.push(format!(
                "station.alt is {a:.0} m: MLAT servers reject altitudes outside \
                 -1000..10000 m. If the antenna is really there, congratulations; \
                 otherwise fix the value."
            )),
            _ => {}
        }
        if self.station.lat == 0.0 && self.station.lon == 0.0 {
            p.push(
                "station position is 0,0 (the ocean off Africa): set the antenna's \
                 real coordinates; MLAT solves other people's aircraft with them."
                    .into(),
            );
        }
        if !self.input.beast.contains(':') {
            p.push(format!(
                "input.beast is \"{}\": write host:port (a readsb Beast output, \
                 usually port 30005).",
                self.input.beast
            ));
        }
        if self.feeds.is_empty() {
            p.push("no [[feed]] blocks: the station would receive and tell no one.".into());
        }
        for (i, f) in self.feeds.iter().enumerate() {
            if f.name.trim().is_empty() {
                p.push(format!("feed #{} has an empty name.", i + 1));
            }
            if f.adsb.is_none() && f.mlat.is_none() {
                p.push(format!(
                    "feed \"{}\" has neither adsb nor mlat: nothing to send it.",
                    f.name
                ));
            }
            for (key, val) in [("adsb", &f.adsb), ("mlat", &f.mlat)] {
                if let Some(v) = val {
                    if !v.contains(':') {
                        p.push(format!(
                            "feed \"{}\": {key} is \"{v}\": write host:port.",
                            f.name
                        ));
                    }
                }
            }
        }
        if self.feeds.iter().any(|f| f.adsb.is_some()) && !self.runs_radio() {
            p.push(
                "a feed has an adsb destination but stationd does not run a radio \
                 ([programs].radio and [programs].readsb are unset): the ADS-B data \
                 would never be sent. Either let stationd run the radio, or add the \
                 --net-connector to your own readsb and drop the adsb line here."
                    .into(),
            );
        }
        if let Some(r) = &self.programs.radio {
            match r.split_whitespace().next() {
                None => p.push("programs.radio is empty: give the path of the rx binary.".into()),
                Some(path) => match std::fs::metadata(path) {
                    Err(_) => p.push(format!(
                        "programs.radio names \"{path}\", which does not exist: install rx \
                         there, or remove the line to run readsb alone."
                    )),
                    Ok(m) => {
                        use std::os::unix::fs::PermissionsExt;
                        if m.permissions().mode() & 0o111 == 0 {
                            p.push(format!(
                                "programs.radio names \"{path}\", which is not executable: \
                                 chmod +x it."
                            ));
                        }
                    }
                },
            }
        }
        let mlat_feeds = || self.feeds.iter().filter(|f| f.mlat.is_some());
        let with_uuid = mlat_feeds().filter(|f| f.uuid.is_some()).count();
        if with_uuid != 0 && with_uuid != mlat_feeds().count() {
            p.push(
                "some MLAT feeds have a uuid and some do not: give every one a uuid, or none."
                    .into(),
            );
        }
        if let Some(r) = &self.results.beast_connect {
            if !r.contains(':') {
                p.push(format!(
                    "results.beast_connect is \"{r}\": write host:port (a readsb \
                     Beast input, usually port 30104)."
                ));
            }
        }
        if !self.status.listen.contains(':') {
            p.push(format!(
                "status.listen is \"{}\": write host:port.",
                self.status.listen
            ));
        }
        p
    }

    /// Whether stationd runs a radio itself (rx, or readsb).
    pub fn runs_radio(&self) -> bool {
        self.programs.radio.is_some() || self.programs.readsb.is_some()
    }

    /// The mlatc invocation this configuration means.
    pub fn mlatc_args(&self, stats_dir: &std::path::Path) -> Vec<String> {
        let mut a = vec![
            "--input-connect".into(),
            self.input.beast.clone(),
            "--user".into(),
            self.station.name.clone(),
            "--lat".into(),
            self.station.lat.to_string(),
            "--lon".into(),
            self.station.lon.to_string(),
            "--alt".into(),
            self.station.alt.clone(),
            "--stats-json".into(),
            stats_dir.join("mlat-stats.json").display().to_string(),
        ];
        let mlat_feeds: Vec<&Feed> = self.feeds.iter().filter(|f| f.mlat.is_some()).collect();
        for f in &mlat_feeds {
            a.push("--server".into());
            a.push(f.mlat.clone().expect("filtered on mlat"));
        }
        for f in &mlat_feeds {
            if let Some(u) = &f.uuid {
                a.push("--uuid".into());
                a.push(u.clone());
            }
        }
        if let Some(r) = &self.results.beast_connect {
            a.push("--results".into());
            a.push(format!("beast,connect,{r}"));
        }
        a
    }

    /// Extra readsb arguments: one --net-connector per ADS-B feed
    /// (readsb's "host,port,protocol[,uuid=…]" form). Only meaningful
    /// when stationd runs readsb itself.
    pub fn readsb_feed_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        for f in &self.feeds {
            if let Some(dest) = &f.adsb {
                let Some((host, port)) = dest.rsplit_once(':') else {
                    continue;
                };
                let mut c = format!("{host},{port},beast_reduce_plus_out");
                if let Some(u) = &f.uuid {
                    c.push_str(&format!(",uuid={u}"));
                }
                a.push("--net-connector".into());
                a.push(c);
            }
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: &str) -> Config {
        toml::from_str(s).expect("parses")
    }

    const GOOD: &str = r#"
[station]
name = "t"
lat = 1.3
lon = 103.8
alt = "65m"
[input]
beast = "127.0.0.1:30005"
[[feed]]
name = "a"
mlat = "h:31090"
"#;

    #[test]
    fn good_config_has_no_problems() {
        assert!(cfg(GOOD).problems().is_empty());
    }

    #[test]
    fn each_problem_is_a_sentence() {
        let c = cfg(r#"
[station]
name = ""
lat = 99.0
lon = 103.8
alt = "sixty"
[input]
beast = "nonsense"
"#);
        let p = c.problems();
        assert_eq!(p.len(), 5, "{p:?}");
        assert!(p.iter().all(|s| s.len() > 20), "sentences, not codes");
    }

    #[test]
    fn mixed_uuids_are_refused() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"b\"\nmlat = \"h2:31090\"\nuuid = \"x\"\n"
        ));
        assert_eq!(c.problems().len(), 1);
    }

    #[test]
    fn unknown_keys_fail_parse() {
        assert!(toml::from_str::<Config>(&format!("{GOOD}\ntypo_key = 1\n")).is_err());
    }

    #[test]
    fn mlatc_args_carry_only_mlat_feeds() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"b\"\nmlat = \"h2:31090\"\n\n[[feed]]\nname = \"c\"\nadsb = \"h3:30004\"\n"
        ));
        let a = c.mlatc_args(std::path::Path::new("/tmp"));
        assert_eq!(a.iter().filter(|x| *x == "--server").count(), 2);
    }

    #[test]
    fn adsb_only_feed_is_valid() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"fp\"\nadsb = \"feed.example:30004\"\n\n[programs]\nreadsb = \"readsb --quiet\"\n"
        ));
        assert!(c.problems().is_empty(), "{:?}", c.problems());
    }

    #[test]
    fn adsb_feed_without_local_readsb_is_refused() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"fp\"\nadsb = \"feed.example:30004\"\n"
        ));
        assert_eq!(c.problems().len(), 1, "{:?}", c.problems());
    }

    #[test]
    fn adsb_feeds_become_net_connectors() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"fp\"\nadsb = \"feed.example:30004\"\nuuid = \"u-1\"\n"
        ));
        assert_eq!(
            c.readsb_feed_args(),
            vec![
                "--net-connector".to_string(),
                "feed.example,30004,beast_reduce_plus_out,uuid=u-1".to_string()
            ]
        );
    }

    #[test]
    fn radio_alone_serves_adsb_feeds() {
        let c = cfg(&format!(
            "{GOOD}\n[[feed]]\nname = \"fp\"\nadsb = \"feed.example:30004\"\n\n[programs]\nradio = \"/bin/sh --gain auto\"\n"
        ));
        assert!(c.problems().is_empty(), "{:?}", c.problems());
    }

    #[test]
    fn missing_radio_binary_is_a_sentence() {
        let c = cfg(&format!(
            "{GOOD}\n[programs]\nradio = \"/nonexistent/rx --gain auto\"\n"
        ));
        let p = c.problems();
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("does not exist"));
    }

    #[test]
    fn empty_feed_is_refused() {
        let c = cfg(&format!("{GOOD}\n[[feed]]\nname = \"void\"\n"));
        assert_eq!(c.problems().len(), 1);
    }

    #[test]
    fn adsb_only_feed_needs_no_uuid() {
        let c = cfg(&format!(
            "{GOOD}\nuuid = \"x\"\n\n[[feed]]\nname = \"fp\"\nadsb = \"feed.example:30004\"\n\n[programs]\nreadsb = \"readsb --quiet\"\n"
        ));
        assert!(c.problems().is_empty(), "{:?}", c.problems());
    }
}
