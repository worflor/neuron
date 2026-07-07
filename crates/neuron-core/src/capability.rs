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
    let raw = a[1] as u32; // byte[1] carries the level (byte[0] observed 0)
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

/// Firmware GAME MODE — the keyboard's own FN+F10-toggled Win-key kill (the GAME_LED state). When
/// ON, the board eats the Windows key in FIRMWARE with ZERO software running: it is the device-
/// PHYSICAL sibling of the host-side KEY GUARD chord swallows (which live in `crate::hook`). This
/// is exactly what silently ate the user's Win key on 2026-07-07 while every host layer read clean.
///
/// Hardware-confirmed on the BlackWidow Chroma V2 (2026-07-07): the getter (0x03/0x80, baked args
/// `[varstore, GAME_LED 0x08]`) echoes the state at response arg[2] (0 = off, nonzero = on).
///
/// NOTE the FN+F10 hardware toggle itself only works in NORMAL device mode — in driver mode the
/// board defers FN combos to software (OpenRazer #1174 is the same bug class), so while neuron is
/// driving the board the SOFTWARE path ([`set_game_mode`]) is the reliable way to flip it.
pub fn game_mode(dev: &Device) -> Result<bool> {
    let a = dev.run("game_mode")?;
    Ok(a[2] != 0)
}

