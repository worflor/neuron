// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! READ-ONLY R&D probe of the Razer Seiren V3 Mini's control pipe (pid 0x056a, usage 000c/0001).
//! The mic speaks the Razer command protocol in a 64-byte / report-id-0x07 envelope
//! (CRC = XOR(buf[2..=61]) @ buf[62]); confirmed live. This tool NEVER sends a setter — only
//! GETTERS (id with the 0x80 bit, read-only by Razer convention) — so it cannot change device state.
//!
//! Modes (`cargo run -p neuron --example seiren_probe -- <mode>`):
//!   dump   (default) — feature-report sweep + a few known class-0x00 getters
//!   sweep            — discover the tap-mute getter: scan getters, then you TAP and we diff
//!
//! Envelope constants for the Seiren variant:
const REPORT_ID: u8 = 0x07;
const BUF: usize = 64;
const TXID: u8 = 0x1F;

use neuron::transport::{self, ReadStep, Transport};
use std::time::{Duration, Instant};

fn hexdump(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X} ")).collect()
}

/// Read one feature report at `report_id` (buf[0]=id, as `HidD_GetFeature` requires). `None` = refused.
fn read_feature(path: &transport::DevicePath, report_id: u8) -> Option<Vec<u8>> {
    let t = transport::open_path(path).ok()?;
    let mut buf = vec![0u8; BUF];
    buf[0] = report_id;
    t.get_feature(&mut buf).ok().map(|_| buf)
}

/// Send a `razer_report` REQUEST in the 64-byte / id-0x07 envelope and read the reply on the SAME
/// handle. Read-only when `id` is a getter (0x80 bit). CRC = XOR(buf[2..=61]) @ buf[62].
fn razer_query(path: &transport::DevicePath, class: u8, id: u8, size: u8) -> Option<[u8; BUF]> {
    let t = transport::open_path(path).ok()?;
    let mut req = [0u8; BUF];
    req[0] = REPORT_ID;
    req[2] = TXID;
    req[6] = size;
    req[7] = class;
    req[8] = id;
    req[62] = req[2..=61].iter().fold(0u8, |c, &b| c ^ b);
    t.set_feature(&req).ok()?;
    std::thread::sleep(Duration::from_millis(25));
    let mut rep = [0u8; BUF];
    rep[0] = REPORT_ID;
    t.get_feature(&mut rep).ok()?;
    Some(rep)
}

/// A reply is a live answer (not the stale buffer) when it echoes our class+id with Success.
fn success_echo(rep: &[u8; BUF], class: u8, id: u8) -> bool {
    rep[1] == 0x02 && rep[7] == class && rep[8] == id
}

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "dump".into());
    let infos = transport::enumerate()?;
    let Some(info) = infos.iter().find(|i| i.vid == 0x1532 && i.pid == 0x056a) else {
        println!("no Razer Seiren (pid 056a) enumerated.");
        return Ok(());
    };
    println!(
        "target: pid {:04x}  usage {:04x}/{:04x}  feature_len {}  \"{}\"\n",
        info.pid, info.usage_page, info.usage, info.feature_len, info.product
    );

    if mode == "sweep" {
        return sweep(&info.path);
    }
    if mode == "input" {
        return input_watch(info);
    }
    if mode == "setter" {
        return setter_hunt(info);
    }
    if mode == "discover" {
        return discover(info);
    }
    if mode == "readmap" {
        return readmap(info);
    }
    if mode == "trymute" {
        return trymute(info);
    }
    if mode == "sweepmute" {
        return sweepmute(info);
    }
    if mode == "muteinfo" {
        return muteinfo(info);
    }
    if mode == "manual" {
        return manual(info);
    }

    // ── dump: feature sweep + known getters ───────────────────────────────────────────────────
    println!("[feature sweep] report ids 0..=8:");
    for id in 0u8..=8 {
        match read_feature(&info.path, id) {
            Some(buf) => println!("  id {id:#04x}: {}", hexdump(&buf)),
            None => println!("  id {id:#04x}: (no answer)"),
        }
    }
    println!("\n[known getters] class 0x00:");
    for (name, class, id, size) in [
        ("firmware", 0x00u8, 0x81u8, 0x02u8),
        ("serial", 0x00, 0x82, 0x16),
        ("device_mode", 0x00, 0x84, 0x02),
    ] {
        match razer_query(&info.path, class, id, size) {
            Some(rep) => println!(
                "  {name:<12} status={:#04x} echo={} args={}",
                rep[1],
                success_echo(&rep, class, id),
                hexdump(&rep[9..9 + size as usize])
            ),
            None => println!("  {name:<12} (no reply)"),
        }
    }
    Ok(())
}

