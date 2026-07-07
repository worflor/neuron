//! Automatic device synthesis — the generalization of the hand-written `devices/*.toml`s.
//!
//! The two builtin defs were reverse-engineered by probing the `razer_report` getter space
//! and watching the device answer SUCCESS / UNSUPPORTED per command. That loop is mechanical,
//! so this module runs it automatically: probe an unknown Razer control pipe against the
//! UNIVERSAL COMMAND CATALOG (the union of the proven builtin specs — OpenRazer-derived,
//! hardware-verified on the Naga V2 Pro + BlackWidow Chroma V2), keep exactly the commands the
//! device says yes to, detect the lighting dialect from which lighting getters answer, measure
//! the link's real round-trip to pick `stream_wait_us`, and assemble a complete [`DeviceDef`].
//!
//! The synthesized def is then written to `devices/auto/razer-<pid>.toml` — from that moment
//! it is ORDINARY CONFIG, loaded by [`crate::registry::Registry::load`] like any other file,
//! editable and (by copying into `devices/`) promotable to a curated def that shadows it.
//! Discovery happens once per device; the file is the memo.
//!
//! What synthesis intentionally does NOT do:
//! * no writes — the probe is getter-only (ids ≥ 0x80 / documented read opcodes). Setters are
//!   emitted only when their PAIRED getter answered, mirroring how the builtins were proven.
//! * no capture-only commands — `set_scroll_stage` (wire-captured) and `[side_plates]`
//!   (push-report map) can't be discovered by asking; they stay per-device curated config.
//! * no transaction-id brute force — writes don't echo honesty (the Chroma V2's wrong-tx
//!   lighting writes ACK'd then no-op'd), so tx comes from the era heuristic below and is a
//!   one-line config fix when a board disagrees.

use crate::lighting::{LightingDef, Protocol};
use crate::registry::{CommandSpec, ControlInterface, DeviceDef, Mode, Registry};
use crate::transport::{self, HidDeviceInfo, Transport};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Razer's USB vendor id — the only vendor this protocol applies to.
pub const RAZER_VID: u16 = 0x1532;
/// The universal `razer_report` control-pipe signature: a 91-byte feature report.
pub const RAZER_FEATURE_LEN: u16 = 91;

/// Probe transaction id. Getters ignore the tx field (proven live on both eras), so any
/// value works for the read-only probe; 0x1F matches `discover`.
const PROBE_TX: u8 = 0x1F;

/// A setter emitted alongside its proven getter (never probed — writes are not probes).
struct Setter {
    name: &'static str,
    class: u8,
    id: u8,
    size: u8,
}

/// One universal-catalog getter: probed read-only; if the device answers SUCCESS the getter
/// (with `args` baked in, exactly as the builtins bake them) and its paired setters join the
/// synthesized command map. Specs are byte-identical to the hand-verified builtin TOMLs.
struct CatalogEntry {
    name: &'static str,
    class: u8,
    id: u8,
    size: u8,
    args: &'static [u8],
    setters: &'static [Setter],
}

/// The universal command catalog — the union of the builtin defs' command maps.
const CATALOG: &[CatalogEntry] = &[
    CatalogEntry { name: "firmware_version", class: 0x00, id: 0x81, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "serial", class: 0x00, id: 0x82, size: 0x16, args: &[], setters: &[] },
    CatalogEntry { name: "device_mode", class: 0x00, id: 0x84, size: 0x02, args: &[], setters: &[] },
    CatalogEntry {
        name: "polling_rate", class: 0x00, id: 0x85, size: 0x01, args: &[],
        setters: &[Setter { name: "set_polling", class: 0x00, id: 0x05, size: 0x01 }],
    },
    CatalogEntry {
        name: "polling2", class: 0x00, id: 0xC0, size: 0x01, args: &[],
        setters: &[Setter { name: "set_polling2", class: 0x00, id: 0x40, size: 0x02 }],
    },
    CatalogEntry {
        name: "dpi", class: 0x04, id: 0x85, size: 0x07, args: &[],
        setters: &[Setter { name: "set_dpi", class: 0x04, id: 0x05, size: 0x07 }],
    },
    CatalogEntry {
        name: "dpi_stages", class: 0x04, id: 0x83, size: 0x26, args: &[],
        setters: &[Setter { name: "set_dpi_stages", class: 0x04, id: 0x06, size: 0x26 }],
    },
    CatalogEntry { name: "dpi_stages_active", class: 0x04, id: 0x86, size: 0x26, args: &[], setters: &[] },
    CatalogEntry { name: "battery_level", class: 0x07, id: 0x80, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "charging_status", class: 0x07, id: 0x84, size: 0x02, args: &[], setters: &[] },
    CatalogEntry { name: "storage_info", class: 0x06, id: 0x8E, size: 0x20, args: &[], setters: &[] },
    CatalogEntry { name: "storage_counts", class: 0x06, id: 0x80, size: 0x20, args: &[], setters: &[] },
    CatalogEntry { name: "storage_directory", class: 0x06, id: 0x8D, size: 0x20, args: &[], setters: &[] },
    // Matrix-era lighting getters (class 0x0F). brightness reads the VISIBLE region 0x04 —
    // the same region set_brightness writes (the read/write-region mismatch was a live bug).
    CatalogEntry {
        name: "brightness", class: 0x0F, id: 0x84, size: 0x03, args: &[0x00, 0x04],
        setters: &[Setter { name: "set_brightness", class: 0x0F, id: 0x04, size: 0x03 }],
    },
    CatalogEntry { name: "lighting_state", class: 0x0F, id: 0x82, size: 0x20, args: &[], setters: &[] },
    // Legacy-era lighting getters (class 0x03). `lighting_state` intentionally reuses the
    // matrix entry's name — first-answering dialect wins the slot (matrix is probed first).
    CatalogEntry { name: "lighting_state", class: 0x03, id: 0x88, size: 0x06, args: &[], setters: &[] },
    CatalogEntry { name: "lighting_caps", class: 0x03, id: 0x89, size: 0x07, args: &[], setters: &[] },
];

