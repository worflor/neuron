// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Typed capability decoders. These hold the *semantics* (how to interpret a reply);
//! the *command codes* come from the device registry, so they generalize across devices
//! that expose the same capability under different class/id.

use crate::device::Device;
use anyhow::Result;

/// Firmware version, e.g. "1.02".
pub fn firmware(dev: &Device) -> Result<String> {
    let a = dev.run("firmware_version")?;
    Ok(format!("{}.{:02}", a[0], a[1]))
}

/// Battery charge as a percentage. Device reports 0..=255; we scale to 0..=100.
pub fn battery_percent(dev: &Device) -> Result<u8> {
    let a = dev.run("battery_level")?;
    let raw = u32::from(a[1]); // byte[1] carries the level (byte[0] observed 0)
    Ok(((raw * 100 + 127) / 255) as u8)
}

/// Whether the device is currently charging.
pub fn charging(dev: &Device) -> Result<bool> {
    let a = dev.run("charging_status")?;
    Ok(a[0] != 0)
}

/// Raw device mode byte (0x03 = driver mode on the Naga).
pub fn device_mode(dev: &Device) -> Result<u8> {
    let a = dev.run("device_mode")?;
    Ok(a[0])
}

/// Firmware GAME MODE — the keyboard's own FN+F10-toggled Win-key kill (the `GAME_LED` state). When
/// ON, the board eats the Windows key in FIRMWARE with ZERO software running: it is the device-
/// PHYSICAL sibling of the host-side KEY GUARD chord swallows (which live in `crate::hook`). This
/// is exactly what silently ate the user's Win key on 2026-07-07 while every host layer read clean.
///
/// Hardware-confirmed on the `BlackWidow` Chroma V2 (2026-07-07): the getter (0x03/0x80, baked args
/// `[varstore, GAME_LED 0x08]`) echoes the state at response arg[2] (0 = off, nonzero = on).
///
/// NOTE the FN+F10 hardware toggle itself only works in NORMAL device mode — in driver mode the
/// board defers FN combos to software (`OpenRazer` #1174 is the same bug class), so while neuron is
/// driving the board the SOFTWARE path ([`set_game_mode`]) is the reliable way to flip it.
pub fn game_mode(dev: &Device) -> Result<bool> {
    let a = dev.run("game_mode")?;
    Ok(a[2] != 0)
}

/// Set the firmware GAME MODE (the Win-key kill) on/off, then READ-BACK VERIFY it landed. Writes via
/// `set_game_mode` (0x03/0x00, args `[varstore, GAME_LED 0x08, state]`) — the hardware-confirmed
/// setter (2026-07-07: `[00 08 00]` `ACKed` and read back, the `GAME_LED` visibly went dark on the
/// software write) — then re-reads the [`game_mode`] getter and bails with an honest MISMATCH error
/// if the board doesn't report the state we asked for. This mirrors the verify-gated discipline of
/// `writes::verify_getter` in spirit; a plain re-read + compare is enough here since both the setter
/// and getter are registry-named commands (no raw class/id/offset to thread).
///
/// The device-PHYSICAL half of the KEY GUARD: the host chord-swallow lives in `crate::hook`, this is
/// the firmware Win-key kill surfaced beside it. See [`game_mode`]'s note on why the software path is
/// the reliable one while neuron holds the board in driver mode.
pub fn set_game_mode(dev: &Device, on: bool) -> Result<()> {
    let state = u8::from(on);
    dev.run_args("set_game_mode", &[0x00, 0x08, state])?;
    // Read-back verify: a lighting/LED write can ACK yet not land, so we never trust the write —
    // the getter must echo the state we asked for, or this is a failure (not a silent success).
    let got = game_mode(dev)?;
    if got != on {
        anyhow::bail!(
            "VERIFY FAILED on game_mode: wrote {on} but device reports {got} — write NOT trusted"
        );
    }
    Ok(())
}

