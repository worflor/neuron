// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Regenerate `crates/neuron-core/protocol/razer-keyboards.toml` from pinned checkouts of
//! openrazer, OpenRGB and SignalRGB's plugins.
//!
//! Facts only: per PID, the numbers each project uses (command class, transaction ids, LED, matrix
//! size, USB interface, control collection) and the line they come from. No upstream code or
//! table is copied; the atlas cites. Run through the `razer-atlas` binary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Where each upstream checkout lives, and the commit it is at.
pub struct Checkouts {
    pub openrazer: (std::path::PathBuf, String),
    pub openrgb: (std::path::PathBuf, String),
    pub signalrgb: (std::path::PathBuf, String),
}

fn line_of(text: &str, idx: usize) -> usize {
    text[..idx].bytes().filter(|&b| b == b'\n').count() + 1
}

/// The hex number starting at `s` (after optional spaces and `0x`).
fn hex_at(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    let end = s.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(s.len());
    u32::from_str_radix(&s[..end], 16).ok()
}

fn dec_at(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// `#define NAME 0xHEX` lines.
#[must_use]
pub fn defines(text: &str, prefix: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#define") else { continue };
        let mut parts = rest.split_whitespace();
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else { continue };
        if name.starts_with(prefix) {
            if let Some(v) = hex_at(value) {
                out.insert(name.to_string(), v);
            }
        }
    }
    out
}

/// One OpenRGB `razer_device` entry for a keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRgbDevice {
    pub name: String,
    pub class: &'static str,
    pub tx: u32,
    pub led: Option<u32>,
    pub rows: u32,
    pub cols: u32,
    pub line: usize,
}

/// Keyboards in OpenRGB's `RazerDevices.cpp`, by PID.
pub fn openrgb_devices(cpp: &str, pids: &BTreeMap<String, u32>) -> BTreeMap<u32, OpenRgbDevice> {
    openrgb_devices_of(cpp, pids, "DEVICE_TYPE_KEYBOARD")
}

/// OpenRGB's `RazerDevices.cpp` entries of one device type (`DEVICE_TYPE_KEYBOARD`,
/// `DEVICE_TYPE_MOUSE`), by PID.
#[must_use]
pub fn openrgb_devices_of(cpp: &str, pids: &BTreeMap<String, u32>, kind: &str) -> BTreeMap<u32, OpenRgbDevice> {
    let mut out = BTreeMap::new();
    let mut at = 0;
    while let Some(i) = cpp[at..].find("static const razer_device ") {
        let start = at + i;
        let Some(open) = cpp[start..].find('{').map(|o| start + o + 1) else { break };
        let Some(close) = cpp[open..].find("};").map(|c| open + c) else { break };
        at = close;
        let fields: Vec<&str> = cpp[open..close].split(",\n").map(str::trim).collect();
        if fields.len() < 8 || fields[2] != kind {
            continue;
        }
        let Some(&pid) = pids.get(fields[1]) else { continue };
        let class = match fields[3] {
            "RAZER_MATRIX_TYPE_EXTENDED" => "extended",
            "RAZER_MATRIX_TYPE_STANDARD" => "standard",
            "RAZER_MATRIX_TYPE_LINEAR" => "linear",
            _ => "other",
        };
        let led = match fields[5] {
            "RAZER_LED_ID_BACKLIGHT" => Some(0x05),
            "RAZER_LED_ID_ZERO" => Some(0x00),
            _ => None,
        };
        let (Some(tx), Some(rows), Some(cols)) = (hex_at(fields[4]), dec_at(fields[6]), dec_at(fields[7])) else {
            continue;
        };
        out.insert(pid, OpenRgbDevice {
            name: fields[0].trim_matches('"').to_string(),
            class,
            tx,
            led,
            rows,
            cols,
            line: line_of(cpp, start),
        });
    }
    out
}

/// Every OpenRGB detector line per PID, in file order: (interface, usage page, usage, line). A PID
/// can have several (the BlackWidow V4 changed collection at firmware 1.5).
#[must_use]
pub fn openrgb_detectors(cpp: &str, pids: &BTreeMap<String, u32>) -> BTreeMap<u32, Vec<(u32, u32, u32, usize)>> {
    let mut out: BTreeMap<u32, Vec<(u32, u32, u32, usize)>> = BTreeMap::new();
    let mut at = 0;
    while let Some(i) = cpp[at..].find("REGISTER_HID_DETECTOR_IPU(") {
        let start = at + i;
        let Some(end) = cpp[start..].find(");").map(|e| start + e) else { break };
        at = end;
        let args: Vec<&str> = cpp[start + 26..end].split(',').map(str::trim).collect();
        if args.len() != 7 {
            continue;
        }
        let Some(&pid) = pids.get(args[3]) else { continue };
        if let (Some(i), Some(p), Some(u)) = (hex_at(args[4]), hex_at(args[5]), hex_at(args[6])) {
            let seen = out.entry(pid).or_default();
            if !seen.iter().any(|d| (d.0, d.1, d.2) == (i, p, u)) {
                seen.push((i, p, u, line_of(cpp, start)));
            }
        }
    }
    out
}