/// Collect `05 11 <state>` mute-confirmation pushes for `ms`, returning the LAST state seen (None if
/// the firmware pushed nothing — i.e. the write we just made did NOT change the mute).
fn drain(rx: &std::sync::mpsc::Receiver<u8>, ms: u64) -> Option<u8> {
    let mut last = None;
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(state) => last = Some(state),
            Err(_) => break,
        }
    }
    last
}

/// Send a `razer_report` command (`set_feature` only, no reply read) in the 64B / id-0x07 envelope.
fn razer_set(path: &transport::DevicePath, class: u8, id: u8, arg: u8) -> bool {
    let Ok(t) = transport::open_path(path) else {
        return false;
    };
    let mut req = [0u8; BUF];
    req[0] = REPORT_ID;
    req[2] = TXID;
    req[6] = 0x02; // data_size
    req[7] = class;
    req[8] = id;
    req[9] = arg; // first arg byte = the state we're trying to set
    req[62] = req[2..=61].iter().fold(0u8, |c, &b| c ^ b);
    t.set_feature(&req).is_ok()
}

/// WRITE-SIDE R&D: find the command that sets the hardware mute (so the UI can drive it). Self-
/// verifying — an input-reader thread listens for the firmware's `05 11 <state>` push, so each write
/// is instantly confirmed by the device itself. Ordered lowest-risk first, stops on the first hit.
fn setter_hunt(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let path = info.path.clone();
    // Confirmation channel: the input reader forwards every `05 11 <state>` the firmware pushes.
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    let rpath = path.clone();
    std::thread::spawn(move || {
        let Ok(reader) = transport::open_reader(&rpath) else { return };
        let mut buf = vec![0u8; 64];
        loop {
            match transport::classify_read(reader.read(&mut buf)) {
                ReadStep::Data(n) if n >= 3 && buf[0] == 0x05 && buf[1] == 0x11 => {
                    if tx.send(buf[2]).is_err() {
                        return;
                    }
                }
                ReadStep::Data(_) => {}
                ReadStep::Idle => {}
                ReadStep::Gone => return,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(200)); // let the reader arm
    let _ = drain(&rx, 100); // clear any stale push

    // Detector = ANY `05 11 <state>` push arriving after a write (a mute STATE CHANGE), regardless of
    // direction — robust to whatever the current hardware state is. The mic is muted (red) now, so a
    // working setter driven toward LIVE (arg 0x00) flips it green and pushes `05 11 00`.

    // ── H1: mirror the notification back as an OUTPUT report (05 11 <state>). Lowest risk: the same
    // bytes the device emits, written the other way. Drive to LIVE then restore MUTE. 2 writes.
    println!("[H1] output-report mirror  (writing 05 11 00 [live] then 05 11 01 [mute]) — WATCH THE LED:");
    if let Ok(t) = transport::open_path(&path) {
        let mut live = [0u8; 64];
        live[0] = 0x05;
        live[1] = 0x11;
        live[2] = 0x00;
        let w1 = t.write_output(&live);
        let c1 = drain(&rx, 500);
        let mut mute = [0u8; 64];
        mute[0] = 0x05;
        mute[1] = 0x11;
        mute[2] = 0x01;
        let w2 = t.write_output(&mute);
        let c2 = drain(&rx, 500);
        println!("     write(live)={w1:?} confirm={c1:?}   write(mute)={w2:?} confirm={c2:?}");
        if c1.is_some() || c2.is_some() {
            println!("     >>> H1 WORKS: the device accepts its own 05 11 report as an OUTPUT write. <<<");
            return Ok(());
        }
        println!("     H1 no confirmation — escalating to the feature-command sweep.\n");
    } else {
        println!("     could not open a write handle — skipping to sweep.\n");
    }

    // ── H2: bounded feature-command setter sweep. For each (class,id) send arg=0x00 (drive to LIVE)
    // and watch for ANY `05 11` push within 150ms; on a hit, confirm with arg=0x01 (re-mute) and STOP.
    // GETTER ids (0x80 bit) are excluded (those READ); the device-mode setter (0x00/0x04) is skipped
    // so we never flip driver mode. Small state args only (0x00/0x01) — no exotic payloads.
    println!("[H2] feature-setter sweep (class 0x00..=0x1F, id 0x00..=0x0F, arg=0x00 live) — WATCH THE LED:");
    for class in 0x00u8..=0x1F {
        for id in 0x00u8..=0x0F {
            if class == 0x00 && id == 0x04 {
                continue; // device-mode setter — never touch it here
            }
            if !razer_set(&path, class, id, 0x00) {
                continue;
            }
            if drain(&rx, 150).is_some() {
                // confirm it really controls mute: re-mute it.
                let back = razer_set(&path, class, id, 0x01);
                let c = drain(&rx, 300);
                println!(
                    "\n     >>> HIT: class {class:#04x} id {id:#04x} controls the mute. \
                     re-mute write={back} confirm={c:?} <<<"
                );
                println!("     (setter = feature command class {class:#04x}/id {id:#04x}, arg 0=live 1=muted)");
                return Ok(());
            }
        }
    }
    println!("\n     no feature setter produced a 05 11 confirmation. The write path may need the");
    println!("     output/interrupt surface or a different envelope — report the LED behaviour you saw.");
    Ok(())
}

/// USER-in-the-loop setter test: fire the top setter candidates ONE at a time, announced, with a
/// ~2.5s pause each, so a human watching the LED can report which (if any) flickers/moves it — even
/// if the getter (which may read the physical sensor) snaps back and masks a transient write. Each
/// candidate targets a FLIP from the current getter state; the getter read-back is logged too.
fn manual(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();
    let cur = read_mute(t).unwrap_or(0);
    let flip = cur ^ 1;
    println!(
        "getter reads {cur} ({}). Each step below tries to set it to {flip}. WATCH THE LED — call out any change.\n",
        if cur == 1 { "muted" } else { "live" }
    );
    // (label, class, id, size, args) — the highest-probability shapes, mirror-write first.
    let steps: Vec<(&str, u8, u8, u8, Vec<u8>)> = vec![
        ("0x08/0x08 [01,state]", 0x08, 0x08, 0x02, vec![0x01, flip]),
        ("0x08/0x88 [01,state] (write the getter id)", 0x08, 0x88, 0x02, vec![0x01, flip]),
        ("0x08/0x08 [state]", 0x08, 0x08, 0x01, vec![flip]),
        ("0x08/0x08 [01,state,00] sz3", 0x08, 0x08, 0x03, vec![0x01, flip, 0x00]),
        ("0x08/0x08 [00,01,state] sz3", 0x08, 0x08, 0x03, vec![0x00, 0x01, flip]),
        ("0x08/0x00 [01,state]", 0x08, 0x00, 0x02, vec![0x01, flip]),
        ("0x08/0x0f [01,state]", 0x08, 0x0f, 0x02, vec![0x01, flip]),
    ];
    for (i, (label, class, id, size, args)) in steps.iter().enumerate() {
        println!("[{}/{}] {label}  — firing now, watch ~2.5s...", i + 1, steps.len());
        send_cmd(t, *class, *id, *size, args);
        std::thread::sleep(Duration::from_millis(2500));
        println!("      getter now reads {:?}\n", read_mute(t));
    }
    // leave it as we found it (best effort — the mirror write, if it worked, is undone by re-reading cur).
    println!("done — tell me which step (if any) moved the LED. If NONE did, the register is sensor-owned over HID.");
    Ok(())
}

/// Dump the mute register's STRUCTURE (read-only) to inform the setter shape: the full 0x08/0x88
/// reply (`data_size` + all body bytes), the same getter probed with store-selector args (does it have
/// volatile/persisted planes like DPI?), and the whole class-0x08 getter map (0x80..=0x8F) so we see
/// every register the mute class exposes.
fn muteinfo(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();

    let full = |class: u8, id: u8, size: u8, args: &[u8]| -> Option<[u8; BUF]> {
        let mut req = [0u8; BUF];
        req[0] = REPORT_ID;
        req[2] = TXID;
        req[6] = size;
        req[7] = class;
        req[8] = id;
        for (i, b) in args.iter().enumerate() {
            req[9 + i] = *b;
        }
        req[62] = req[2..=61].iter().fold(0u8, |c, &b| c ^ b);
        t.set_feature(&req).ok()?;
        std::thread::sleep(Duration::from_millis(15));
        let mut rep = [0u8; BUF];
        rep[0] = REPORT_ID;
        t.get_feature(&mut rep).ok()?;
        Some(rep)
    };

    println!("[muteinfo] the mute getter 0x08/0x88, various request sizes + store selectors:");
    for (label, size, args) in [
        ("size02 noargs", 0x02u8, vec![]),
        ("size02 [00]", 0x02, vec![0x00]),
        ("size02 [01]", 0x02, vec![0x01]),
        ("size08 noargs", 0x08, vec![]),
        ("size01 noargs", 0x01, vec![]),
    ] {
        if let Some(r) = full(0x08, 0x88, size, &args) {
            println!(
                "  {label:<14}: st{:02x} dsz{:02x}  args {}",
                r[1],
                r[6],
                hexdump(&r[9..25])
            );
        }
    }

    println!("\n[muteinfo] class 0x08 getter map (id 0x80..=0x8F):");
    for id in 0x80u8..=0x8F {
        if let Some(r) = full(0x08, id, 0x02, &[]) {
            if r[7] == 0x08 && r[8] == id {
                println!("  0x08/{id:02x}: st{:02x} dsz{:02x}  {}", r[1], r[6], hexdump(&r[9..21]));
            }
        }
    }
    Ok(())
}

/// Broad setter sweep using the RELIABLE getter (0x08/0x88) as the oracle: for every (class, id)
/// send `[0x01, target]` (the getter's own [selector, state] shape) and re-read the getter; the
/// command that moves it from `cur` is the setter. Precise (getter read-back, not the flaky push),
/// so no user, no guessing. Restores the original state on a hit. Skips device-mode (0x00/0x04).
fn sweepmute(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();
    let Some(cur) = read_mute(t) else {
        println!("getter 0x08/0x88 did not answer — aborting.");
        return Ok(());
    };
    let target = cur ^ 1;
    println!(
        "[sweepmute] getter reads {cur} ({}); sweeping class 0x00..=0x3F id 0x00..=0x7F, args [01,{target}], oracle = getter read-back.\n",
        if cur == 1 { "muted" } else { "live" }
    );
    for class in 0x00u8..=0x3F {
        for id in 0x00u8..=0x7F {
            if class == 0x00 && id == 0x04 {
                continue;
            }
            send_cmd(t, class, id, 0x02, &[0x01, target]);
            std::thread::sleep(Duration::from_millis(18));
            if read_mute(t) == Some(target) {
                println!("\n>>> SETTER FOUND: class {class:#04x} / id {id:#04x}, size 0x02, args [01, state] (0 live / 1 muted) <<<");
                send_cmd(t, class, id, 0x02, &[0x01, cur]); // restore
                std::thread::sleep(Duration::from_millis(30));
                println!(">>> restored to original ({cur}); getter now {:?}", read_mute(t));
                return Ok(());
            }
        }
        print!("{class:02x} ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    println!("\n[sweepmute] no [01,state] setter across class 0x00..=0x3F. Next: other layouts / the state may be write-locked to the physical sensor.");
    Ok(())
}

/// Read the mute state via the discovered getter (class 0x08, id 0x88): reply byte args[1] (buf[10])
/// is 0=live, 1=muted. `None` if the read didn't echo. Read-only.
fn read_mute(t: &dyn Transport) -> Option<u8> {
    send_cmd(t, 0x08, 0x88, 0x02, &[]);
    std::thread::sleep(Duration::from_millis(15));
    let mut rep = [0u8; BUF];
    rep[0] = REPORT_ID;
    t.get_feature(&mut rep).ok()?;
    (rep[7] == 0x08 && rep[8] == 0x88).then_some(rep[10])
}

/// Test the mute SETTER: read the current state via the getter, then send candidate set-commands to
/// FLIP it, verifying each by re-reading the getter AND watching the `05 11` push. Class 0x08 is the
/// mute register (proven by `readmap`); the primary candidate is id 0x08 (getter 0x88 with the high
/// bit cleared) with args `[0x01, state]` mirroring the getter's `[selector, state]` reply. Falls
/// through a few arg layouts. First one that moves the getter wins — then restores your prior state.
fn trymute(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    // 05 11 push oracle (secondary confirmation alongside the getter read-back).
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    let rpath = info.path.clone();
    std::thread::spawn(move || {
        let Ok(reader) = transport::open_reader(&rpath) else { return };
        let mut buf = vec![0u8; 64];
        loop {
            match transport::classify_read(reader.read(&mut buf)) {
                ReadStep::Data(n) if n >= 3 && buf[0] == 0x05 && buf[1] == 0x11 => {
                    let _ = tx.send(buf[2]);
                }
                ReadStep::Data(_) => {}
                ReadStep::Idle => {}
                ReadStep::Gone => return,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(150));
    let _ = drain(&rx, 80);

    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();

    let Some(cur) = read_mute(t) else {
        println!("mute getter 0x08/0x88 did not answer — aborting.");
        return Ok(());
    };
    let target = cur ^ 1;
    println!(
        "current mute (getter 0x08/0x88) = {cur} ({}). Trying to set it to {target} ({})...\n",
        if cur == 1 { "muted" } else { "live" },
        if target == 1 { "muted" } else { "live" }
    );

    // Class 0x08 is the mute register (0x88 reads it). Sweep every id in this class with the arg
    // layouts a state setter plausibly uses, using the GETTER as the oracle (read-back == target).
    // Bounded to class 0x08 so the blast radius is just this one register.
    let layouts: [(u8, fn(u8) -> Vec<u8>); 4] = [
        (0x02, |v| vec![0x01, v]), // mirror getter [selector 0x01, state]
        (0x01, |v| vec![v]),       // bare state
        (0x02, |v| vec![0x00, v]), // [00, state]
        (0x02, |v| vec![v, 0x00]), // state-first
    ];
    for id in 0x00u8..=0x8F {
        for (size, mk) in &layouts {
            let args = mk(target);
            send_cmd(t, 0x08, id, *size, &args);
            std::thread::sleep(Duration::from_millis(30));
            let push = drain(&rx, 40);
            let after = read_mute(t);
            if after == Some(target) || push == Some(target) {
                println!(
                    ">>> SETTER FOUND: class 0x08 / id {id:#04x}, size {size:#04x}, args {} (state 0 live / 1 muted); getter now {after:?}, push {push:?} <<<",
                    hexdump(&args)
                );
                // restore original state, being a good guest.
                send_cmd(t, 0x08, id, *size, &mk(cur));
                std::thread::sleep(Duration::from_millis(30));
                println!(">>> restored to your original state ({cur}); getter now {:?}", read_mute(t));
                return Ok(());
            }
        }
    }
    println!("\nno id in class 0x08 moved the getter with these 4 layouts. Getter 0x08/0x88 reads it fine, so the setter is exotic (bigger payload, or a different class writes what 0x08 reads).");
    Ok(())
}

/// READ-ONLY getter MAP: on a reused handle, first self-test the write path (`device_mode` getter must
/// echo), then sweep every getter (class 0x00..=0x3F, id 0x80..=0xBF) and print each LIVE reply (one
/// that echoes our class/id — i.e. not stale) with its first 12 arg bytes. Run this ONCE with the mic
/// MUTED and once LIVE, then `diff` the two dumps: the getter whose bytes differ is the mute register,
/// which names the class the setter lives in. Pure reads — cannot change device state.
fn readmap(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();

    // SELF-TEST: prove set_feature+get_feature round-trips on this handle before trusting a null sweep.
    send_cmd(t, 0x00, 0x84, 0x02, &[]);
    std::thread::sleep(Duration::from_millis(20));
    let mut probe = [0u8; BUF];
    probe[0] = REPORT_ID;
    let echo = t.get_feature(&mut probe).is_ok() && probe[7] == 0x00 && probe[8] == 0x84;
    eprintln!(
        "# write-path self-test (device_mode getter 0x00/0x84): {}",
        if echo { "OK (echoed)" } else { "FAILED — handle can't round-trip; results meaningless" }
    );
    if !echo {
        return Ok(());
    }

    let mut live = 0usize;
    for class in 0x00u8..=0x3F {
        for id in 0x80u8..=0xBF {
            send_cmd(t, class, id, 0x02, &[]);
            std::thread::sleep(Duration::from_millis(6));
            let mut rep = [0u8; BUF];
            rep[0] = REPORT_ID;
            if t.get_feature(&mut rep).is_err() {
                continue;
            }
            // LIVE reply = echoes our class+id (Success OR Unsupported — both prove the frame parsed
            // and this is THIS request's reply, not a stale buffer). Print args regardless of status.
            if rep[7] == class && rep[8] == id {
                live += 1;
                println!(
                    "{class:02x} {id:02x} st{:02x} {}",
                    rep[1],
                    hexdump(&rep[9..21])
                );
            }
        }
    }
    eprintln!("# {live} live getter(s) mapped. Re-run in the OTHER mute state and diff the two dumps.");
    Ok(())
}

/// Send one `razer_report` command on a REUSED handle (no per-command `CreateFile` — the discovery
/// sweep sends thousands, so the handle open dominates otherwise). Envelope: report id 0x07, txid
/// 0x1F, CRC XOR(buf[2..=61]) @ buf[62]. Args placed from buf[9].
fn send_cmd(t: &dyn Transport, class: u8, id: u8, size: u8, args: &[u8]) {
    let mut req = [0u8; BUF];
    req[0] = REPORT_ID;
    req[2] = TXID;
    req[6] = size;
    req[7] = class;
    req[8] = id;
    for (i, b) in args.iter().enumerate() {
        if 9 + i < 62 {
            req[9 + i] = *b;
        }
    }
    req[62] = req[2..=61].iter().fold(0u8, |c, &b| c ^ b);
    let _ = t.set_feature(&req);
}

/// WRITE-SIDE DISCOVERY — find the vendor command that sets the firmware mute (so the UI can drive
/// it, the thing Synapse used to do). Oracle = the firmware's own `05 11 <state>` push, which only
/// fires when the mute ACTUALLY changes (the parroted Success is worthless — this ignores it). For
/// every candidate (class, id, arg-layout) we send BOTH values 0 and 1: whichever differs from the
/// current state forces a FLIP, so detection is robust to the current mute state AND to the value
/// encoding — no priming, no assumptions. First command that causes a push wins; we verify by
/// flipping it back, then STOP so the sweep never strands the device mid-flip.
///
/// This DOES write unknown commands to the mic. Bounded (class 0x00..=0x0F, id 0x00..=0x1F, 3 arg
/// layouts), device-mode setter (0x00/0x04) skipped so we never touch driver mode, small state args
/// only. A wrong command could nudge some other setting (gain/EQ) — recoverable by replug — but the
/// stop-on-first-hit keeps the blast radius tiny in practice.
fn discover(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    // Oracle: a reader thread forwards every `05 11 <state>` push.
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    let rpath = info.path.clone();
    std::thread::spawn(move || {
        let Ok(reader) = transport::open_reader(&rpath) else { return };
        let mut buf = vec![0u8; 64];
        loop {
            match transport::classify_read(reader.read(&mut buf)) {
                ReadStep::Data(n) if n >= 3 && buf[0] == 0x05 && buf[1] == 0x11 => {
                    if tx.send(buf[2]).is_err() {
                        return;
                    }
                }
                ReadStep::Data(_) => {}
                ReadStep::Idle => {}
                ReadStep::Gone => return,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    let _ = drain(&rx, 100); // clear stale

    let t = transport::open_path(&info.path)?;
    let t = t.as_ref();

    // arg layouts a binary-state setter plausibly uses (name, size, |val| -> args):
    let layouts: [(&str, u8, fn(u8) -> Vec<u8>); 3] = [
        ("[v]", 0x01, |v| vec![v]),
        ("[v,00]", 0x02, |v| vec![v, 0x00]),
        ("[00,v]", 0x02, |v| vec![0x00, v]), // varstore/selector + state
    ];

    println!(
        "[discover] sweeping class 0x00..=0x0F, id 0x00..=0x1F, 3 layouts x 2 values — WATCH THE LED.\n\
         (a HIT flips it; the `05 11` push is the oracle. ~2-3 min. Ctrl-C is safe.)\n"
    );
    for class in 0x00u8..=0x0F {
        for id in 0x00u8..=0x1F {
            if class == 0x00 && id == 0x04 {
                continue; // device-mode setter — never here
            }
            for (lname, size, mk) in &layouts {
                for val in [0u8, 1u8] {
                    send_cmd(t, class, id, *size, &mk(val));
                    if let Some(state) = drain(&rx, 60) {
                        println!(
                            "\n>>> HIT  class {class:#04x} id {id:#04x} size {size:#04x} layout {lname} val {val:#04x}  -> firmware pushed 05 11 {state:02x}"
                        );
                        // verify: flip back with the other value, same layout.
                        let other = val ^ 1;
                        send_cmd(t, class, id, *size, &mk(other));
                        let back = drain(&rx, 300);
                        println!(
                            ">>> VERIFY flip-back val {other:#04x} -> {}",
                            back.map_or("NO push (one-way? partial?)".into(), |s| format!("05 11 {s:02x}"))
                        );
                        println!(
                            ">>> SETTER = class {class:#04x} / id {id:#04x}, size {size:#04x}, args {lname} (v=state)"
                        );
                        return Ok(());
                    }
                }
            }
        }
        print!("{class:02x} ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    println!("\n[discover] no setter in class 0x00..=0x0F / id 0x00..=0x1F. Widen the range and rerun.");
    Ok(())
}

/// Passively read the vendor collection's INPUT reports (direct `ReadFile`, bypassing Raw Input's
/// cooked path) while the user taps the physical mute. Fully read-only. If the self-contained
/// firmware pushes a report on state change, the tap-mute byte shows up here directly.
fn input_watch(info: &transport::HidDeviceInfo) -> anyhow::Result<()> {
    let len = info.input_len.max(64) as usize;
    println!("[input watch] opening the vendor collection for reads (input_len {len})...");
    let reader = match transport::open_reader(&info.path) {
        Ok(r) => r,
        Err(e) => {
            println!("  reader unavailable (collection OS-protected / busy): {e}");
            return Ok(());
        }
    };
    println!("  reading INPUT reports for 20s — TAP THE MUTE BUTTON on/off several times now:\n");
    // The read blocks until a report arrives; run it on a worker so a silent 20s still ends cleanly.
    let path = info.path.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let Ok(reader) = transport::open_reader(&path) else { return };
        let mut buf = vec![0u8; len];
        loop {
            match transport::classify_read(reader.read(&mut buf)) {
                ReadStep::Data(n) if n > 0 => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                ReadStep::Data(_) => {}
                ReadStep::Idle => {}
                ReadStep::Gone => return,
            }
        }
    });
    drop(reader); // the worker owns its own handle; this one was just the reachability probe
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut count = 0usize;
    while Instant::now() < deadline {
        if let Ok(report) = rx.recv_timeout(Duration::from_millis(250)) {
            count += 1;
            println!("  input report: {}", hexdump(&report));
        }
    }
    println!("\ndone — {count} input report(s) captured. A byte that tracks mute/unmute is the state.");
    Ok(())
}

/// DISCOVERY: scan the getter space for live (Success+echo) responders, then poll them while the
/// user taps the physical mute — the getter whose args flip 1:1 with the tap is the mute state.
/// GETTERS ONLY (ids 0x80..=0x8F), so every probe is a read.
fn sweep(path: &transport::DevicePath) -> anyhow::Result<()> {
    println!("[phase A] getter support scan — classes 0x00..=0x1F, ids 0x80..=0x8F (read-only):");
    let mut live: Vec<(u8, u8, [u8; BUF])> = Vec::new();
    for class in 0x00u8..=0x1F {
        for id in 0x80u8..=0x8F {
            if let Some(rep) = razer_query(path, class, id, 0x02) {
                if success_echo(&rep, class, id) {
                    println!("  class {class:#04x} id {id:#04x}: args {}", hexdump(&rep[9..17]));
                    live.push((class, id, rep));
                }
            }
        }
    }
    if live.is_empty() {
        println!("\nNo getter answered Success — the tap-mute may ride an INPUT report, not a getter.");
        return Ok(());
    }
    println!(
        "\n[phase B] {} live getter(s). Polling them for 25s — TAP THE MUTE BUTTON several times.",
        live.len()
    );
    println!("          (lines = a getter arg byte that changed: class/id[idx] old->new)\n");
    let mut last: std::collections::HashMap<(u8, u8), [u8; BUF]> =
        live.iter().map(|(c, i, r)| ((*c, *i), *r)).collect();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut hits = 0usize;
    while Instant::now() < deadline {
        for (class, id, _) in &live {
            if let Some(now) = razer_query(path, *class, *id, 0x02) {
                if let Some(prev) = last.get(&(*class, *id)) {
                    for (idx, (a, b)) in prev.iter().zip(now.iter()).enumerate().take(20) {
                        if a != b {
                            println!("  class {class:#04x} id {id:#04x}[{idx}]: {a:#04x} -> {b:#04x}");
                            hits += 1;
                        }
                    }
                }
                last.insert((*class, *id), now);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("\ndone — {hits} change(s). The getter whose byte flips with each tap is the tap-mute state.");
    Ok(())
}