/// Current sensitivity, (`DPI_X`, `DPI_Y`). Response is [varstore, `X_hi`, `X_lo`, `Y_hi`, `Y_lo`].
pub fn dpi(dev: &Device) -> Result<(u16, u16)> {
    let a = dev.run("dpi")?;
    let x = (u16::from(a[1]) << 8) | u16::from(a[2]);
    let y = (u16::from(a[3]) << 8) | u16::from(a[4]);
    Ok((x, y))
}

/// Where a write lands. Volatile = NOSTORE (live, reverts on power-cycle); Persist = VARSTORE
/// (flashed to the device's onboard memory — survives with zero software running). The
/// difference matters: a sniper's temporary DPI must be Volatile; a saved profile is Persist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    Volatile,
    Persist,
}

impl Store {
    /// The varstore arg byte.
    #[must_use]
    pub fn byte(self) -> u8 {
        match self {
            Store::Volatile => 0x00,
            Store::Persist => 0x01,
        }
    }
    #[must_use]
    pub fn from_persist(persist: bool) -> Self {
        if persist {
            Store::Persist
        } else {
            Store::Volatile
        }
    }
}

/// Set sensitivity (DPI), X and Y, big-endian. `store` picks volatile vs onboard-persistent.
///
/// `cause` is not decoration. It is recorded in [`crate::dpi_origin`] before the bytes go out, which
/// is what later lets a device-pushed DPI announce be recognised as this write coming back rather
/// than as the device drifting on its own; and when [`crate::dpi_origin::Cause::is_durable`] holds,
/// it also writes [`crate::feel_intent`], the record every wake reassert heals from. Both live here
/// rather than at the call sites so that adding a new way to change DPI cannot skip either one —
/// the reconcile's correctness depends on every writer declaring itself, which is the type system's
/// job now rather than a convention six call sites had to remember. The matching `dpi` getter is
/// read from the same varstore plane after the SET; an ACK without matching bytes fails and rolls
/// back the provisional provenance and durable intent.
pub fn set_dpi(dev: &Device, x: u16, y: u16, store: Store, cause: crate::dpi_origin::Cause) -> Result<()> {
    // Both provenance channels are claimed BEFORE the write, and rolled back if it is refused.
    //
    // The ordering is the point. The device can announce the new value before this call returns, so
    // an announce that overtook its own record would be judged against stale evidence and "healed"
    // straight back — neuron fighting a write it had just made. The on-disk half is what makes this
    // hold across processes: a `neuron dpi 800` in a terminal is observed by the resident tray only
    // as a device announce, and the record is the sole thing that tells the tray a human asked for
    // it. Recorded after the write instead, the tray could read the old value and undo the command.
    let prev_stamp = crate::dpi_origin::expect(dev.pid, &dev.dpi_unit, x, cause);
    let prev_intent = cause.is_durable().then(|| {
        let snap = crate::feel_intent::snapshot(dev.pid);
        // A failed record is swallowed, never propagated as a failed write: the device half still
        // stands, and the cost of a lost record is a reassert that does nothing, not a wrong
        // sensitivity.
        let _ = crate::feel_intent::record_dpi(dev.pid, x, y);
        snap
    });
    let args = [
        store.byte(),
        (x >> 8) as u8,
        x as u8,
        (y >> 8) as u8,
        y as u8,
        0x00,
        0x00,
    ];
    let landed = (|| -> Result<()> {
        dev.run_args("set_dpi", &args)?;
        // `dpi` is store-aware; select the same plane this SET addressed. This is the same
        // varstore argument used by the existing DPI reconcile read-back path.
        let reply = dev.run_args("dpi", &[store.byte()])?;
        if reply[0] != store.byte() {
            anyhow::bail!(
                "VERIFY FAILED on DPI: requested {:?} plane but device reports varstore {:#04x} — write NOT trusted",
                store,
                reply[0]
            );
        }
        let got = (
            (u16::from(reply[1]) << 8) | u16::from(reply[2]),
            (u16::from(reply[3]) << 8) | u16::from(reply[4]),
        );
        if got != (x, y) {
            anyhow::bail!(
                "VERIFY FAILED on DPI: wrote {x} x {y} but device reports {} x {} — write NOT trusted",
                got.0,
                got.1
            );
        }
        Ok(())
    })();
    if let Err(e) = landed {
        crate::dpi_origin::rollback(dev.pid, &dev.dpi_unit, prev_stamp);
        if let Some(snap) = prev_intent {
            let _ = crate::feel_intent::restore(dev.pid, snap);
        }
        return Err(e);
    }
    Ok(())
}