/// A C line without its trailing `//` comment, trimmed.
fn code(line: &str) -> &str {
    line.split("//").next().unwrap_or("").trim()
}

/// openrazer's per-PID choice in one sysfs function: (extended?, tx) for the first request each
/// `case` group builds. A group with no explicit tx takes the driver default 0xFF.
#[must_use]
pub fn openrazer_switch(c: &str, function: &str, pids: &BTreeMap<String, u32>) -> BTreeMap<u32, (bool, u32)> {
    let mut out = BTreeMap::new();
    let Some(start) = c.find(&format!("{function}(struct device")) else { return out };
    let end = c[start + 10..].find("\nstatic ").map_or(c.len(), |e| start + 10 + e);
    let mut group: Vec<u32> = Vec::new();
    let mut request: Option<bool> = None;
    let mut tx: Option<u32> = None;
    let flush = |group: &mut Vec<u32>, request: &mut Option<bool>, tx: &mut Option<u32>, out: &mut BTreeMap<u32, (bool, u32)>| {
        if let Some(ext) = *request {
            for &p in group.iter() {
                out.entry(p).or_insert((ext, tx.unwrap_or(0xFF)));
            }
        }
        group.clear();
        *request = None;
        *tx = None;
    };
    for line in c[start..end].lines() {
        let t = code(line);
        if let Some(name) = t.strip_prefix("case ").and_then(|r| r.strip_suffix(':')) {
            if request.is_some() {
                flush(&mut group, &mut request, &mut tx, &mut out);
            }
            if let Some(&p) = pids.get(name) {
                group.push(p);
            }
            continue;
        }
        if request.is_none() && t.contains("razer_chroma_") && t.contains('=') && !group.is_empty() {
            request = Some(t.contains("extended"));
        }
        if let Some(i) = t.find("transaction_id.id") {
            if tx.is_none() && request.is_some() {
                tx = t[i..].split('=').nth(1).and_then(hex_at);
            }
        }
        if t.starts_with("break;") || t.starts_with("default:") {
            flush(&mut group, &mut request, &mut tx, &mut out);
        }
    }
    flush(&mut group, &mut request, &mut tx, &mut out);
    out
}

/// openrazer's `razer_get_report_params`, per PID and for its `default:` branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportParams {
    /// PID -> (USB interface (wIndex), SET->GET wait in µs).
    pub by_pid: BTreeMap<u32, (u32, Option<u32>)>,
    pub default: (u32, Option<u32>),
}

impl ReportParams {
    #[must_use]
    pub fn get(&self, pid: u32) -> (u32, Option<u32>) {
        self.by_pid.get(&pid).copied().unwrap_or(self.default)
    }
}

/// Decimal `#define NAME 123` lines.
fn decimal_defines(text: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#define") else { continue };
        let mut parts = rest.split_whitespace();
        if let (Some(name), Some(v)) = (parts.next(), parts.next().and_then(|v| v.parse().ok())) {
            out.insert(name.to_string(), v);
        }
    }
    out
}

/// openrazer's mouse `razer_get_report`: per PID, the USB interface (`index`, 0 unless a case sets
/// it) and the SET->GET wait passed to `razer_get_usb_response`; `h` resolves the wait constants.
#[must_use]
pub fn openrazer_mouse_params(c: &str, h: &str, pids: &BTreeMap<String, u32>) -> ReportParams {
    let waits = decimal_defines(h);
    let mut out = ReportParams { default: (0, None), ..ReportParams::default() };
    let Some(start) = c.find("static int razer_get_report(") else { return out };
    let body = &c[start..];
    let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
    let (mut pending, mut iface, mut default) = (Vec::new(), 0, false);
    for line in body.lines().map(code) {
        if let Some(name) = line.strip_prefix("case ").and_then(|r| r.strip_suffix(':')) {
            if let Some(&pid) = pids.get(name.trim()) {
                pending.push(pid);
            }
        } else if line.starts_with("default:") {
            default = true;
        } else if let Some(v) = line.strip_prefix("index = ").and_then(hex_at) {
            iface = v;
        } else if line.contains("razer_get_usb_response(") {
            let name = line.rsplit(',').next().unwrap_or("").trim().trim_end_matches(';').trim_end_matches(')').trim();
            let wait = waits.get(name).copied();
            if default {
                out.default = (iface, wait);
            } else {
                for pid in pending.drain(..) {
                    out.by_pid.insert(pid, (iface, wait));
                }
            }
            (iface, default) = (0, false);
            pending.clear();
        }
    }
    out
}