/// OpenRazer's per-receiver SET→GET wait for "new mouse receiver" wireless dongles (µs).
const WIRELESS_STREAM_WAIT_US: u64 = 31_000;
/// Round-trips at/above this are a slow (wireless) link that needs the stream wait.
const WIRELESS_ROUNDTRIP_MS: u64 = 15;

/// `stream_wait_us` for the synthesized def. A wireless link overruns without pacing —
/// streamed lighting writes outrun the host→dongle→device round-trip and DROP frames (the
/// FLICKER) — so it gets OpenRazer's receiver wait. Wirelessness is detected two ways,
/// either sufficing, because the LIVE-VERIFIED failure mode of latency alone is a dongle
/// that ACKs getters in ~6ms yet still needs the write pacing (the Naga's receiver did
/// exactly that):
///   * a slow measured round-trip (the direct evidence), or
///   * a battery (`wireless_capable`) — a battery-bearing board is built to run wireless,
///     and the wait on a wired link only lowers the stream fps ceiling, never corrupts;
///     flicker-by-default on a real dongle would be the dishonest failure mode.
pub fn stream_wait_for(roundtrip_ms: u64, wireless_capable: bool) -> u64 {
    if roundtrip_ms >= WIRELESS_ROUNDTRIP_MS || wireless_capable {
        WIRELESS_STREAM_WAIT_US
    } else {
        0
    }
}

/// Per-command probe window (2ms polls): ~320ms covers the slowest awake wireless round-trip.
const PROBE_POLLS: usize = 160;
/// Qualify window: the FIRST command to the device also has to wake a parked wireless radio,
/// which takes host traffic on the order of seconds — give it ~2s of re-armed polling. (A
/// DEEP-asleep mouse only wakes on user input; no window fixes that — the retry paths do.)
const QUALIFY_POLLS: usize = 1000;

/// Send one getter and poll (2ms grain, up to `max_polls`) for its echoed reply. Returns the
/// args and how long the round-trip took. Finer-grained than `discover::exec` because the
/// measured latency FEEDS `stream_wait_us` — 8ms polling would read every wired board as ~8ms.
fn timed_exec(
    t: &dyn Transport,
    class: u8,
    id: u8,
    size: u8,
    args: &[u8],
    max_polls: usize,
) -> Option<([u8; 80], Duration)> {
    use crate::protocol::{Report, Status, BUF_LEN};
    let mut req = Report::command(PROBE_TX, class, id, size);
    for (i, b) in args.iter().enumerate() {
        if i < req.args.len() {
            req.args[i] = *b;
        }
    }
    let out = req.to_buf();
    let start = Instant::now();
    t.set_feature(&out).ok()?;
    for i in 0..max_polls {
        std::thread::sleep(Duration::from_millis(2));
        let mut b = [0u8; BUF_LEN];
        if t.get_feature(&mut b).is_ok() && b[7] == class && b[8] == id {
            match Status::from_u8(b[1]) {
                Status::Success => return Some((Report::from_buf(&b).args, start.elapsed())),
                Status::Fail | Status::Unsupported => return None,
                _ => {}
            }
        }
        if i % 48 == 47 {
            let _ = t.set_feature(&out); // re-arm a busy/slow/waking wireless link
        }
    }
    None
}

/// Everything synthesis needs to know about the pipe besides the transport itself.
pub struct SynthCtx {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    /// USB product string (may be empty) — becomes the def's honest name.
    pub product: String,
}

impl SynthCtx {
    pub fn from_info(i: &HidDeviceInfo) -> Self {
        SynthCtx {
            vid: i.vid,
            pid: i.pid,
            usage_page: i.usage_page,
            usage: i.usage,
            product: i.product.clone(),
        }
    }
}

/// A synthesized device: the def plus the probe provenance that goes into the emitted file.
pub struct Synthesis {
    pub def: DeviceDef,
    /// Device serial (when the serial getter answered) — file-comment provenance.
    pub serial: String,
    /// Median measured command round-trip (ms) — the basis of `stream_wait_us`.
    pub roundtrip_ms: u64,
}

fn cmd(class: u8, id: u8, size: u8, args: &[u8]) -> CommandSpec {
    CommandSpec {
        class,
        id,
        size,
        args: args.to_vec(),
        transaction_id: None,
    }
}

fn cmd_tx(class: u8, id: u8, size: u8, args: &[u8], tx: u8) -> CommandSpec {
    CommandSpec {
        transaction_id: Some(tx),
        ..cmd(class, id, size, args)
    }
}