/// Polling rate in Hz. Device reports a divisor of 1000 (1→1000, 2→500, 4→250, 8→125).
pub fn polling_rate_hz(dev: &Device) -> Result<u32> {
    let a = dev.run("polling_rate")?;
    let div = u32::from(a[0]);
    Ok(1000u32.checked_div(div).unwrap_or(0))
}

/// Set polling rate in Hz (snaps to the nearest supported 1000/500/250/125) and verify the matching
/// divisor getter before returning the selected rate.
pub fn set_polling_hz(dev: &Device, hz: u32) -> Result<u32> {
    let div: u8 = match hz {
        h if h >= 1000 => 1,
        h if h >= 500 => 2,
        h if h >= 250 => 4,
        _ => 8,
    };
    dev.run_args("set_polling", &[div])?;
    let expected = 1000 / u32::from(div);
    let got = polling_rate_hz(dev)?;
    if got != expected {
        anyhow::bail!(
            "VERIFY FAILED on polling rate: wrote {expected} Hz but device reports {got} Hz — write NOT trusted"
        );
    }
    Ok(expected)
}

/// Map a target Hz to the hi-res polling code (`OpenRazer` polling2: 0x01=8000 … 0x40=125Hz).
fn polling2_code(hz: u32) -> u8 {
    match hz {
        h if h >= 8000 => 0x01,
        h if h >= 4000 => 0x02,
        h if h >= 2000 => 0x04,
        h if h >= 1000 => 0x08,
        h if h >= 500 => 0x10,
        h if h >= 250 => 0x20,
        _ => 0x40,
    }
}

/// Inverse of [`polling2_code`].
#[must_use]
pub fn polling2_code_to_hz(code: u8) -> u32 {
    match code {
        0x01 => 8000,
        0x02 => 4000,
        0x04 => 2000,
        0x08 => 1000,
        0x10 => 500,
        0x20 => 250,
        0x40 => 125,
        _ => 0,
    }
}

/// Hi-res polling rate via the newer command (0x00/0xC0), if the device exposes it. Returns `None`
/// when the device only has the legacy divisor path. Confirmed opcode (`OpenRazer` polling2).
#[must_use]
pub fn polling_rate_hz_hires(dev: &Device) -> Option<u32> {
    let a = dev.run("polling2").ok()?;
    let hz = polling2_code_to_hz(a.first().copied().unwrap_or(0));
    (hz > 0).then_some(hz)
}

/// Set polling via the hi-res command (0x00/0x40 = `[arg0, code]`) — supports up to 8000Hz on
/// devices that have a `HyperPolling` path. Returns the Hz actually selected.
pub fn set_polling_hz_hires(dev: &Device, hz: u32) -> Result<u32> {
    let code = polling2_code(hz);
    // Keep the direct capability surface on the same explicitly gated, verified path used by the
    // in-game polling writer; this opcode has not been hardware-confirmed on the shipped dongle.
    crate::writes::set_in_game_polling(dev, hz, hz)?;
    let expected = polling2_code_to_hz(code);
    let got = polling_rate_hz_hires(dev).ok_or_else(|| {
        anyhow::anyhow!("VERIFY FAILED on hi-res polling rate: matching getter is unavailable")
    })?;
    if got != expected {
        anyhow::bail!(
            "VERIFY FAILED on hi-res polling rate: wrote {expected} Hz but device reports {got} Hz — write NOT trusted"
        );
    }
    Ok(expected)
}