/// openrazer's `razer_get_report_params`: the USB interface and reply wait per PID; `h` resolves
/// the wait constants.
#[must_use]
pub fn openrazer_report_params(c: &str, h: &str, pids: &BTreeMap<String, u32>) -> ReportParams {
    let mut out = ReportParams { default: (1, None), ..ReportParams::default() };
    let waits = decimal_defines(h);
    let Some(start) = c.find("static void razer_get_report_params") else { return out };
    let end = c[start..].find("\n}\n").map_or(c.len(), |e| start + e);
    let mut group = Vec::new();
    let mut default = false;
    let (mut iface, mut wait) = (None, None);
    for line in c[start..end].lines() {
        let t = code(line);
        if let Some(name) = t.strip_prefix("case ").and_then(|r| r.strip_suffix(':')) {
            if let Some(&p) = pids.get(name) {
                group.push(p);
            }
        } else if t == "default:" {
            default = true;
        } else if let Some(rest) = t.strip_prefix("*report_index =") {
            iface = hex_at(rest);
        } else if let Some(rest) = t.strip_prefix("*wait =") {
            wait = waits.get(rest.trim().trim_end_matches(';')).copied();
        } else if t.starts_with("break;") {
            if let Some(i) = iface {
                if default {
                    out.default = (i, wait);
                }
                for &p in &group {
                    out.by_pid.insert(p, (i, wait));
                }
            }
            group.clear();
            default = false;
            (iface, wait) = (None, None);
        }
    }
    out
}

/// One openrazer daemon keyboard class: its name, `(rows, cols)` when declared, and its line.
pub type DaemonClass = (String, Option<(u32, u32)>, usize);

/// openrazer daemon keyboard classes by PID.
#[must_use]
pub fn openrazer_dims(py: &str) -> BTreeMap<u32, DaemonClass> {
    let mut out = BTreeMap::new();
    let mut starts: Vec<usize> = py.match_indices("\nclass ").map(|(i, _)| i + 1).collect();
    starts.push(py.len());
    for w in starts.windows(2) {
        let body = &py[w[0]..w[1]];
        let name = body[6..].split(['(', ':']).next().unwrap_or("").to_string();
        let Some(pid) = body.find("USB_PID = ").and_then(|i| hex_at(&body[i + 10..])) else { continue };
        let dims = body.find("MATRIX_DIMS = [").and_then(|i| {
            let rest = &body[i + 15..];
            let mut nums = rest.split([',', ']']).map(str::trim);
            Some((nums.next()?.parse().ok()?, nums.next()?.parse().ok()?))
        });
        out.entry(pid).or_insert((name, dims, line_of(py, w[0])));
    }
    out
}

/// One SignalRGB Razer plugin's facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalPlugin {
    pub file: String,
    pub name: String,
    pub interface: Option<u32>,
    /// The custom-frame row report: (class, tx, data_size, line).
    pub row: Option<(u32, u32, u32, usize)>,
}

impl SignalPlugin {
    /// Columns from an extended row's data_size: 5 header bytes and 3 per column.
    #[must_use]
    pub fn cols(&self) -> Option<u32> {
        let (class, _, size, _) = self.row?;
        (class == 0x0F && size > 5 && (size - 5) % 3 == 0).then(|| (size - 5) / 3)
    }
}

/// The byte a JS plugin assigns to `packet[n]` or `report[n]` in `body`, in order.
fn packet_bytes(body: &str, n: usize) -> Vec<u32> {
    let mut out = Vec::new();
    for key in [format!("packet[{n}]"), format!("report[{n}]")] {
        let mut at = 0;
        while let Some(i) = body[at..].find(&key) {
            let after = &body[at + i + key.len()..];
            at += i + key.len();
            if let Some(rest) = after.trim_start().strip_prefix('=') {
                if !rest.starts_with('=') {
                    if let Some(v) = hex_at(rest) {
                        out.push(v);
                    }
                }
            }
        }
    }
    out
}