/// The matrix (extended, class 0x0F) lighting block — Naga-proven values. `custom_id` MUST be
/// 0x08 on this era (0x05 is a real native effect there — REACTIVE on the Naga — and using it
/// as the display id painted-then-un-painted every streamed frame).
fn matrix_lighting(rows: u8, cols: u8, brightness_proven: bool) -> LightingDef {
    LightingDef {
        protocol: Protocol::Matrix,
        rows,
        cols,
        varstore: 0x00,
        led_id: 0x04,
        effects: [
            ("off", 0x00u8),
            ("static", 0x01),
            ("breathing", 0x02),
            ("wave", 0x03),
            ("spectrum", 0x04),
            ("reactive", 0x05),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        custom_id: 0x08,
        effect: cmd(0x0F, 0x02, 0x0C, &[0x00, 0x04]),
        custom_frame: Some(cmd(0x0F, 0x03, 0x40, &[0x00, 0x04])),
        // The matrix dialect HAS a brightness getter (0x0F/0x84 — the same probe that gates the
        // top-level `set_brightness` command), so the block's brightness rides that SAME evidence:
        // emit it only when the getter answered. `lighting.brightness` is now a WRITE path
        // (`DeviceDef::supports(SetBrightness)` reads it), so an unanswered getter must mean NO
        // brightness write anywhere in the def — leave it `None` and `supports(SetBrightness)`
        // honestly reads false rather than promising an unproven write.
        brightness: brightness_proven.then(|| cmd(0x0F, 0x04, 0x03, &[0x00, 0x04])),
    }
}

/// The legacy (standard, class 0x03) lighting block — Chroma-V2-proven values, including the
/// split-brain tx: getters ride the device default while EFFECT and CUSTOM-FRAME writes need
/// 0x3F (at the default they ACK and silently no-op). No brightness — see the field comment.
fn legacy_lighting(rows: u8, cols: u8) -> LightingDef {
    LightingDef {
        protocol: Protocol::Legacy,
        rows,
        cols,
        varstore: 0x00,
        led_id: 0x00,
        effects: [
            ("off", 0x00u8),
            ("wave", 0x01),
            ("reactive", 0x02),
            ("breathing", 0x03),
            ("spectrum", 0x04),
            ("static", 0x06),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
        custom_id: 0x05,
        effect: cmd_tx(0x03, 0x0A, 0x08, &[], 0x3F),
        custom_frame: Some(cmd_tx(0x03, 0x0B, 0x46, &[0xFF], 0x3F)),
        // No brightness: the legacy dialect has NO brightness getter to prove a setter against
        // (the curated BlackWidow's 0x03/0x03 write was verified on that physical board —
        // per-device evidence a blind probe cannot produce). Now that `lighting.brightness`
        // GRANTS a write capability, baking the standard 0x03/0x03 spec here would put unproven
        // bytes on the wire for every legacy board synthesis touches. A user who KNOWS their
        // legacy board takes the standard write adds `[lighting.brightness]` to the auto file by
        // hand — it is config.
        brightness: None,
    }
}

/// Probe one Razer control pipe and synthesize its complete [`DeviceDef`]. Read-only.
/// `None` when the pipe never answers the firmware getter (not a talking razer_report pipe —
/// e.g. a secondary collection that enumerates with the right shape but stays mute).
pub fn synthesize(t: &dyn Transport, ctx: &SynthCtx) -> Option<Synthesis> {
    // Wake + qualify: the first command to a sleeping wireless device can take an outlier
    // round-trip (it also has to wake the radio — the long QUALIFY window), so it qualifies
    // the pipe but doesn't count toward the latency sample.
    timed_exec(t, 0x00, 0x81, 0x02, &[], QUALIFY_POLLS)?;

    // Measure the link: median of three firmware round-trips.
    let mut samples: Vec<u64> = (0..3)
        .filter_map(|_| {
            timed_exec(t, 0x00, 0x81, 0x02, &[], PROBE_POLLS).map(|(_, d)| d.as_millis() as u64)
        })
        .collect();
    samples.sort_unstable();
    let roundtrip_ms = samples.get(samples.len() / 2).copied().unwrap_or(0);

    // Catalog probe: keep what answers. First-wins on name collisions (the two eras' state
    // getters share "lighting_state"; matrix precedes legacy in the catalog).
    let mut commands: BTreeMap<String, CommandSpec> = BTreeMap::new();
    let mut serial = String::new();
    let mut matrix = false;
    let mut legacy = false;
    for e in CATALOG {
        let Some((args, _)) = timed_exec(t, e.class, e.id, e.size, e.args, PROBE_POLLS) else {
            continue;
        };
        if e.name == "serial" {
            serial = args
                .iter()
                .take_while(|&&b| (0x20..0x7F).contains(&b))
                .map(|&b| b as char)
                .collect();
        }
        match (e.class, e.id) {
            (0x0F, _) => matrix = true,
            (0x03, _) => legacy = true,
            _ => {}
        }
        commands
            .entry(e.name.to_string())
            .or_insert_with(|| cmd(e.class, e.id, e.size, e.args));
        for s in e.setters {
            commands
                .entry(s.name.to_string())
                .or_insert_with(|| cmd(s.class, s.id, s.size, &[]));
        }
    }

    // Lighting dialect: matrix getters answering ⇒ extended matrix; else legacy getters ⇒
    // standard/legacy. Matrix wins when both answer (never observed; 0x0F supersedes 0x03).
    // rows×cols is the one thing a getter can't reveal — HEURISTIC (a DPI class = a mouse),
    // flagged loudly in the emitted file. Wrong dims degrade custom frames (partial paint),
    // never native effects, which are whole-device.
    let has_dpi = commands.contains_key("dpi");
    let lighting = if matrix {
        let (rows, cols) = if has_dpi { (1, 2) } else { (6, 22) };
        // "brightness" is the matrix getter's registry name — inserted above only when 0x0F/0x84
        // answered — so it is the exact probe evidence the block's brightness write must ride.
        Some(matrix_lighting(rows, cols, commands.contains_key("brightness")))
    } else if legacy {
        Some(legacy_lighting(6, 22))
    } else {
        None
    };

    // Era tx heuristic: matrix-era boards take 0x1F device-wide (Naga-proven); legacy-only
    // boards default 0xFF with the 0x3F lighting override baked into the lighting block
    // (Chroma-V2-proven). Getters ignore tx either way — only writes can disagree, and the
    // emitted file says exactly which knob to turn.
    let transaction_id = if matrix || !legacy { 0x1F } else { 0xFF };

    // The name mirrors the emitted file, which is the source of truth: a TOML basic string can't
    // carry control bytes, so `emit_toml` maps them to spaces — strip them here too (via the same
    // control→space rule) so the in-memory def equals what a reload of its own file would yield.
    // Quotes/backslashes are KEPT (they round-trip losslessly through `toml_escape`).
    let name = if ctx.product.trim().is_empty() {
        format!("Razer device {:04x} (auto)", ctx.pid)
    } else {
        comment_safe(ctx.product.trim())
    };
    let wireless_capable = commands.contains_key("battery_level");
    let def = DeviceDef {
        name,
        codename: format!("auto-{:04x}", ctx.pid),
        vendor_id: ctx.vid,
        transaction_id,
        stream_wait_us: stream_wait_for(roundtrip_ms, wireless_capable),
        modes: vec![Mode {
            name: "default".into(),
            product_id: ctx.pid,
        }],
        control_interface: ControlInterface {
            usage_page: ctx.usage_page,
            usage: ctx.usage,
            feature_report_len: RAZER_FEATURE_LEN,
        },
        commands,
        lighting,
        side_plates: None,
    };
    Some(Synthesis {
        def,
        serial,
        roundtrip_ms,
    })
}

/// Escape a string for a TOML basic-string value (`"..."`). Backslash and quote get their
/// TOML escapes; every ASCII control byte (`c < 0x20` or DEL) becomes a single space.
///
/// A USB product string is attacker-adjacent junk — a device is free to report a name carrying
/// a quote, a backslash, or raw control bytes, and interpolating that raw into `name = "{}"`
/// emits INVALID TOML. That is the worst failure this module has: [`Registry::load`] silently
/// skips the unparseable file, yet [`adopt_unknown`] sees `auto_def_path().exists()` and refuses
/// to clobber it — so the device is PERMANENTLY un-adopted until the user hand-edits the file.
/// A control byte in a name is garbage regardless, so a space keeps the file readable rather
/// than round-tripping junk; the FILE is the source of truth and must stay valid.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7F}' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Make a string safe to interpolate INTO a `# ...` header comment line. A `\r`/`\n` here would
/// break out of the comment and turn the remainder of the product string (or serial) into stray
/// top-level TOML — corrupting the file exactly like an unescaped value does. Every control byte
/// collapses to a space (same reasoning as [`toml_escape`]: it stays a comment, stays readable).
fn comment_safe(s: &str) -> String {
    s.chars()
        .map(|c| if (c as u32) < 0x20 || c == '\u{7F}' { ' ' } else { c })
        .collect()
}

fn emit_cmd(out: &mut String, table: &str, c: &CommandSpec) {
    out.push_str(&format!("[{table}]\nclass = 0x{:02X}\nid = 0x{:02X}\nsize = 0x{:02X}\n", c.class, c.id, c.size));
    if !c.args.is_empty() {
        let args: Vec<String> = c.args.iter().map(|b| format!("0x{b:02X}")).collect();
        out.push_str(&format!("args = [{}]\n", args.join(", ")));
    }
    if let Some(tx) = c.transaction_id {
        out.push_str(&format!("transaction_id = 0x{tx:02X}\n"));
    }
    out.push('\n');
}

/// Serialize a synthesized def to the same commented-TOML shape as the curated files. The
/// output parses back into an identical [`DeviceDef`] (round-trip tested) — from here on
/// it's plain config the user owns.
pub fn emit_toml(s: &Synthesis) -> String {
    let d = &s.def;
    let mut out = String::new();
    out.push_str(&format!(
        "# Device definition — {} (AUTO-SYNTHESIZED by probing the device's razer_report\n\
         # getter space; every command below answered SUCCESS on the real hardware).\n",
        comment_safe(&d.name)
    ));
    if !s.serial.is_empty() {
        out.push_str(&format!("# Probed unit serial: {}\n", comment_safe(&s.serial)));
    }
    out.push_str(&format!(
        "# Measured command round-trip: ~{}ms -> stream_wait_us = {}.\n\
         # This file is CONFIG, not discovery: edit freely (it is never overwritten), or copy\n\
         # it into devices/ to promote it to a curated def that shadows this one.\n\
         # HEURISTIC fields worth checking on a new board:\n\
         #   - transaction_id: era-derived, not probe-proven (writes don't echo honesty).\n\
         #   - [lighting] rows/cols: a GUESS — native effects don't need it, custom frames do.\n\
         #   - [lighting.brightness]: emitted ONLY when the probe proved its getter. Legacy-dialect\n\
         #     defs omit it by design (no brightness getter exists to prove the write against) —\n\
         #     add [lighting.brightness] by hand if the board is known to take the standard write.\n\
         #   - stream_wait_us: battery-bearing boards get the wireless write pacing even on a\n\
         #     fast link (a dongle ACKs getters quickly yet still drops streamed frames without\n\
         #     it); set it to 0 for a permanently-wired board to raise the stream fps ceiling.\n\n",
        s.roundtrip_ms, d.stream_wait_us
    ));
    out.push_str(&format!("name = \"{}\"\n", toml_escape(&d.name)));
    out.push_str(&format!("codename = \"{}\"\n", toml_escape(&d.codename)));
    out.push_str(&format!("vendor_id = 0x{:04X}\n", d.vendor_id));
    out.push_str(&format!("transaction_id = 0x{:02X}\n", d.transaction_id));
    if d.stream_wait_us > 0 {
        out.push_str(&format!("stream_wait_us = {}\n", d.stream_wait_us));
    }
    out.push('\n');
    for m in &d.modes {
        out.push_str(&format!(
            "[[modes]]\nname = \"{}\"\nproduct_id = 0x{:04X}\n\n",
            toml_escape(&m.name), m.product_id
        ));
    }
    out.push_str(&format!(
        "[control_interface]\nusage_page = 0x{:04X}\nusage = 0x{:04X}\nfeature_report_len = {}\n\n",
        d.control_interface.usage_page, d.control_interface.usage, d.control_interface.feature_report_len
    ));
    for (name, c) in &d.commands {
        emit_cmd(&mut out, &format!("commands.{name}"), c);
    }
    if let Some(l) = &d.lighting {
        let proto = match l.protocol {
            Protocol::Matrix => "matrix",
            Protocol::Legacy => "legacy",
        };
        out.push_str(&format!(
            "[lighting]\nprotocol = \"{proto}\"\nrows = {}\ncols = {}   # HEURISTIC guess — refine for full custom-frame coverage\nvarstore = 0x{:02X}\nled_id = 0x{:02X}\ncustom_id = 0x{:02X}\n\n",
            l.rows, l.cols, l.varstore, l.led_id, l.custom_id
        ));
        out.push_str("[lighting.effects]\n");
        for (k, v) in &l.effects {
            out.push_str(&format!("{k} = 0x{v:02X}\n"));
        }
        out.push('\n');
        emit_cmd(&mut out, "lighting.effect", &l.effect);
        if let Some(cf) = &l.custom_frame {
            emit_cmd(&mut out, "lighting.custom_frame", cf);
        }
        if let Some(b) = &l.brightness {
            emit_cmd(&mut out, "lighting.brightness", b);
        }
    }
    out
}

/// One device adopted by [`adopt_unknown`].
pub struct Adopted {
    pub pid: u16,
    pub name: String,
    pub path: PathBuf,
}

/// The outcome of one [`adopt_unknown`] pass. Per-device problems land in `skipped` (with the
/// reason) instead of aborting the pass — one broken file or unwritable path must never stop a
/// DIFFERENT new device from being adopted.
#[derive(Default)]
pub struct Adoption {
    pub adopted: Vec<Adopted>,
    pub skipped: Vec<String>,
}

/// Where an auto-synthesized def for `pid` lives (relative, like every other config path).
pub fn auto_def_path(pid: u16) -> PathBuf {
    PathBuf::from("devices")
        .join("auto")
        .join(format!("razer-{pid:04x}.toml"))
}

/// Find every connected Razer `razer_report` pipe the registry does NOT recognize, synthesize
/// a def for it, and write `devices/auto/razer-<pid>.toml`. Existing files are never
/// overwritten (they are the user's config now — a present-but-unloadable one is reported in
/// `skipped` instead). Per-device failures skip that device only; `Err` is reserved for HID
/// enumeration itself failing. Reload the registry to pick the new files up.
pub fn adopt_unknown(reg: &Registry) -> Result<Adoption> {
    adopt_filtered(reg, None)
}

/// Like [`adopt_unknown`], but adopt ONLY the given pids — any other unknown pid seen in
/// enumeration is left untouched. The app's background workers use this: a worker spawned for
/// one pid must not also probe a DIFFERENT pid that another worker is already adopting, or the
/// two race the `auto_def_path().exists()` check and double-write the same file. Constraining
/// each worker to its own pids is the single-writer half of the auto-file ownership rule (the
/// single-probe half is the caller's spawn guard).
pub fn adopt_pids(reg: &Registry, pids: &[u16]) -> Result<Adoption> {
    adopt_filtered(reg, Some(pids))
}

/// Shared adoption pass. `only = None` adopts every unrecognized Razer pipe; `only = Some(pids)`
/// restricts the pass to those pids (skipping any enumerated pid not in the slice) so concurrent
/// callers never overlap on a pid. Logic is otherwise identical for both wrappers.
fn adopt_filtered(reg: &Registry, only: Option<&[u16]>) -> Result<Adoption> {
    let infos = transport::enumerate().context("HID enumeration failed")?;
    let mut out = Adoption::default();
    let mut tried: Vec<u16> = Vec::new();
    for i in &infos {
        if i.vid != RAZER_VID || i.feature_len != RAZER_FEATURE_LEN {
            continue;
        }
        // Filtered pass: leave pids this caller wasn't asked to adopt to whoever owns them.
        if only.is_some_and(|pids| !pids.contains(&i.pid)) {
            continue;
        }
        if reg.find_by_pid(i.vid, i.pid).is_some() || tried.contains(&i.pid) {
            continue;
        }
        let file = auto_def_path(i.pid);
        if file.exists() {
            // The registry didn't match it, yet its auto file exists — the file is broken or
            // shadow-named. Never clobber user-owned config; report and move on.
            tried.push(i.pid);
            out.skipped.push(format!(
                "pid {:04x}: {} exists but the registry does not resolve it — fix or delete that file",
                i.pid,
                file.display()
            ));
            continue;
        }
        // Several collections share one pid; probe pipes until one talks. Only a talking
        // pipe marks the pid tried — a mute vendor collection must not veto its sibling.
        let Ok(t) = transport::open_path(&i.path) else {
            continue;
        };
        let Some(synth) = synthesize(&*t, &SynthCtx::from_info(i)) else {
            continue;
        };
        tried.push(i.pid);
        let write = std::fs::create_dir_all(file.parent().expect("auto path has a parent"))
            .and_then(|()| std::fs::write(&file, emit_toml(&synth)));
        match write {
            Ok(()) => out.adopted.push(Adopted {
                pid: i.pid,
                name: synth.def.name.clone(),
                path: file,
            }),
            Err(e) => out
                .skipped
                .push(format!("pid {:04x}: writing {} failed: {e}", i.pid, file.display())),
        }
    }
    Ok(out)
}

/// Are there connected Razer control pipes the registry doesn't recognize? Cheap (one HID
/// enumeration, no device I/O) — the app's scan tick uses this to decide whether adoption
/// is worth spawning at all.
pub fn unknown_present(reg: &Registry) -> Vec<u16> {
    let Ok(infos) = transport::enumerate() else {
        return Vec::new();
    };
    let mut pids: Vec<u16> = infos
        .iter()
        .filter(|i| i.vid == RAZER_VID && i.feature_len == RAZER_FEATURE_LEN)
        .filter(|i| reg.find_by_pid(i.vid, i.pid).is_none())
        .map(|i| i.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Report, BUF_LEN};
    use std::sync::Mutex;

    /// A scripted razer_report device: answers SUCCESS (with canned args) for the (class, id)
    /// pairs it "supports", UNSUPPORTED for everything else — the same yes/no contract the
    /// real firmware gives the probe.
    struct MockDevice {
        supports: Vec<(u8, u8, [u8; 80])>,
        last: Mutex<Option<(u8, u8)>>,
    }

    impl MockDevice {
        fn new(cmds: &[(u8, u8)]) -> Self {
            MockDevice {
                supports: cmds.iter().map(|&(c, i)| (c, i, [0u8; 80])).collect(),
                last: Mutex::new(None),
            }
        }
        fn with_args(mut self, class: u8, id: u8, args: &[u8]) -> Self {
            for (c, i, a) in &mut self.supports {
                if *c == class && *i == id {
                    a[..args.len()].copy_from_slice(args);
                }
            }
            self
        }
    }

    impl Transport for MockDevice {
        fn set_feature(&self, buf: &[u8]) -> anyhow::Result<()> {
            *self.last.lock().unwrap() = Some((buf[7], buf[8]));
            Ok(())
        }
        fn get_feature(&self, buf: &mut [u8]) -> anyhow::Result<()> {
            let Some((class, id)) = *self.last.lock().unwrap() else {
                anyhow::bail!("no pending command");
            };
            let mut rep = Report::command(PROBE_TX, class, id, 0);
            match self.supports.iter().find(|(c, i, _)| *c == class && *i == id) {
                Some((_, _, args)) => {
                    rep.status = 0x02; // SUCCESS
                    rep.args = *args;
                }
                None => rep.status = 0x05, // UNSUPPORTED
            }
            let out = rep.to_buf();
            let n = buf.len().min(BUF_LEN);
            buf[..n].copy_from_slice(&out[..n]);
            Ok(())
        }
    }

    fn ctx(pid: u16, product: &str) -> SynthCtx {
        SynthCtx {
            vid: RAZER_VID,
            pid,
            usage_page: 0x0001,
            usage: 0x0002,
            product: product.into(),
        }
    }

    /// A matrix-era wireless mouse shaped like the Naga V2 Pro (getter space only).
    fn mock_matrix_mouse() -> MockDevice {
        MockDevice::new(&[
            (0x00, 0x81), (0x00, 0x82), (0x00, 0x84), (0x00, 0x85), (0x00, 0xC0),
            (0x04, 0x85), (0x04, 0x83), (0x04, 0x86),
            (0x07, 0x80), (0x07, 0x84),
            (0x06, 0x8E), (0x06, 0x80), (0x06, 0x8D),
            (0x0F, 0x84), (0x0F, 0x82),
        ])
        .with_args(0x00, 0x82, b"UNIT12345SERIAL")
    }

    /// A legacy-era wired keyboard shaped like the BlackWidow Chroma V2.
    fn mock_legacy_keyboard() -> MockDevice {
        MockDevice::new(&[(0x00, 0x81), (0x00, 0x82), (0x00, 0x84), (0x03, 0x88), (0x03, 0x89)])
    }

    #[test]
    fn synthesizes_matrix_mouse_matching_the_curated_naga_def() {
        let s = synthesize(&mock_matrix_mouse(), &ctx(0x00A8, "Razer Naga V2 Pro")).unwrap();
        let d = &s.def;
        // The curated Naga def is the ground truth this synthesis must reproduce.
        let naga: DeviceDef =
            toml::from_str(include_str!("../devices/razer-naga-v2-pro.toml")).unwrap();
        for name in [
            "firmware_version", "serial", "device_mode", "polling_rate", "set_polling",
            "polling2", "set_polling2", "dpi", "set_dpi", "dpi_stages", "set_dpi_stages",
            "dpi_stages_active", "battery_level", "charging_status", "storage_info",
            "brightness", "set_brightness", "lighting_state",
        ] {
            let (a, b) = (d.command(name), naga.command(name));
            let a = a.unwrap_or_else(|| panic!("synth missing '{name}'"));
            let b = b.unwrap_or_else(|| panic!("curated missing '{name}'"));
            assert_eq!(
                (a.class, a.id, a.size, &a.args),
                (b.class, b.id, b.size, &b.args),
                "spec mismatch for '{name}'"
            );
        }
        assert_eq!(d.transaction_id, naga.transaction_id, "matrix era tx");
        let (l, nl) = (d.lighting.as_ref().unwrap(), naga.lighting.as_ref().unwrap());
        assert_eq!(l.protocol, Protocol::Matrix);
        assert_eq!((l.led_id, l.custom_id, l.varstore), (nl.led_id, nl.custom_id, nl.varstore));
        assert_eq!(l.effects, nl.effects);
        assert_eq!(l.effect, nl.effect);
        assert_eq!(l.custom_frame, nl.custom_frame);
        assert_eq!(l.brightness, nl.brightness);
        assert_eq!((l.rows, l.cols), (nl.rows, nl.cols), "dpi-present heuristic = mouse dims");
        assert_eq!(d.name, "Razer Naga V2 Pro");
        assert_eq!(s.serial, "UNIT12345SERIAL");
        // Battery-bearing = wireless-capable -> gets the receiver pacing even though the mock
        // answers instantly (the live Naga dongle ACKs getters in ~6ms yet still flickers
        // without it). Must equal the curated def's hardware-calibrated value.
        assert_eq!(d.stream_wait_us, naga.stream_wait_us);
        // Capture-only knowledge is NOT synthesized — it stays curated config.
        assert!(d.command("set_scroll_stage").is_none());
        assert!(d.side_plates.is_none());
    }

    #[test]
    fn synthesizes_legacy_keyboard_matching_the_curated_blackwidow_def() {
        use crate::registry::Capability;
        let s = synthesize(&mock_legacy_keyboard(), &ctx(0x0221, "")).unwrap();
        let d = &s.def;
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        assert_eq!(d.transaction_id, bw.transaction_id, "legacy default tx 0xFF");
        let (l, bl) = (d.lighting.as_ref().unwrap(), bw.lighting.as_ref().unwrap());
        assert_eq!(l.protocol, Protocol::Legacy);
        assert_eq!(l.effects, bl.effects);
        assert_eq!(l.effect, bl.effect, "split-brain 0x3F lighting tx");
        assert_eq!(l.custom_frame, bl.custom_frame);
        // The legacy dialect has NO brightness getter, so the probe can't prove a brightness
        // write — the synthesized block omits it even though the CURATED def keeps Some (that
        // 0x03/0x03 spec was verified on the physical board, per-device evidence a blind probe
        // can't reproduce). Since `lighting.brightness` now grants SetBrightness, emitting it
        // untested would put unproven bytes on the wire; without it the def honestly declines.
        assert!(
            l.brightness.is_none(),
            "legacy synthesis has no getter to prove a brightness write against"
        );
        assert!(bl.brightness.is_some(), "curated BW keeps its board-verified brightness write");
        assert!(
            !d.supports(Capability::SetBrightness),
            "no proven brightness getter -> no brightness write capability"
        );
        // legacy lighting_state resolves to the class-0x03 getter (no matrix on this board)
        let st = d.command("lighting_state").unwrap();
        assert_eq!((st.class, st.id), (0x03, 0x88));
        // no product string -> honest placeholder name
        assert_eq!(d.name, "Razer device 0221 (auto)");
        // nothing mouse-shaped leaked in
        assert!(d.command("dpi").is_none());
        assert!(d.command("battery_level").is_none());
    }

    #[test]
    fn matrix_without_brightness_getter_gets_no_brightness_write() {
        // The exact review scenario: a matrix board whose lighting_state getter (0x0F/0x82)
        // answers — so the dialect is Matrix and there IS a lighting block — but whose brightness
        // getter (0x0F/0x84) does NOT. The brightness getter gates the top-level `set_brightness`
        // command AND (now) the block's brightness write, so an unproven getter must leave BOTH
        // absent, and `supports(SetBrightness)` must read false. Emitting the block's brightness
        // on era-inference alone (the old behaviour) put unproven bytes behind that capability.
        use crate::registry::Capability;
        let mock = MockDevice::new(&[(0x00, 0x81), (0x0F, 0x82)]);
        let s = synthesize(&mock, &ctx(0x1234, "Razer matrix (no brightness getter)")).unwrap();
        let d = &s.def;
        let l = d.lighting.as_ref().expect("lighting_state answered -> a matrix block");
        assert_eq!(l.protocol, Protocol::Matrix);
        assert!(d.command("set_brightness").is_none(), "unproven getter -> no setter");
        assert!(l.brightness.is_none(), "unproven getter -> no block brightness write");
        assert!(
            !d.supports(Capability::SetBrightness),
            "no brightness write path anywhere -> supports(SetBrightness) is false"
        );
    }

    #[test]
    fn mute_pipe_synthesizes_nothing() {
        let mock = MockDevice::new(&[]); // answers UNSUPPORTED to everything
        assert!(synthesize(&mock, &ctx(0x9999, "x")).is_none());
    }

    #[test]
    fn emitted_toml_round_trips_to_the_same_def() {
        for (mock, pid, product) in [
            (mock_matrix_mouse(), 0x00A8u16, "Razer Naga V2 Pro"),
            (mock_legacy_keyboard(), 0x0221, ""),
        ] {
            let s = synthesize(&mock, &ctx(pid, product)).unwrap();
            let text = emit_toml(&s);
            let parsed: DeviceDef = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("emitted TOML must parse: {e}\n---\n{text}"));
            assert_eq!(parsed, s.def, "emit → parse must be lossless");
        }
    }

    #[test]
    fn toml_escape_neutralizes_toml_metacharacters() {
        // quote and backslash get their TOML escapes (they round-trip losslessly)…
        assert_eq!(toml_escape("a\"b"), "a\\\"b");
        assert_eq!(toml_escape("a\\b"), "a\\\\b");
        // …control bytes become a space (a name carrying them is garbage — keep the file readable).
        assert_eq!(toml_escape("a\nb"), "a b");
        assert_eq!(toml_escape("a\tb"), "a b");
        assert_eq!(toml_escape("a\u{7F}b"), "a b");
        // the escaped body must actually parse as a TOML basic string.
        let v: toml::Value = format!("k = \"{}\"", toml_escape("x\"y\\z\nw")).parse().unwrap();
        assert_eq!(v["k"].as_str().unwrap(), "x\"y\\z w");
    }

    #[test]
    fn emitted_toml_survives_a_hostile_product_string() {
        // A device is free to report a name carrying quotes, a backslash, and raw control bytes.
        // Interpolated raw, that name emits INVALID TOML — Registry::load then silently skips the
        // file while adopt_unknown refuses to clobber it, leaving the device permanently
        // un-adopted. The emitted file must PARSE, and the parsed def must equal the synthesized
        // one: the name's control byte is space-replaced in BOTH (the file is the source of truth,
        // and synthesize already mirrors what a reload of the file yields).
        let s = synthesize(
            &mock_matrix_mouse(),
            &ctx(0x00A8, "Weird \"Product\" \\ v2\nrev"),
        )
        .unwrap();
        assert_eq!(s.def.name, "Weird \"Product\" \\ v2 rev", "control byte → space, quotes/\\ kept");
        let text = emit_toml(&s);
        let parsed: DeviceDef = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("hostile product must still emit valid TOML: {e}\n---\n{text}"));
        assert_eq!(parsed, s.def, "emit → parse must be lossless even for a hostile name");
        // The `name = "…"` line must be whole — a raw newline in the value would split it, so a
        // found line that stops before its closing quote is the bug this fix prevents.
        let name_line = text
            .lines()
            .find(|l| l.starts_with("name = "))
            .expect("emitted file has a name line");
        assert!(name_line.ends_with('"'), "name value split by a raw newline: {name_line:?}");
    }

    #[test]
    fn stream_wait_tracks_link_and_wireless_capability() {
        assert_eq!(stream_wait_for(1, false), 0, "wired-class round-trip, no battery");
        assert_eq!(stream_wait_for(8, false), 0);
        assert_eq!(stream_wait_for(15, false), WIRELESS_STREAM_WAIT_US, "slow link alone");
        assert_eq!(stream_wait_for(32, false), WIRELESS_STREAM_WAIT_US);
        // battery-bearing board on a FAST link still gets the pacing (the live Naga case)
        assert_eq!(stream_wait_for(1, true), WIRELESS_STREAM_WAIT_US);
    }

    #[test]
    fn synthesized_commands_satisfy_the_capability_map() {
        // Every capability the mock mouse's getter space implies must resolve through the
        // SAME registry-driven gate the GUI/CLI use — synthesis feeds that gate, so the
        // names must line up exactly (a typo here would silently grey-out real features).
        use crate::registry::Capability;
        let s = synthesize(&mock_matrix_mouse(), &ctx(0x00A8, "n")).unwrap();
        for cap in [
            Capability::Dpi, Capability::SetDpi, Capability::DpiStages,
            Capability::SetDpiStages, Capability::Polling, Capability::SetPolling,
            Capability::Polling2, Capability::SetPolling2, Capability::Brightness,
            Capability::SetBrightness, Capability::Battery, Capability::Storage,
            Capability::Lighting,
        ] {
            assert!(s.def.supports(cap), "synthesized def must support {cap:?}");
        }
        // and the one capability synthesis can't discover stays absent
        assert!(!s.def.supports(Capability::SetScrollStage));
    }
}