/// Set lighting brightness as a 0..=100 percentage (visible LED region).
///
/// Two honest write paths, decided by the device's DATA: matrix-era boards expose a top-level
/// `set_brightness` command ([varstore, led 0x04, level]); legacy boards wire brightness inside
/// the `[lighting]` block instead (the `BlackWidow`'s 0x03/0x03 with its own baked [varstore, led]
/// prefix). The old matrix-only path made the GUI's apply fail on the keyboard with "has no
/// command '`set_brightness`'" even though the board CAN set brightness — a dialect leak, not a
/// missing capability.
///
/// The `store` parameter applies to the TOP-LEVEL-command dialect only. The lighting-block
/// fallback uses the SPEC'S OWN baked varstore byte (the legacy dialect's single hardware-proven
/// layout, e.g. the `BlackWidow`'s `args = [0x01, 0x05]`); `store` is deliberately NOT spliced into
/// that legacy prefix, because a volatile-varstore legacy brightness write is an unproven byte
/// combination this write path refuses to invent (verify-gated culture: no unproven bytes on the
/// wire). The top-level path verifies the matching brightness getter on the same plane. Legacy
/// devices without a getter stay gated behind `NEURON_BRIGHTNESS_WRITE=1`.
pub fn set_brightness(dev: &Device, pct: u8, store: Store) -> Result<()> {
    let level = (u16::from(pct.min(100)) * 255 / 100) as u8;
    // Legacy brightness specs have no paired getter. Keep that write explicitly opt-in until the
    // device can verify its own bytes; an ACK alone is not evidence that the LED changed.
    if !dev.def.has_command("brightness") && !brightness_write_enabled() {
        anyhow::bail!(
            "brightness write on '{}' has no read-back getter and is gated; set NEURON_BRIGHTNESS_WRITE=1 to opt in to the unverified legacy path",
            dev.def.name
        );
    }
    if dev.def.has_command("set_brightness") {
        dev.run_args("set_brightness", &[store.byte(), 0x04, level])?;
    } else {
        let Some(spec) = dev.def.lighting.as_ref().and_then(|l| l.brightness.as_ref()) else {
            anyhow::bail!("device '{}' has no brightness write path", dev.def.name);
        };
        // The spec's args are the full dialect prefix; only the level is appended.
        let mut args = spec.args.clone();
        args.push(level);
        dev.exec_dynamic_tx(
            spec.transaction_id.unwrap_or(dev.def.transaction_id),
            spec.class,
            spec.id,
            spec.size,
            &args,
        )?;
    }
    if dev.def.has_command("brightness") {
        let got = dev.run_args("brightness", &[store.byte(), 0x04])?;
        if got[0] != store.byte() || got[1] != 0x04 || got[2] != level {
            anyhow::bail!(
                "VERIFY FAILED on brightness: wrote varstore {:#04x}, region 0x04, level {level} but device reports varstore {:#04x}, region {:#04x}, level {:#04x} — write NOT trusted",
                store.byte(),
                got[0],
                got[1],
                got[2]
            );
        }
    }
    Ok(())
}

fn brightness_write_enabled() -> bool {
    std::env::var("NEURON_BRIGHTNESS_WRITE").is_ok_and(|v| v == "1")
}

/// Lighting brightness as a percentage. Response arg[2] is raw 0..255 (Synapse shows %).
pub fn brightness_percent(dev: &Device) -> Result<u8> {
    let a = dev.run("brightness")?;
    Ok(((u32::from(a[2]) * 100 + 127) / 255) as u8)
}

/// The single onboard pool. Everything — macros, profiles, our files — draws from the
/// same `max_bytes`. `avail` is immediately allocatable; `recycle` is reclaimable GC
/// slack; together they are the "free" Synapse reports as "% remaining".
pub struct Storage {
    pub max_bytes: u32,
    pub avail_bytes: u32,
    pub recycle_bytes: u32,
    pub max_macros: u8,
}

impl Storage {
    /// Free = immediately-available + reclaimable (matches Synapse's "free").
    #[must_use]
    pub fn free_bytes(&self) -> u32 {
        self.avail_bytes + self.recycle_bytes
    }
    #[must_use]
    pub fn used_bytes(&self) -> u32 {
        self.max_bytes.saturating_sub(self.free_bytes())
    }
    #[must_use]
    pub fn pct_remaining(&self) -> u32 {
        (self.free_bytes() * 100)
            .checked_div(self.max_bytes)
            .unwrap_or(0)
    }
}

