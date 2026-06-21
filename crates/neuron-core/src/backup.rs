//! Device state backup — the first move of every safe write. A read-only snapshot of a
//! device's entire getter space (raw bytes + structural classification) to a timestamped JSON,
//! so any future write can be diffed against, verified, and rolled back to a known-good prior
//! state. This is the foundation the hardware-native features (firmware button mapping / onboard
//! HyperShift / storage / settings writes) build on — no write is "safe" without it.

use serde::{Deserialize, Serialize};

/// One getter's response (raw + how `discover::classify` read its shape).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GetterSnap {
    pub class: u8,
    pub id: u8,
    pub kind: String,
    /// full 80-byte arg payload as hex (lossless).
    pub raw: String,
}

/// One control interface's worth of getters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IfaceSnap {
    pub usage_page: u16,
    pub usage: u16,
    pub getters: Vec<GetterSnap>,
}

/// A full device snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub vid: u16,
    pub pid: u16,
    pub name: String,
    /// seconds since the Unix epoch (stamped by the caller).
    pub unix_time: u64,
    pub interfaces: Vec<IfaceSnap>,
}

impl Snapshot {
    pub fn filename(&self) -> String {
        format!("neuron-backup-{:04x}-{}.json", self.pid, self.unix_time)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Snapshot> {
        serde_json::from_str(s).ok()
    }

    /// Total getters captured across all interfaces.
    pub fn getter_count(&self) -> usize {
        self.interfaces.iter().map(|i| i.getters.len()).sum()
    }

    /// Diff against another snapshot of the same device: returns the getters whose raw bytes
    /// differ (by class/id). This is the verify/round-trip primitive — after a write, re-snapshot
    /// and diff to confirm ONLY the intended bytes changed.
    pub fn diff<'a>(&'a self, other: &'a Snapshot) -> Vec<Changed<'a>> {
        let mut out = Vec::new();
        for ai in &self.interfaces {
            let bi = other
                .interfaces
                .iter()
                .find(|x| x.usage_page == ai.usage_page && x.usage == ai.usage);
            for ag in &ai.getters {
                let before = &ag.raw;
                let after = bi
                    .and_then(|bi| {
                        bi.getters
                            .iter()
                            .find(|g| g.class == ag.class && g.id == ag.id)
                    })
                    .map(|g| g.raw.as_str());
                match after {
                    Some(a) if a == before => {}
                    Some(a) => out.push(Changed {
                        class: ag.class,
                        id: ag.id,
                        before,
                        after: Some(a),
                    }),
                    None => out.push(Changed {
                        class: ag.class,
                        id: ag.id,
                        before,
                        after: None,
                    }),
                }
            }
        }
        out
    }
}

/// A getter whose value changed between two snapshots (the unit of write verification).
#[derive(Debug, Clone, PartialEq)]
pub struct Changed<'a> {
    pub class: u8,
    pub id: u8,
    pub before: &'a str,
    pub after: Option<&'a str>,
}

/// Format an 80-byte payload as space-separated hex (the lossless on-disk form).
pub fn hex80(a: &[u8; 80]) -> String {
    a.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(getters: Vec<(u8, u8, &str)>) -> Snapshot {
        Snapshot {
            vid: 0x1532,
            pid: 0x00a8,
            name: "test".into(),
            unix_time: 1000,
            interfaces: vec![IfaceSnap {
                usage_page: 1,
                usage: 2,
                getters: getters
                    .into_iter()
                    .map(|(c, i, r)| GetterSnap {
                        class: c,
                        id: i,
                        kind: "x".into(),
                        raw: r.into(),
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn json_round_trips() {
        let s = snap(vec![(0x00, 0x82, "AA BB"), (0x04, 0x85, "00 03 20")]);
        let j = s.to_json();
        assert_eq!(Snapshot::from_json(&j), Some(s));
    }

    #[test]
    fn filename_has_pid_and_time() {
        let s = snap(vec![]);
        assert_eq!(s.filename(), "neuron-backup-00a8-1000.json");
    }

    #[test]
    fn diff_finds_only_changed_getters() {
        let before = snap(vec![
            (0x00, 0x82, "AA"),
            (0x04, 0x85, "00 03 20"),
            (0x0F, 0x84, "00 05 80"),
        ]);
        let after = snap(vec![
            (0x00, 0x82, "AA"),
            (0x04, 0x85, "00 03 20"),
            (0x0F, 0x84, "00 05 FF"),
        ]);
        let d = before.diff(&after);
        assert_eq!(d.len(), 1, "only the brightness getter changed");
        assert_eq!((d[0].class, d[0].id), (0x0F, 0x84));
        assert_eq!(d[0].before, "00 05 80");
        assert_eq!(d[0].after, Some("00 05 FF"));
    }

    #[test]
    fn identical_snapshots_have_no_diff() {
        let s = snap(vec![(0x00, 0x82, "AA"), (0x07, 0x83, "01 2C")]);
        assert!(s.diff(&s.clone()).is_empty());
    }

    #[test]
    fn getter_count_sums_interfaces() {
        let s = snap(vec![(0x00, 0x80, "x"), (0x00, 0x81, "y")]);
        assert_eq!(s.getter_count(), 2);
    }
}