/// Parse one SignalRGB plugin: product ids and its facts. `None` when it declares no product id.
#[must_use]
pub fn signal_plugin(file: &str, js: &str) -> Option<(Vec<u32>, SignalPlugin)> {
    let pid_at = js.find("export function ProductId()")?;
    let body = &js[pid_at..];
    let ret = body.find("return")?;
    let close = body[ret..].find('}')?;
    let mut pids = Vec::new();
    let mut rest = &body[ret..ret + close];
    while let Some(i) = rest.find("0x") {
        if let Some(v) = hex_at(&rest[i..]) {
            pids.push(v);
        }
        rest = &rest[i + 2..];
    }
    let name = js
        .find("export function Name()")
        .and_then(|i| js[i..].split('"').nth(1))
        .unwrap_or(file)
        .to_string();
    let interface = js.find("endpoint.interface").and_then(|i| {
        let rest = js[i + 18..].trim_start().trim_start_matches('=').trim_start_matches('=').trim_start_matches('=');
        dec_at(rest)
    });
    let mut row = None;
    let mut at = 0;
    while let Some(i) = js[at..].find("function ") {
        let start = at + i;
        let Some(open) = js[start..].find('{').map(|o| start + o + 1) else { break };
        let mut depth = 1;
        let mut end = open;
        for (k, ch) in js[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = open + k;
                break;
            }
        }
        at = open;
        let body = &js[open..end.max(open)];
        let (class, id) = (packet_bytes(body, 7).first().copied(), packet_bytes(body, 8).first().copied());
        if matches!((class, id), (Some(0x0F), Some(0x03)) | (Some(0x03), Some(0x0B))) {
            let tx = packet_bytes(body, 2).first().copied();
            let size = packet_bytes(body, 6).first().copied();
            if let (Some(class), Some(tx), Some(size)) = (class, tx, size) {
                row = Some((class, tx, size, line_of(js, start)));
                break;
            }
        }
    }
    Some((pids, SignalPlugin { file: file.to_string(), name, interface, row }))
}

/// Read an upstream file with its line endings normalized (a Windows checkout has CRLF).
fn read(dir: &std::path::Path, rel: &str) -> std::io::Result<String> {
    Ok(std::fs::read_to_string(dir.join(rel))?.replace("\r\n", "\n"))
}