/// Read the unified pool accounting (06/8E):
/// `[vs, MaxMacros, MaxStorage(4 BE), Avail(4 BE), Recycle(4 BE)]`.
pub fn storage(dev: &Device) -> Result<Storage> {
    let a = dev.run("storage_info")?;
    Ok(Storage {
        max_macros: a[1],
        max_bytes: u32::from_be_bytes([a[2], a[3], a[4], a[5]]),
        avail_bytes: u32::from_be_bytes([a[6], a[7], a[8], a[9]]),
        recycle_bytes: u32::from_be_bytes([a[10], a[11], a[12], a[13]]),
    })
}

/// Current item counts on the device, best-effort: (macros, profiles) from 06/80.
pub fn storage_counts(dev: &Device) -> Result<(u8, u8)> {
    let a = dev.run("storage_counts")?;
    Ok((a[0], a[1]))
}

// --- "feel" controls: idle/sleep timeout (read side) ----------------------------------------
// The GUI's feel controls need to DISPLAY the current sleep/idle timeout next to the
// `writes::set_idle_secs` setter. The READ (power class 0x07 / id 0x83) is proven live (memory:
// sleep timeout read = 300s). This getter is additive (uses `exec_dynamic`, no registry command
// needed) and pairs with the verify-gated `writes::set_idle_secs`. The reply carries a big-endian
// u16 of seconds; `0` = "never sleep / stay awake".

/// Power class + idle-timeout getter (matches `writes::set_idle_secs`'s `CLASS_POWER/ID_IDLE_GET`).
const CLASS_POWER: u8 = 0x07;
const ID_IDLE_GET: u8 = 0x83;
const IDLE_SIZE: u8 = 0x02;

/// Read the device sleep/idle timeout in seconds (big-endian u16; `0` = never sleep). Proven read
/// path (0x07/0x83). The matching write is `writes::set_idle_secs` (verify-gated). Wireless mice
/// idle-sleep, so on an asleep Naga this getter times out until the mouse is moved — that surfaces
/// as the `Err` from `exec_dynamic`, not a fabricated value.
pub fn idle_timeout_secs(dev: &Device) -> Result<u16> {
    let a = dev.exec_dynamic(CLASS_POWER, ID_IDLE_GET, IDLE_SIZE, &[])?;
    Ok(decode_idle_secs(&a))
}

