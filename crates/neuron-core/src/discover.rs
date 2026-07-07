//! Self-emergent capability discovery. No per-device registry: enumerate every Razer
//! `razer_report` control pipe, probe its getter space, and classify each response by its
//! information structure (enum / level / xy-pair / table / string). Two different devices
//! reveal two different feature suites from this one routine — the universal-tool brain.

use crate::protocol::{Report, Status, BUF_LEN};
use crate::transport::{self, Transport};
use std::collections::BTreeSet;
use std::time::Duration;

/// Send a command and busy-poll for the echoed success reply (read-only when id>=0x80).
pub fn exec(
    t: &dyn Transport,
    tid: u8,
    class: u8,
    id: u8,
    size: u8,
    args: &[u8],
) -> Option<[u8; 80]> {
    let mut req = Report::command(tid, class, id, size);
    for (i, b) in args.iter().enumerate() {
        if i < req.args.len() {
            req.args[i] = *b;
        }
    }
    let out = req.to_buf();
    t.set_feature(&out).ok()?;
    for i in 0..40 {
        std::thread::sleep(Duration::from_millis(8));
        let mut b = [0u8; BUF_LEN];
        if t.get_feature(&mut b).is_ok() && b[7] == class && b[8] == id {
            match Status::from_u8(b[1]) {
                Status::Success => return Some(Report::from_buf(&b).args),
                Status::Fail | Status::Unsupported => return None,
                _ => {}
            }
        }
        if i % 12 == 11 {
            let _ = t.set_feature(&out);
        }
    }
    None
}

/// Classify a response by its information structure (the Logos-spirit seed).
pub fn classify(a: &[u8; 80]) -> &'static str {
    let p = &a[..20];
    let nz = p.iter().filter(|&&b| b != 0).count();
    if nz == 0 {
        return "empty";
    }
    let ascii = p.iter().filter(|&&b| (48..=122).contains(&b)).count();
    if ascii >= 5 && ascii + 1 >= nz {
        return "string";
    }
    if a[1] == a[3] && a[2] == a[4] && (a[1] | a[2]) != 0 {
        return "xy-pair";
    }
    if nz <= 2 {
        return "enum/level";
    }
    "struct/table"
}

/// Human hint for a command class (emergent grouping; the class itself is discovered).
pub fn class_hint(c: u8) -> &'static str {
    match c {
        0x00 => "device-info",
        0x03 => "lighting (legacy)",
        0x04 => "sensitivity/dpi",
        0x06 => "onboard-storage",
        0x07 => "power/battery",
        0x0F => "lighting-matrix",
        _ => "?",
    }
}

pub struct DeviceFp {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub classes: Vec<u8>,
    pub cmds: Vec<(u8, u8, &'static str)>,
}

// NOTE: the old `to_toml_skeleton` self-onboarding stub lived here — it emitted opaque
// `get_XX_YY` getter lists that still needed hand-RE before a device worked. Superseded by
// [`crate::synth`], which synthesizes a COMPLETE, immediately-usable def from the same probe.

/// Probe one control pipe's getter space (classes × ids 0x80..0x8F, read-only).
pub fn fingerprint(t: &dyn Transport, tid: u8) -> (Vec<u8>, Vec<(u8, u8, &'static str)>) {
    let mut cmds = Vec::new();
    let mut classes = BTreeSet::new();
    for class in 0x00u8..=0x0F {
        for id in 0x80u8..=0x8F {
            if let Some(a) = exec(t, tid, class, id, 0x20, &[]) {
                let k = classify(&a);
                if k != "empty" {
                    cmds.push((class, id, k));
                    classes.insert(class);
                }
            }
        }
    }
    (classes.into_iter().collect(), cmds)
}

/// Discover every connected Razer `razer_report` device and fingerprint it. Zero registry.
pub fn discover() -> Vec<DeviceFp> {
    let infos = match transport::enumerate() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for info in infos {
        // The universal signature of a razer_report control pipe: VID Razer + a 91-byte
        // feature report, on WHATEVER interface it lives.
        if info.vid != 0x1532 || info.feature_len != 91 {
            continue;
        }
        if !seen.insert((info.pid, info.usage_page, info.usage)) {
            continue;
        }
        let t = match transport::open_path(&info.path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let (classes, cmds) = fingerprint(&*t, 0x1F);
        if !cmds.is_empty() {
            out.push(DeviceFp {
                vid: info.vid,
                pid: info.pid,
                usage_page: info.usage_page,
                usage: info.usage,
                classes,
                cmds,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::classify;

    fn args(prefix: &[u8]) -> [u8; 80] {
        let mut a = [0u8; 80];
        a[..prefix.len()].copy_from_slice(prefix);
        a
    }

    #[test]
    fn classifies_structures() {
        assert_eq!(classify(&args(&[])), "empty");
        assert_eq!(classify(&args(&[0x03])), "enum/level"); // device mode
        assert_eq!(classify(&args(&[0x00, 0x45])), "enum/level"); // battery
                                                                  // DPI: [vs, X_hi, X_lo, Y_hi, Y_lo] with X==Y
        assert_eq!(classify(&args(&[0x00, 0x03, 0x20, 0x03, 0x20])), "xy-pair");
        // storage info: many nonzero bytes
        assert_eq!(
            classify(&args(&[
                0x00, 0x64, 0x00, 0x01, 0xF0, 0x00, 0x00, 0x01, 0xE4
            ])),
            "struct/table"
        );
        // an ascii serial
        assert_eq!(classify(&args(b"IO1735F09002641")), "string");
    }
}