fn human(name: &str) -> String {
    let s = name.trim().trim_start_matches("Razer").trim();
    if s.contains(' ') {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.char_indices() {
        let prev = s[..i].chars().last();
        if i > 0 && c.is_ascii_uppercase() && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Build the whole atlas file text from the three checkouts.
pub fn build(c: &Checkouts) -> std::io::Result<String> {
    let (or_dir, or_sha) = (&c.openrazer.0, &c.openrazer.1);
    let (rgb_dir, rgb_sha) = (&c.openrgb.0, &c.openrgb.1);
    let (sig_dir, sig_sha) = (&c.signalrgb.0, &c.signalrgb.1);

    let rgb_h = read(rgb_dir, "Controllers/RazerController/RazerDevices.h")?;
    let rgb_pids = defines(&rgb_h, "RAZER_");
    let rgb = openrgb_devices(&read(rgb_dir, "Controllers/RazerController/RazerDevices.cpp")?, &rgb_pids);
    let detect = openrgb_detectors(&read(rgb_dir, "Controllers/RazerController/RazerControllerDetect.cpp")?, &rgb_pids);

    let or_h = read(or_dir, "driver/razerkbd_driver.h")?;
    let or_pids = defines(&or_h, "USB_DEVICE_ID_RAZER_");
    let or_c = read(or_dir, "driver/razerkbd_driver.c")?;
    let statics = openrazer_switch(&or_c, "razer_attr_write_matrix_effect_static", &or_pids);
    let getters = openrazer_switch(&or_c, "razer_attr_read_matrix_brightness", &or_pids);
    let frames = openrazer_switch(&or_c, "razer_attr_write_matrix_custom_frame", &or_pids);
    let params = openrazer_report_params(&or_c, &or_h, &or_pids);
    let dims = openrazer_dims(&read(or_dir, "daemon/openrazer_daemon/hardware/keyboards.py")?);
    let line_static = or_c.find("razer_attr_write_matrix_effect_static(struct").map_or(0, |i| line_of(&or_c, i));
    let line_bright = or_c.find("razer_attr_read_matrix_brightness(struct").map_or(0, |i| line_of(&or_c, i));
    let line_frame = or_c.find("razer_attr_write_matrix_custom_frame(struct").map_or(0, |i| line_of(&or_c, i));
    let line_params = or_c.find("static void razer_get_report_params").map_or(0, |i| line_of(&or_c, i));

    let mut signal: BTreeMap<u32, SignalPlugin> = BTreeMap::new();
    let plugins = sig_dir.join("Plugins/Razer");
    let mut files: Vec<_> = std::fs::read_dir(&plugins)?.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files.iter().filter(|p| p.extension().is_some_and(|x| x == "js")) {
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some((pids, plugin)) = signal_plugin(&file, &std::fs::read_to_string(path)?.replace("\r\n", "\n")) {
            for p in pids {
                signal.entry(p).or_insert_with(|| plugin.clone());
            }
        }
    }

    // the generated file's own licence header, assembled so this file still carries exactly one
    // identifier line (tests/license_boundary.rs counts them)
    let mut s = format!(
        "# SPDX-FileCopyrightText: 2026 Woflo Labs\n# {}: GPL-3.0-or-later\n",
        "SPDX-License-Identifier"
    );
    let _ = write!(
        s,
        "# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.\n\
         \n\
         # RAZER KEYBOARD ATLAS: what three open implementations send each Razer keyboard.\n\
         #\n\
         # Generated by `cargo run -p neuron-testkit --bin razer-atlas` from pinned checkouts; do not\n\
         # edit by hand. Facts only, one [[keyboard]] per USB PID, each source's values kept apart.\n\
         # `src/atlas.rs` weighs agreement by independent lineage (OpenRGB derives from openrazer;\n\
         # SignalRGB is its own); nothing here is hardware evidence. Synthesis uses these values as\n\
         # priors, and probing still decides what a board answers.\n\
         #\n\
         # openrazer (driver/razerkbd_driver.c): class and effect_tx from the static-effect builder\n\
         #   (razer_attr_write_matrix_effect_static, L{line_static}); getter_tx from the brightness read\n\
         #   (razer_attr_read_matrix_brightness, L{line_bright}); frame_tx from the custom-frame rows\n\
         #   (razer_attr_write_matrix_custom_frame, L{line_frame}); interface from razer_get_report_params\n\
         #   (L{line_params}), the default branch's when the PID takes it, with wait_us, its SET->GET wait;\n\
         #   rows/cols from the daemon class at `at`.\n\
         # openrgb (Controllers/RazerController): class, tx, led, rows, cols from the razer_device entry at\n\
         #   `at`; interface, usage_page, usage from the REGISTER_HID_DETECTOR_IPU line at `detect_at`.\n\
         # signalrgb (Plugins/Razer): interface from Validate(); class, row_tx and cols from the function\n\
         #   building the custom-frame row report (cols = (data_size - 5) / 3) at `at`.\n\
         \n\
         [sources.openrazer]\nurl = \"https://github.com/openrazer/openrazer\"\nlicense = \"GPL-2.0-or-later\"\nsha = \"{or_sha}\"\n\n\
         [sources.openrgb]\nurl = \"https://gitlab.com/CalcProgrammer1/OpenRGB\"\nlicense = \"GPL-2.0-or-later\"\nsha = \"{rgb_sha}\"\nderived_from = \"openrazer\"\n\n\
         [sources.signalrgb]\nurl = \"https://gitlab.com/signalrgb/signal-plugins\"\nlicense = \"NOASSERTION\"\nsha = \"{sig_sha}\"\n\n"
    );

    // Every PID openrazer or OpenRGB names as a lit keyboard, plus SignalRGB plugins that drive a
    // keyboard-shaped frame.
    let mut pids: BTreeSet<u32> = statics.keys().chain(rgb.keys()).copied().collect();
    pids.extend(signal.iter().filter(|(p, _)| dims.contains_key(p) || rgb.contains_key(p)).map(|(p, _)| *p));
    for pid in pids {
        let st = statics.get(&pid);
        let o = rgb.get(&pid);
        let d = dims.get(&pid);
        if st.is_none() && o.is_none() {
            continue;
        }
        let name = o.map(|o| o.name.clone()).or_else(|| d.map(|d| d.0.clone())).unwrap_or_else(|| format!("{pid:04X}"));
        let name = human(&name);
        let _ = writeln!(s, "[[keyboard]]\npid = 0x{pid:04X}\nname = \"{name}\"");
        if name.contains("Blade") || name.contains("Book") {
            s.push_str("form = \"laptop\"\n");
        }
        if let Some(&(ext, tx)) = st {
            let mut parts = vec![format!("class = \"{}\"", if ext { "extended" } else { "standard" }), format!("effect_tx = 0x{tx:02X}")];
            if let Some(&(_, gtx)) = getters.get(&pid).filter(|g| g.0 == ext) {
                parts.push(format!("getter_tx = 0x{gtx:02X}"));
            }
            if let Some(&(_, ftx)) = frames.get(&pid).filter(|f| f.0 == ext) {
                parts.push(format!("frame_tx = 0x{ftx:02X}"));
            }
            if let Some((_, Some((r, c)), _)) = d {
                parts.push(format!("rows = {r}"));
                parts.push(format!("cols = {c}"));
            }
            let (iface, wait) = params.get(pid);
            parts.push(format!("interface = {iface}"));
            if let Some(w) = wait {
                parts.push(format!("wait_us = {w}"));
            }
            if let Some((_, _, line)) = d {
                parts.push(format!("at = \"daemon/openrazer_daemon/hardware/keyboards.py#L{line}\""));
            }
            let _ = writeln!(s, "openrazer = {{ {} }}", parts.join(", "));
        }
        if let Some(o) = o {
            let mut parts = vec![format!("class = \"{}\"", o.class), format!("tx = 0x{:02X}", o.tx)];
            if let Some(led) = o.led {
                parts.push(format!("led = 0x{led:02X}"));
            }
            parts.push(format!("rows = {}", o.rows));
            parts.push(format!("cols = {}", o.cols));
            let found = detect.get(&pid).map(Vec::as_slice).unwrap_or_default();
            if let Some(&(i, p, u, _)) = found.first() {
                parts.push(format!("interface = {i}"));
                parts.push(format!("usage_page = 0x{p:04X}"));
                parts.push(format!("usage = 0x{u:04X}"));
            }
            if found.len() > 1 {
                let also: Vec<String> = found[1..].iter().map(|&(i, p, u, _)| format!("[{i}, 0x{p:04X}, 0x{u:04X}]")).collect();
                parts.push(format!("also = [{}]", also.join(", ")));
            }
            parts.push(format!("at = \"Controllers/RazerController/RazerDevices.cpp#L{}\"", o.line));
            if let Some(&(_, _, _, line)) = found.first() {
                parts.push(format!("detect_at = \"Controllers/RazerController/RazerControllerDetect.cpp#L{line}\""));
            }
            let _ = writeln!(s, "openrgb = {{ {} }}", parts.join(", "));
        }
        if let Some(p) = signal.get(&pid) {
            let mut parts = Vec::new();
            if let Some((class, tx, _, _)) = p.row {
                parts.push(format!("class = \"{}\"", if class == 0x0F { "extended" } else { "standard" }));
                parts.push(format!("row_tx = 0x{tx:02X}"));
            }
            if let Some(cols) = p.cols() {
                parts.push(format!("cols = {cols}"));
            }
            if let Some(i) = p.interface {
                parts.push(format!("interface = {i}"));
            }
            let line = p.row.map_or(String::new(), |r| format!("#L{}", r.3));
            parts.push(format!("at = \"Plugins/Razer/{}{line}\"", p.file));
            let _ = writeln!(s, "signalrgb = {{ {} }}", parts.join(", "));
        }
        s.push('\n');
    }
    Ok(s.trim_end().to_string() + "\n")
}

/// The Razer mouse atlas: what openrazer and OpenRGB send each mouse.
pub fn build_mice(c: &Checkouts) -> std::io::Result<String> {
    let (or_dir, or_sha) = (&c.openrazer.0, &c.openrazer.1);
    let (rgb_dir, rgb_sha) = (&c.openrgb.0, &c.openrgb.1);

    let rgb_pids = defines(&read(rgb_dir, "Controllers/RazerController/RazerDevices.h")?, "RAZER_");
    let rgb = openrgb_devices_of(&read(rgb_dir, "Controllers/RazerController/RazerDevices.cpp")?, &rgb_pids, "DEVICE_TYPE_MOUSE");
    let detect = openrgb_detectors(&read(rgb_dir, "Controllers/RazerController/RazerControllerDetect.cpp")?, &rgb_pids);
    let or_h = read(or_dir, "driver/razermouse_driver.h")?;
    let or_pids = defines(&or_h, "USB_DEVICE_ID_RAZER_");
    let or_c = read(or_dir, "driver/razermouse_driver.c")?;
    let params = openrazer_mouse_params(&or_c, &or_h, &or_pids);
    let dims = openrazer_dims(&read(or_dir, "daemon/openrazer_daemon/hardware/mouse.py")?);
    let line_params = or_c.find("static int razer_get_report(").map_or(0, |i| line_of(&or_c, i));

    let mut s = format!(
        "# SPDX-FileCopyrightText: 2026 Woflo Labs\n# {}: GPL-3.0-or-later\n",
        "SPDX-License-Identifier"
    );
    let _ = write!(
        s,
        "# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.\n\
         \n\
         # RAZER MOUSE ATLAS: what two open implementations send each Razer mouse.\n\
         #\n\
         # Generated by `cargo run -p neuron-testkit --bin razer-atlas -- --mice` from pinned checkouts;\n\
         # do not edit by hand. Facts only, one [[mouse]] per USB PID, each source's values kept apart.\n\
         # OpenRGB derives from openrazer, so agreement here is one lineage; nothing is hardware evidence.\n\
         #\n\
         # openrazer: interface and wait_us (the SET->GET wait) from the mouse driver's razer_get_report\n\
         #   (driver/razermouse_driver.c L{line_params}, the default branch's when the PID takes it);\n\
         #   rows/cols from the daemon class at `at`.\n\
         # openrgb (Controllers/RazerController): class, tx, led, rows, cols from the razer_device entry at\n\
         #   `at`; interface, usage_page, usage from the REGISTER_HID_DETECTOR_IPU line at `detect_at`.\n\
         \n\
         [sources.openrazer]\nurl = \"https://github.com/openrazer/openrazer\"\nlicense = \"GPL-2.0-or-later\"\nsha = \"{or_sha}\"\n\n\
         [sources.openrgb]\nurl = \"https://gitlab.com/CalcProgrammer1/OpenRGB\"\nlicense = \"GPL-2.0-or-later\"\nsha = \"{rgb_sha}\"\nderived_from = \"openrazer\"\n\n"
    );
    let pids: BTreeSet<u32> = rgb.keys().chain(dims.keys()).chain(params.by_pid.keys()).copied().collect();
    for pid in pids {
        let o = rgb.get(&pid);
        let d = dims.get(&pid);
        let name = o.map(|o| o.name.clone()).or_else(|| d.map(|d| d.0.clone())).unwrap_or_else(|| format!("{pid:04X}"));
        let _ = writeln!(s, "[[mouse]]\npid = 0x{pid:04X}\nname = \"{}\"", human(&name));
        let (iface, wait) = params.get(pid);
        let mut parts = Vec::new();
        if let Some((_, Some((r, c)), _)) = d {
            parts.push(format!("rows = {r}"));
            parts.push(format!("cols = {c}"));
        }
        parts.push(format!("interface = {iface}"));
        if let Some(w) = wait {
            parts.push(format!("wait_us = {w}"));
        }
        if let Some((_, _, line)) = d {
            parts.push(format!("at = \"daemon/openrazer_daemon/hardware/mouse.py#L{line}\""));
        }
        let _ = writeln!(s, "openrazer = {{ {} }}", parts.join(", "));
        if let Some(o) = o {
            let mut parts = vec![format!("class = \"{}\"", o.class), format!("tx = 0x{:02X}", o.tx)];
            if let Some(led) = o.led {
                parts.push(format!("led = 0x{led:02X}"));
            }
            parts.push(format!("rows = {}", o.rows));
            parts.push(format!("cols = {}", o.cols));
            if let Some(&(i, p, u, _)) = detect.get(&pid).and_then(|f| f.first()) {
                parts.push(format!("interface = {i}"));
                parts.push(format!("usage_page = 0x{p:04X}"));
                parts.push(format!("usage = 0x{u:04X}"));
            }
            parts.push(format!("at = \"Controllers/RazerController/RazerDevices.cpp#L{}\"", o.line));
            if let Some(&(_, _, _, line)) = detect.get(&pid).and_then(|f| f.first()) {
                parts.push(format!("detect_at = \"Controllers/RazerController/RazerControllerDetect.cpp#L{line}\""));
            }
            let _ = writeln!(s, "openrgb = {{ {} }}", parts.join(", "));
        }
        s.push('\n');
    }
    Ok(s.trim_end().to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic snippets in each upstream's shape (not upstream text): the parsers read the
    /// numbers the atlas cites and nothing else.
    #[test]
    fn the_mouse_wait_switch_reads_interface_and_wait_per_case() {
        let h = "#define RAZER_MOUSE_WAIT_US 600\n#define RAZER_NEW_MOUSE_RECEIVER_WAIT_US 31000\n";
        let c = "static int razer_get_report(struct hid_device *hdev)\n{\n    switch (pid) {\n    case USB_DEVICE_ID_RAZER_A:\n    case USB_DEVICE_ID_RAZER_B: // a comment\n        return razer_get_usb_response(hdev, index, request, index, response, RAZER_NEW_MOUSE_RECEIVER_WAIT_US);\n    case USB_DEVICE_ID_RAZER_C:\n        index = 0x03;\n        return razer_get_usb_response(hdev, index, request, index, response, RAZER_MOUSE_WAIT_US);\n    default:\n        return razer_get_usb_response(hdev, index, request, index, response, RAZER_MOUSE_WAIT_US);\n    }\n}\n";
        let pids = BTreeMap::from([
            ("USB_DEVICE_ID_RAZER_A".to_string(), 0x0A),
            ("USB_DEVICE_ID_RAZER_B".to_string(), 0x0B),
            ("USB_DEVICE_ID_RAZER_C".to_string(), 0x0C),
        ]);
        let p = openrazer_mouse_params(c, h, &pids);
        assert_eq!(p.get(0x0A), (0, Some(31000)));
        assert_eq!(p.get(0x0B), (0, Some(31000)));
        assert_eq!(p.get(0x0C), (3, Some(600)));
        assert_eq!(p.get(0x0D), (0, Some(600)), "the default branch");
    }

    #[test]
    fn each_parser_reads_its_shape() {
        let h = "#define RAZER_TEST_PID 0x024E\n#define OTHER 0x1\n";
        let pids = defines(h, "RAZER_");
        assert_eq!(pids.get("RAZER_TEST_PID"), Some(&0x024E));
        let cpp = "static const razer_device test_device =\n{\n    \"Razer Test\",\n    RAZER_TEST_PID,\n    DEVICE_TYPE_KEYBOARD,\n    RAZER_MATRIX_TYPE_EXTENDED,\n    0x3F,\n    RAZER_LED_ID_BACKLIGHT,\n    6,\n    22,\n    {\n        NULL\n    },\n    NULL\n};\n";
        let d = &openrgb_devices(cpp, &pids)[&0x024E];
        assert_eq!((d.class, d.tx, d.led, d.rows, d.cols), ("extended", 0x3F, Some(0x05), 6, 22));
        let det = "REGISTER_HID_DETECTOR_IPU(\"Razer Test\", DetectRazerControllers, RAZER_VID, RAZER_TEST_PID, 0x03, 0x0C, 0x01);";
        let det2 = format!("{det}\n{}", det.replace("0x03, 0x0C, 0x01", "0x03, 0x01, 0x00"));
        let found = &openrgb_detectors(&det2, &pids)[&0x024E];
        assert_eq!(found.iter().map(|d| (d.0, d.1, d.2)).collect::<Vec<_>>(), vec![(3, 0x0C, 0x01), (3, 0x01, 0x00)]);

        let or_pids = defines("#define USB_DEVICE_ID_RAZER_TEST 0x024E\n", "USB_DEVICE_ID_RAZER_");
        let c = "static ssize_t razer_attr_write_matrix_effect_static(struct device *dev)\n{\n    switch (pid) {\n    case USB_DEVICE_ID_RAZER_TEST: // one static only\n        request = razer_chroma_extended_matrix_effect_static(VARSTORE, BACKLIGHT_LED, rgb);\n        request.transaction_id.id = 0x3F;\n        break;\n    }\n}\nstatic void razer_get_report_params(struct usb_device *usb_dev)\n{\n    switch (pid) {\n    case USB_DEVICE_ID_RAZER_TEST:\n        *report_index = 0x03;\n        *wait = TEST_WAIT_US;\n        break;\n    default:\n        *report_index = 0x01;\n        break;\n    }\n}\n";
        assert_eq!(openrazer_switch(c, "razer_attr_write_matrix_effect_static", &or_pids)[&0x024E], (true, 0x3F));
        let params = openrazer_report_params(c, "#define TEST_WAIT_US 4900\n", &or_pids);
        assert_eq!((params.get(0x024E), params.get(0x0001)), ((3, Some(4900)), (1, None)));
        let py = "x = 1\nclass RazerTest(_Keyboard):\n    USB_PID = 0x024E\n    MATRIX_DIMS = [6, 22]\n";
        assert_eq!(openrazer_dims(py)[&0x024E].1, Some((6, 22)));

        let js = "export function Name() { return \"Razer Test\"; }\nexport function ProductId() { return 0x024E; }\nexport function Validate(endpoint) { return endpoint.interface === 3; }\nfunction SendRow(idx) {\n  packet[2] = 0x3F;\n  packet[6] = 0x47;\n  packet[7] = 0x0F;\n  packet[8] = 0x03;\n}\n";
        let (ids, p) = signal_plugin("Razer_Test.js", js).unwrap();
        assert_eq!(ids, vec![0x024E]);
        assert_eq!((p.interface, p.row.map(|r| (r.0, r.1, r.2)), p.cols()), (Some(3), Some((0x0F, 0x3F, 0x47)), Some(22)));
        assert_eq!(human("RazerBlackWidowV3"), "Black Widow V3");
        assert_eq!(human("Razer Blackwidow V3"), "Blackwidow V3");
    }
}