/// Decode the big-endian u16 seconds from an idle-timeout reply payload (bytes 0..2). Pure, so the
/// decode is unit-testable without a device, and it is the exact inverse of `writes::build_idle_payload`.
fn decode_idle_secs(reply: &[u8]) -> u16 {
    (u16::from(reply[0]) << 8) | u16::from(reply[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naga_device(pid: u16, phantom: crate::transport::mock::MockDevice) -> Device {
        let def = toml::from_str(include_str!("../devices/razer-naga-v2-pro.toml"))
            .expect("the curated Naga definition parses");
        let phantom = std::sync::Arc::new(phantom);
        Device::with_transport(def, pid, Box::new(phantom.handle()))
    }

    #[test]
    fn dpi_polling_and_brightness_setters_verify_matching_getters() {
        use crate::transport::mock::MockDevice;

        let pid = 0xFDC1;
        let phantom = MockDevice::razer(pid, "phantom naga")
            .answering(0x04, 0x05, &[])
            .answering(0x04, 0x85, &[0, 0x03, 0x20, 0x03, 0x20])
            .answering(0x00, 0x05, &[])
            .answering(0x00, 0x85, &[2])
            .answering(0x0F, 0x04, &[])
            .answering(0x0F, 0x84, &[1, 0x04, 127]);
        let d = naga_device(pid, phantom);

        set_dpi(&d, 800, 800, Store::Volatile, crate::dpi_origin::Cause::Momentary)
            .expect("DPI must succeed only when the matching getter confirms it");
        assert_eq!(set_polling_hz(&d, 500).unwrap(), 500);
        set_brightness(&d, 50, Store::Persist)
            .expect("brightness must succeed only when the matching getter confirms it");
    }

    #[test]
    fn acknowledged_but_mismatched_writes_fail_verification() {
        use crate::transport::mock::MockDevice;

        let pid = 0xFDC2;
        let phantom = MockDevice::razer(pid, "phantom naga")
            .answering(0x04, 0x05, &[])
            .answering(0x04, 0x85, &[0, 0x06, 0x40, 0x06, 0x40])
            .answering(0x00, 0x05, &[])
            .answering(0x00, 0x85, &[4])
            .answering(0x0F, 0x04, &[])
            .answering(0x0F, 0x84, &[1, 0x04, 0]);
        let d = naga_device(pid, phantom);
        let cause = crate::dpi_origin::Cause::Momentary;
        let _ = crate::dpi_origin::expect(pid, &d.dpi_unit, 1600, cause);

        let err = set_dpi(&d, 800, 800, Store::Volatile, cause)
            .expect_err("an ACK without the requested DPI read-back must fail");
        assert!(err.to_string().contains("VERIFY FAILED on DPI"));
        assert_ne!(
            crate::dpi_origin::classify(pid, &d.dpi_unit, 800),
            crate::dpi_origin::Origin::Echo(cause),
            "a mismatched write must not remain claimed as an echo"
        );
        assert!(set_polling_hz(&d, 500)
            .expect_err("a polling ACK with a mismatched getter must fail")
            .to_string()
            .contains("VERIFY FAILED on polling rate"));
        assert!(set_brightness(&d, 50, Store::Persist)
            .expect_err("a brightness ACK with a mismatched getter must fail")
            .to_string()
            .contains("VERIFY FAILED on brightness"));
        crate::dpi_origin::forget(pid, &d.dpi_unit);
    }

    #[test]
    fn dpi_verifies_the_varstore_plane_that_was_written() {
        use crate::transport::mock::MockDevice;

        let pid = 0xFDC4;
        let phantom = MockDevice::razer(pid, "phantom naga")
            .answering(0x04, 0x05, &[])
            .answering(0x04, 0x85, &[1, 0x03, 0x20, 0x03, 0x20]);
        let d = naga_device(pid, phantom);
        set_dpi(&d, 800, 800, Store::Persist, crate::dpi_origin::Cause::Momentary)
            .expect("the persisted write must be read back from the persisted plane");
        crate::dpi_origin::forget(pid, &d.dpi_unit);
    }

    #[test]
    fn legacy_brightness_without_a_getter_is_gated_before_the_write() {
        use crate::transport::mock::MockDevice;

        let pid = 0xFDC3;
        let def: crate::registry::DeviceDef = toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml"))
            .expect("the curated BlackWidow definition parses");
        let phantom = std::sync::Arc::new(
            MockDevice::razer(pid, "phantom BlackWidow").answering(0x03, 0x03, &[]),
        );
        let d = Device::with_transport(def, pid, Box::new(phantom.handle()));
        assert!(!d.def.has_command("brightness"));

        if brightness_write_enabled() {
            set_brightness(&d, 50, Store::Persist)
                .expect("the explicit opt-in permits the legacy ACK-only setter");
            assert_eq!(phantom.asked(), vec![(0x03, 0x03)]);
        } else {
            let err = set_brightness(&d, 50, Store::Persist)
                .expect_err("an ACK-only legacy setter must remain gated without a getter");
            assert!(err.to_string().contains("has no read-back getter and is gated"));
            assert!(phantom.asked().is_empty(), "the gate must run before any wire write");
        }
    }

    #[test]
    fn idle_secs_decode_inverts_the_write_payload() {
        // The getter's decode is the exact inverse of `writes::build_idle_payload` (big-endian u16).
        assert_eq!(decode_idle_secs(&[0x01, 0x2C]), 300); // 0x012C
        assert_eq!(decode_idle_secs(&[0x00, 0x78]), 120); // 0x0078
        assert_eq!(decode_idle_secs(&[0x00, 0x00]), 0, "0 = never sleep");
        assert_eq!(decode_idle_secs(&[0xFF, 0xFF]), u16::MAX);
        // Round-trip against the writer's payload builder.
        let p = crate::writes::build_idle_payload(300);
        assert_eq!(decode_idle_secs(&p), 300);
    }

    #[test]
    fn polling2_code_maps_each_supported_rate() {
        // The exact OpenRazer polling2 code table (hz -> code byte).
        assert_eq!(polling2_code(8000), 0x01);
        assert_eq!(polling2_code(4000), 0x02);
        assert_eq!(polling2_code(2000), 0x04);
        assert_eq!(polling2_code(1000), 0x08);
        assert_eq!(polling2_code(500), 0x10);
        assert_eq!(polling2_code(250), 0x20);
        assert_eq!(polling2_code(125), 0x40);
    }

    #[test]
    fn polling2_code_snaps_between_steps_down_to_nearest_supported() {
        // Values between rungs snap DOWN to the nearest supported rate (>= threshold).
        assert_eq!(
            polling2_code(16000),
            0x01,
            "above max clamps to 8000Hz code"
        );
        assert_eq!(polling2_code(6000), 0x02, "6000 -> 4000Hz rung");
        assert_eq!(polling2_code(1500), 0x08, "1500 -> 1000Hz rung");
        assert_eq!(polling2_code(60), 0x40, "below min -> 125Hz code");
    }

    #[test]
    fn polling2_code_round_trips_through_hz() {
        // build code from hz, decode hz from code -> the canonical supported rate.
        for (hz, code, canon) in [
            (8000u32, 0x01u8, 8000u32),
            (4000, 0x02, 4000),
            (2000, 0x04, 2000),
            (1000, 0x08, 1000),
            (500, 0x10, 500),
            (250, 0x20, 250),
            (125, 0x40, 125),
        ] {
            assert_eq!(polling2_code(hz), code, "hz {hz} -> code {code:#04x}");
            assert_eq!(
                polling2_code_to_hz(code),
                canon,
                "code {code:#04x} -> hz {canon}"
            );
            // Full round-trip: hz -> code -> hz lands on the canonical rate.
            assert_eq!(polling2_code_to_hz(polling2_code(hz)), canon);
        }
    }

    #[test]
    fn polling2_unknown_code_decodes_to_zero() {
        // A code the device shouldn't send (not a power-of-two rung) decodes to 0 = "unknown",
        // which `polling_rate_hz_hires` treats as "no hi-res path" (returns None).
        assert_eq!(polling2_code_to_hz(0x00), 0);
        assert_eq!(polling2_code_to_hz(0x03), 0);
        assert_eq!(polling2_code_to_hz(0xFF), 0);
    }

    #[test]
    fn legacy_polling_divisor_snaps_like_the_writes_helper() {
        // The legacy single-rate divisor (set_polling_hz) and writes::polling_divisor must agree:
        // both snap 1000/500/250/125 to divisor 1/2/4/8.
        for hz in [125u32, 250, 500, 1000, 8000, 60, 999] {
            // Re-derive the divisor the same way set_polling_hz does and check it matches the
            // shared helper, so the two polling code paths can't drift apart.
            let expect = match hz {
                h if h >= 1000 => 1u8,
                h if h >= 500 => 2,
                h if h >= 250 => 4,
                _ => 8,
            };
            assert_eq!(crate::writes::polling_divisor(hz), expect, "hz {hz}");
        }
    }

    #[test]
    fn dpi_decode_inverts_the_dpi_stage_record() {
        // `capability::dpi` decodes [vs, Xhi, Xlo, Yhi, Ylo, ..]; the stage builder writes the same
        // big-endian X/Y in its records. Decode a single-stage payload's record with the dpi reader
        // shape and confirm it round-trips (the two layouts share the BE u16 X/Y convention).
        let stages = [crate::writes::DpiStage { x: 16000, y: 16000 }];
        let p = crate::writes::build_dpi_stages_payload(&stages, 0, Store::Volatile).unwrap();
        // Record body at offset 3: [id, Xhi, Xlo, Yhi, Ylo, 0, 0].
        let x = (u16::from(p[4]) << 8) | u16::from(p[5]);
        let y = (u16::from(p[6]) << 8) | u16::from(p[7]);
        assert_eq!((x, y), (16000, 16000));
    }
}