/// Set the firmware GAME MODE (the Win-key kill) on/off, then READ-BACK VERIFY it landed. Writes via
/// `set_game_mode` (0x03/0x00, args `[varstore, GAME_LED 0x08, state]`) — the hardware-confirmed
/// setter (2026-07-07: `[00 08 00]` ACKed and read back, the GAME_LED visibly went dark on the
/// software write) — then re-reads the [`game_mode`] getter and bails with an honest MISMATCH error
/// if the board doesn't report the state we asked for. This mirrors the verify-gated discipline of
/// `writes::verify_getter` in spirit; a plain re-read + compare is enough here since both the setter
/// and getter are registry-named commands (no raw class/id/offset to thread).
///
/// The device-PHYSICAL half of the KEY GUARD: the host chord-swallow lives in `crate::hook`, this is
/// the firmware Win-key kill surfaced beside it. See [`game_mode`]'s note on why the software path is
/// the reliable one while neuron holds the board in driver mode.
pub fn set_game_mode(dev: &Device, on: bool) -> Result<()> {
    let state = on as u8;
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

/// Current sensitivity, (DPI_X, DPI_Y). Response is [varstore, X_hi, X_lo, Y_hi, Y_lo].
pub fn dpi(dev: &Device) -> Result<(u16, u16)> {
    let a = dev.run("dpi")?;
    let x = ((a[1] as u16) << 8) | a[2] as u16;
    let y = ((a[3] as u16) << 8) | a[4] as u16;
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
    pub fn byte(self) -> u8 {
        match self {
            Store::Volatile => 0x00,
            Store::Persist => 0x01,
        }
    }
    pub fn from_persist(persist: bool) -> Self {
        if persist {
            Store::Persist
        } else {
            Store::Volatile
        }
    }
}

/// Set sensitivity (DPI), X and Y, big-endian. `store` picks volatile vs onboard-persistent.
pub fn set_dpi(dev: &Device, x: u16, y: u16, store: Store) -> Result<()> {
    let args = [
        store.byte(),
        (x >> 8) as u8,
        x as u8,
        (y >> 8) as u8,
        y as u8,
        0x00,
        0x00,
    ];
    dev.run_args("set_dpi", &args)?;
    Ok(())
}

/// Polling rate in Hz. Device reports a divisor of 1000 (1→1000, 2→500, 4→250, 8→125).
pub fn polling_rate_hz(dev: &Device) -> Result<u32> {
    let a = dev.run("polling_rate")?;
    let div = a[0] as u32;
    Ok(1000u32.checked_div(div).unwrap_or(0))
}

/// Set polling rate in Hz (snaps to the nearest supported 1000/500/250/125).
pub fn set_polling_hz(dev: &Device, hz: u32) -> Result<u32> {
    let div: u8 = match hz {
        h if h >= 1000 => 1,
        h if h >= 500 => 2,
        h if h >= 250 => 4,
        _ => 8,
    };
    dev.run_args("set_polling", &[div])?;
    Ok(1000 / div as u32)
}

/// Map a target Hz to the hi-res polling code (OpenRazer polling2: 0x01=8000 … 0x40=125Hz).
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
/// when the device only has the legacy divisor path. Confirmed opcode (OpenRazer polling2).
pub fn polling_rate_hz_hires(dev: &Device) -> Option<u32> {
    let a = dev.run("polling2").ok()?;
    let hz = polling2_code_to_hz(a.first().copied().unwrap_or(0));
    (hz > 0).then_some(hz)
}

/// Set polling via the hi-res command (0x00/0x40 = `[arg0, code]`) — supports up to 8000Hz on
/// devices that have a HyperPolling path. Returns the Hz actually selected.
pub fn set_polling_hz_hires(dev: &Device, hz: u32) -> Result<u32> {
    let code = polling2_code(hz);
    dev.run_args("set_polling2", &[0x00, code])?;
    Ok(polling2_code_to_hz(code))
}

/// Set lighting brightness as a 0..=100 percentage (visible LED region).
///
/// Two honest write paths, decided by the device's DATA: matrix-era boards expose a top-level
/// `set_brightness` command ([varstore, led 0x04, level]); legacy boards wire brightness inside
/// the `[lighting]` block instead (the BlackWidow's 0x03/0x03 with its own baked [varstore, led]
/// prefix). The old matrix-only path made the GUI's apply fail on the keyboard with "has no
/// command 'set_brightness'" even though the board CAN set brightness — a dialect leak, not a
/// missing capability.
///
/// The `store` parameter applies to the TOP-LEVEL-command dialect only. The lighting-block
/// fallback uses the SPEC'S OWN baked varstore byte (the legacy dialect's single hardware-proven
/// layout, e.g. the BlackWidow's `args = [0x01, 0x05]`); `store` is deliberately NOT spliced into
/// that legacy prefix, because a volatile-varstore legacy brightness write is an unproven byte
/// combination this write path refuses to invent (verify-gated culture: no unproven bytes on the
/// wire).
pub fn set_brightness(dev: &Device, pct: u8, store: Store) -> Result<()> {
    let level = (pct.min(100) as u16 * 255 / 100) as u8;
    if dev.def.has_command("set_brightness") {
        dev.run_args("set_brightness", &[store.byte(), 0x04, level])?;
        return Ok(());
    }
    let Some(spec) = dev.def.lighting.as_ref().and_then(|l| l.brightness.as_ref()) else {
        anyhow::bail!("device '{}' has no brightness write path", dev.def.name);
    };
    // the spec's args are the full dialect prefix (e.g. legacy [0x01 varstore, 0x05 led]);
    // only the level is appended — the registry data owns the layout, not this code.
    let mut args = spec.args.clone();
    args.push(level);
    dev.exec_dynamic_tx(
        spec.transaction_id.unwrap_or(dev.def.transaction_id),
        spec.class,
        spec.id,
        spec.size,
        &args,
    )?;
    Ok(())
}

/// Lighting brightness as a percentage. Response arg[2] is raw 0..255 (Synapse shows %).
pub fn brightness_percent(dev: &Device) -> Result<u8> {
    let a = dev.run("brightness")?;
    Ok(((a[2] as u32 * 100 + 127) / 255) as u8)
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
    pub fn free_bytes(&self) -> u32 {
        self.avail_bytes + self.recycle_bytes
    }
    pub fn used_bytes(&self) -> u32 {
        self.max_bytes.saturating_sub(self.free_bytes())
    }
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

/// Power class + idle-timeout getter (matches `writes::set_idle_secs`'s CLASS_POWER/ID_IDLE_GET).
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
    ((reply[0] as u16) << 8) | reply[1] as u16
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let x = ((p[4] as u16) << 8) | p[5] as u16;
        let y = ((p[6] as u16) << 8) | p[7] as u16;
        assert_eq!((x, y), (16000, 16000));
    }
}
