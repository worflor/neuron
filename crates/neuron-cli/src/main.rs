// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::float_cmp, clippy::drop_non_drop, clippy::field_reassign_with_default))]

//! Neuron CLI — the lightweight, open replacement for Razer Synapse.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fmt::Write as _;
#[cfg(any(windows, target_os = "linux"))]
use neuron::{
    device::DeviceSession,
    executor::{DispatchExecutor, DispatchOutcome, IntentRunner, TurboRuntime},
};
#[cfg(any(windows, target_os = "linux"))]
use neuron::audio;
use neuron::{
    backup,
    bindings::Bindings,
    capability as cap,
    cast::CastConfig,
    device::Device,
    discover,
    gesture::Vault,
    glyph,
    lighting::{self, Effect, Rgb},
    profile::Profile,
    radial::{self, RadialMenu},
    registry::{DeviceDef, Registry},
    transport,
    writes::{self, decode_dpi_active, decode_dpi_stages, DpiStage},
};

#[derive(Parser)]
#[command(
    name = "neuron",
    version,
    about = "Open, lightweight control for Razer devices: the anti-Synapse"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected, recognized Razer devices
    List,
    /// Self-emergent capability discovery: probe ANY Razer `razer_report` device, no registry
    Discover {
        /// adopt unknown devices: synthesize a FULL device def per unknown device and write it
        /// to devices/auto/ in the run folder (same as `neuron adopt`)
        #[arg(long)]
        emit: bool,
    },
    /// Adopt unknown Razer devices: probe the getter space, synthesize a complete device def
    /// (commands + lighting dialect + measured link pacing) and write devices/auto/<pid>.toml
    /// in the run folder (next to the neuron binaries).
    /// From then on the file is plain per-device config — editable, never overwritten.
    Adopt {
        /// print the synthesized TOML instead of writing files, and include ALREADY-KNOWN
        /// devices — diff against a curated def to verify synthesis on proven hardware
        #[arg(long)]
        dry_run: bool,
    },
    /// Show device info (firmware, mode, battery)
    Info,
    /// Show battery level and charging state
    Battery,
    /// Mouse DPI: no value shows current; a value sets it (verified by read-back)
    Dpi { value: Option<u16> },
    /// Polling rate (Hz): show, or set — snaps to 1000/500/250/125
    Polling { hz: Option<u32> },
    /// Mouse DPI STAGE LIST (the cycle): no values shows the current configured stages; a list of
    /// values sets the whole table via the verify-gated `set_dpi_stages` write (e.g.
    /// `neuron dpi-stages 800 1600 3200`). `--active` picks which stage is live (1-based); `--persist`
    /// flashes the table to onboard memory.
    DpiStages {
        /// the DPI values that make up the cycle, in order (omit to just read the current table)
        stages: Vec<u16>,
        /// which stage is active, 1-based (default: the first)
        #[arg(long, default_value_t = 1)]
        active: u8,
        /// flash the stage table to onboard memory (survives with no software running)
        #[arg(long)]
        persist: bool,
    },
    /// `HyperScroll` active wheel stage (tactile / free-spin / ...): no value shows the current stage;
    /// a value selects it via the wire-confirmed `set_scroll_stage` write (class 0x15/0x00).
    Scroll {
        /// the 1-based stage to make active (omit to just read the current stage)
        stage: Option<u8>,
        /// flash the choice to onboard memory (default: persist, matching what Synapse sends)
        #[arg(long)]
        volatile: bool,
    },
    /// Sensor LIFT-OFF DISTANCE (the height the mouse stops tracking). No args reads it;
    /// `--lift N --landing M` sets an ASYMMETRIC split (separate lift/landing, verify-gated);
    /// `--sym L` sets a symmetric level (0=low/1=med/2=high) and flips back out of async.
    Lod {
        /// asymmetric LIFT level 2..=26 (requires --landing)
        #[arg(long)]
        lift: Option<u8>,
        /// asymmetric LANDING level 1..=25 (requires --lift)
        #[arg(long)]
        landing: Option<u8>,
        /// set a SYMMETRIC level instead (0=low / 1=med / 2=high)
        #[arg(long)]
        sym: Option<u8>,
    },
    /// Lighting brightness 0..=100: show, or set
    Brightness { pct: Option<u8> },
    /// Keyboard FIRMWARE game mode — the FN+F10 Win-key kill. No arg reads it; `on`/`off` sets it
    /// (write is read-back verified). This is the DEVICE-side Win-key kill (firmware, zero software),
    /// distinct from the host-side KEY GUARD chord swallows. Resolves the keyboard by capability.
    GameMode {
        /// on | off (omit to just read the current state)
        state: Option<String>,
    },
    /// Sniper / on-the-fly DPI: hold a control to drop to a precision DPI, release to snap back.
    /// Authors the bind into the shared rule store; the resident neuron app enforces the hold.
    /// First run asks you to PRESS the control you want — nothing hardcoded.
    Sniper {
        /// re-bind the hold control (press the one you want)
        #[arg(long)]
        bind: bool,
        /// precision DPI while held (default 400, or whatever you last set)
        #[arg(long)]
        dpi: Option<u16>,
    },
    /// Onboard memory: unified pool view (macros + files + profiles share one 124 KB)
    Storage {
        /// also dump the raw 06/8E + 06/8D device bytes (full transparency)
        #[arg(long)]
        raw: bool,
    },
    /// Watch headset knob / mute / consumer control events (remap foundation)
    Watch {
        /// how long to listen, seconds
        #[arg(long, default_value_t = 20)]
        seconds: u64,
    },
    /// Gesture engine (glyph eigenmotion): record / match / tune / list
    Gesture {
        #[command(subcommand)]
        action: GestureCmd,
    },
    /// Audio endpoints (Core Audio) — cross-device remap targets: real mic gain & mute.
    /// This is the exact OS path Synapse's `WinAudio` uses; no vendor RE needed.
    Audio {
        #[command(subcommand)]
        action: AudioCmd,
    },
    /// Manage remap bindings (control event -> action), stored in bindings.toml
    Bind {
        #[command(subcommand)]
        action: BindCmd,
    },
    /// Run the remap daemon: listen for control events and fire bound actions (ESC to stop)
    Run {
        /// stop after N seconds (default: run until ESC)
        #[arg(long)]
        seconds: Option<u64>,
        /// safe mode — observe + dry-run only: keep real input synthesis DISARMED so no
        /// keystrokes/clicks are actually injected (the engine still resolves & reports).
        #[arg(long)]
        safe: bool,
    },
    /// Unified lighting across both Chroma eras. No args = show capabilities (live).
    /// `lighting effect spectrum` = dry-run the exact bytes per device (writes gated).
    Lighting {
        #[command(subcommand)]
        action: Option<LightingCmd>,
    },
    /// Radial (pie / comms-wheel) menu — hold the trigger, flick a direction, release.
    Radial {
        #[command(subcommand)]
        action: RadialCmd,
    },
    /// Cast engine — one trigger fires gestures (drawn glyphs) AND radial flicks into actions.
    Cast {
        #[command(subcommand)]
        action: CastCmd,
    },
    /// Profiles — save/apply named bundles of settings (dpi + polling + brightness + lighting).
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
    },
    /// Flip the device-control switch Razer gates host access behind: driver (host/Neuron
    /// controls it, like Synapse does) | hardware (onboard profile runs autonomously).
    Mode {
        /// driver | hardware
        mode: String,
        /// device PID (hex), e.g. 00a8
        #[arg(long)]
        pid: String,
    },
    /// DEVICE-SIDE thumb-button remap (Razer 15/02, RE'd live): the physical button emits the new
    /// key AT THE SOURCE — one keystroke, no host injection, no double-send. `--key <thumb key>`
    /// names the stock keypad key to remap (e.g. `=`); `--button <hex>` targets a raw button id;
    /// `--to <key>` is the target (e.g. `g`, `f13`, `space`). `--reset` restores the stock grid.
    Remap {
        /// stock keypad key on the thumb grid to remap (e.g. "=", "5") — resolves to its button id
        #[arg(long)]
        key: Option<String>,
        /// raw thumb button id in hex (e.g. 4b) — alternative to --key
        #[arg(long)]
        button: Option<String>,
        /// target key the button should emit (e.g. "g", "f13", "space")
        #[arg(long)]
        to: Option<String>,
        /// restore the whole thumb grid to stock (1 2 3 4 5 6 7 8 9 0 - =)
        #[arg(long)]
        reset: bool,
    },
    /// Read-only: snapshot every recognized device's full state to backups/*.json. The first
    /// move of every safe write — a known-good restore/verify reference.
    Backup {
        /// only this PID (hex), e.g. 00a8
        #[arg(long)]
        pid: Option<String>,
    },
    /// Read-only: re-snapshot a device and diff it against a saved backup (the round-trip
    /// verify step — confirm only intended bytes moved, or detect drift).
    Verify {
        /// path to a backups/*.json file
        file: String,
    },
    /// Eat existing Synapse config (any version) into Neuron. Version-agnostic: locates the
    /// Razer vendor root and classifies config by content, never by a hard-coded v3 schema.
    Import {
        /// also extract + print sample values (not just file classification)
        #[arg(long)]
        deep: bool,
    },
    /// Import a Synapse EXPORT file (`*.synapse3` / `*.ChromaEffects`) into Neuron config.
    /// Plaintext, no crypto: unzips the export, parses its XML, and (with --apply) writes the
    /// resulting profile + spine rules to disk.
    ImportExport {
        /// path to the exported file (a ZIP with a fake extension)
        file: String,
        /// write the imported Profile + rules to disk (else just preview what would import)
        #[arg(long)]
        apply: bool,
    },
    /// Python macros — the power tier of the spine. Real `CPython` run by the warm Macro Host sidecar:
    /// full unsandboxed power (ctypes/subprocess/anything), registered once, fired by name.
    Macro {
        #[command(subcommand)]
        action: MacroCmd,
    },
    /// KNOCKBACK — the rhythm familiar. Prove the twin headless: play a scripted session,
    /// print the brain's stats, and export the visual language (sigil + storyboard SVGs).
    /// `neuron twin demo`  ·  `neuron twin sigil out.svg`  ·  `neuron twin stats`
    Twin {
        #[command(subcommand)]
        action: TwinCmd,
    },
    /// Portable clipboards. `neuron pocket a` MOVES the clipboard into/out of pocket "a" (stash if
    /// the clipboard has content, restore if the pocket does, swap if both) — carrying every
    /// format, not just text. `--list` shows what each pocket holds; `--sigil out.svg` exports a
    /// pocket's emergent content-sigil; `--keep` makes it survive a restart.
    Pocket {
        /// the pocket name (omit = the single default pocket)
        name: Option<String>,
        /// show every pocket's current contents (read-only)
        #[arg(long)]
        list: bool,
        /// keep this pocket on disk so it survives a restart
        #[arg(long)]
        keep: bool,
        /// export this pocket's content-sigil as an SVG to this path (read-only)
        #[arg(long)]
        sigil: Option<String>,
    },
    /// Read-only: interrogate a device's `razer_report` getter space with raw bytes.
    /// `neuron probe 0221 --scan`  or  `neuron probe 0221 06 8e`
    Probe {
        /// device product id in hex, e.g. 0221 (keyboard) or 00a8 (mouse)
        pid: String,
        /// capability class in hex (e.g. 06); requires <id>
        class: Option<String>,
        /// command id in hex — must be a getter (>= 0x80)
        id: Option<String>,
        /// sweep every class x id 0x80..0x8F and dump raw responders
        #[arg(long)]
        scan: bool,
    },
    /// Measurement harness for the input pump — read-only diagnostics for the baseline the
    /// upcoming pump rewrite is measured against (wake counts by reason, wake -> first-edge
    /// latency histogram, tick-starvation watchdog). Counters are IN-PROCESS: `neuron run`
    /// prints its own session's snapshot on exit; a bare `neuron prof pump` reads whatever this
    /// process itself has pumped (zero, unless something in-process ran the loop first).
    Prof {
        #[command(subcommand)]
        action: ProfCmd,
    },
}

#[derive(Subcommand)]
enum ProfCmd {
    /// Print the current pump-loop counters.
    Pump,
}

#[derive(Subcommand)]
enum TwinCmd {
    /// Play a deterministic scripted session and narrate the loop (KNOCK→KNOCKBACK→…).
    Demo {
        /// number of exchanges to play
        #[arg(long, default_value_t = 16)]
        turns: usize,
        /// also write a storyboard SVG of the final knockback here
        #[arg(long)]
        svg: Option<String>,
    },
    /// Load the saved familiar (or a fresh one) and print its brain stats.
    Stats {
        /// path to a saved familiar (.knbk); defaults to the runtime location
        #[arg(long)]
        file: Option<String>,
    },
    /// Export the personal sigil — the glyph no other human could generate — as an SVG.
    Sigil {
        /// output path
        out: String,
        /// load the familiar from here first (otherwise plays a short demo to grow one)
        #[arg(long)]
        file: Option<String>,
        /// sigil canvas size in px
        #[arg(long, default_value_t = 480)]
        size: u32,
    },
    /// Export a preview of the live session STAGE (the in-game overlay's exact layout) as an
    /// SVG — the twin's reply standing played, the blueprint waiting, the weave below.
    Stage {
        /// output path
        out: String,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List saved profiles
    List,
    /// Show a profile's settings
    Show { name: String },
    /// Save a profile from explicit values (only the flags you pass are stored)
    Save {
        name: String,
        #[arg(long)]
        dpi: Option<u16>,
        #[arg(long)]
        polling: Option<u32>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        disable_alt_tab: bool,
        #[arg(long)]
        disable_win: bool,
        #[arg(long)]
        disable_alt_f4: bool,
        #[arg(long)]
        disable_alt_esc: bool,
        #[arg(long)]
        idle_secs: Option<u32>,
        #[arg(long)]
        in_game_wired: Option<u32>,
        #[arg(long)]
        in_game_dongle: Option<u32>,
        /// flash to onboard memory on apply (survives with no software running)
        #[arg(long)]
        persist: bool,
    },
    /// Apply a profile to the connected devices
    Apply { name: String },
    /// Delete a profile and the binds sidecar paired with it
    Delete {
        name: String,
        /// skip the "this profile exists and here's what it holds" confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Rename a profile, carrying its binds sidecar and any auto-switch rules with it
    Rename { from: String, to: String },
    /// Capture the CURRENT live device settings into a profile — the clean Synapse import
    /// (reads what Synapse wrote to your hardware; no encrypted-file gimmicks).
    Capture { name: String },
    /// App-aware auto-switch: no args shows rules + the focused app; with args adds a rule
    Autoswitch {
        /// app substring (e.g. "valorant")
        app: Option<String>,
        /// profile to apply for that app
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
enum MacroCmd {
    /// List the python macros registered under `macros/scripts/`.
    List,
    /// Add a macro from a Python FILE: stores it under `macros/scripts/<name>.py` and registers it
    /// into the warm `MacroHost` (so a syntax error surfaces now). The body defines `def macro(ctx):`
    /// and may `import neuron` (ctx + key/clipboard/mouse/run helpers) or reach past it into any
    /// raw API (`import ctypes`, `subprocess`, …).
    Add {
        /// a name for the macro (stored as macros/scripts/<name>.py)
        name: String,
        /// path to a .py file containing the macro
        file: String,
    },
    /// Run a macro by name (registered) or from a --file, ONCE, against the live context, and print
    /// its result/traceback. (Requires the python runtime; arms the helper input layer per --arm.)
    Run {
        /// run a registered macro by name
        name: Option<String>,
        /// or register+run a .py file directly
        #[arg(long)]
        file: Option<String>,
        /// arm real input synthesis for this run (default: SAFE — helper input is traced, not fired)
        #[arg(long)]
        arm: bool,
    },
    /// Syntax-check a Python macro file (the honest "dry-run": parse + list defs, no execution —
    /// python is full-power, so we never claim a behavioural trace).
    Check {
        /// path to a .py file
        file: String,
    },
    /// Print the `neuron` host-module reference — what a macro gets (ctx + the helper surface).
    Prelude,
}

#[derive(Subcommand)]
enum CastCmd {
    /// Show the cast config (trigger, mode, wheel bindings, glyph bindings)
    Show,
    /// Write a starter cast.toml you can edit
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Run the cast engine: hold trigger, flick or draw, release -> fires the action (ESC stops)
    Run {
        /// override the config's trigger VK
        #[arg(long)]
        trigger: Option<i32>,
    },
}

#[derive(Subcommand)]
enum RadialCmd {
    /// Print the sector map for N sectors (no device needed)
    Map {
        #[arg(long, default_value_t = 8)]
        sectors: usize,
    },
    /// Live: hold the trigger, flick a direction, release -> shows the chosen sector
    Pick {
        #[arg(long, default_value_t = DEFAULT_TRIGGER)]
        trigger: i32,
        #[arg(long, default_value_t = 8)]
        sectors: usize,
    },
}

#[derive(Subcommand)]
enum LightingCmd {
    /// Animate an emulated effect by streaming computed frames (the open-effects engine, live).
    Run {
        /// off | static | breathing | spectrum | wave | reactive | starlight
        name: String,
        #[arg(long)]
        color: Option<String>,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value_t = 30)]
        fps: u64,
        /// device PID (hex); default = first custom-frame-capable device
        #[arg(long)]
        pid: Option<String>,
    },
    /// Preview (dry-run) the exact bytes to set an effect on every lit device. Writes gated.
    Effect {
        /// off | static | breathing | spectrum | wave | reactive | starlight
        name: String,
        /// base colour as RRGGBB (for static/breathing/reactive)
        #[arg(long)]
        color: Option<String>,
        /// also preview a brightness set, 0..=100
        #[arg(long)]
        brightness: Option<u8>,
        /// ACTUALLY send it (gated write). Only NOSTORE/volatile effects are allowed here.
        #[arg(long)]
        apply: bool,
        /// restrict to one device PID (hex), e.g. 00a8 — recommended for the first validation
        #[arg(long)]
        pid: Option<String>,
        /// override the target LED region id (hex), e.g. 04 — for probing which region is visible
        #[arg(long)]
        led: Option<String>,
        /// override the raw effect-id byte (hex) — for empirically mapping the firmware's effects
        #[arg(long = "effect-id")]
        effect_id: Option<String>,
        /// send EXACT arg bytes to the effect command (hex, e.g. "00 04 03 01 28") — full control
        #[arg(long)]
        raw: Option<String>,
        /// override the command id (hex) — e.g. 0B to hit the custom-frame command, not effect
        #[arg(long = "cmd-id")]
        cmd_id: Option<String>,
        /// render the effect host-side as a streamed custom frame (the emulation path)
        #[arg(long)]
        emulate: bool,
        /// use VARSTORE (persist to onboard) instead of NOSTORE
        #[arg(long)]
        store: bool,
    },
    /// CROSS-DEVICE DATA SURFACE — paint the MOUSE's live vitals (battery / charge / active DPI
    /// stage) onto the KEYBOARD's LED matrix. One process speaking BOTH devices: the mouse is the
    /// data source, the keyboard the sink. Loops ~1s, repainting (on-demand, ACK'd) only when the
    /// state changes. Runs until ESC unless `--seconds N` is given.
    Mirror {
        /// stop after N seconds (default: run until ESC)
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// HARDWARE RE-VERIFY the key map: walk the keyboard key by key (reading order), lighting ONLY
    /// each key's mapped cell in a bright accent so you can confirm the LIT key matches its printed
    /// name — and flag any that don't, for a map correction. Uses the ACK'd on-demand custom-frame
    /// paint (not streaming). Prints `lighting: <KEY> (row N, col M)` per key; ESC stops early.
    Keytest {
        /// keyboard PID (hex); default = the `BlackWidow` Chroma V2 (0221)
        #[arg(long, default_value = "0221")]
        pid: String,
        /// how long to hold each key lit, in milliseconds
        #[arg(long, default_value_t = 700)]
        dwell: u64,
        /// accent colour as RRGGBB (default: a bright cyan)
        #[arg(long)]
        color: Option<String>,
    },
    /// REVERSE-ENGINEER wide-key LED footprints: walk EVERY cell of the matrix — (row, col) for row
    /// in 0..rows, col in 0..cols, INCLUDING the unmapped "gap" cells the keymap/keytest skip — and
    /// light ONLY that one cell in a bright accent via the ACK'd on-demand custom-frame paint (not
    /// streaming). Note which cells a wide key (space, shift, enter, backspace) physically spans, so
    /// you can map real footprints from hardware truth. Prints `cell (row N, col M)` per cell; ESC
    /// stops early; clears the board at the end.
    Cellsweep {
        /// keyboard PID (hex); default = the `BlackWidow` Chroma V2 (0221)
        #[arg(long, default_value = "0221")]
        pid: String,
        /// how long to hold each cell lit, in milliseconds
        #[arg(long, default_value_t = 500)]
        dwell: u64,
        /// sweep ONLY this row (0-based) instead of the whole matrix — e.g. scan row 5 for the space bar
        #[arg(long)]
        row: Option<u8>,
        /// accent colour as RRGGBB (default: a bright cyan)
        #[arg(long)]
        color: Option<String>,
    },
    /// BLOCK-VERIFY a wide key's span: light a CONTIGUOUS BLOCK of cells (row N, colA..=colB) ALL AT
    /// ONCE and HOLD, so you can confirm on hardware which cells a wide key physically covers. Paints
    /// every cell in the block simultaneously via the ACK'd on-demand custom-frame paint (not
    /// streaming), prints `lit: row N, cols A..=B (K cells)`, holds (ESC-interruptible, or `--seconds`),
    /// then clears. E.g. `neuron lighting cells --row 5 --from 4 --to 10` lights exactly the space bar.
    Cells {
        /// the row to light (0-based)
        #[arg(long)]
        row: u8,
        /// first column of the block, inclusive (0-based)
        #[arg(long)]
        from: u8,
        /// last column of the block, inclusive (0-based)
        #[arg(long)]
        to: u8,
        /// keyboard PID (hex); default = the `BlackWidow` Chroma V2 (0221)
        #[arg(long, default_value = "0221")]
        pid: String,
        /// block colour as RRGGBB (default: a bright cyan)
        #[arg(long)]
        color: Option<String>,
        /// hold for N seconds (default: hold until ESC)
        #[arg(long)]
        seconds: Option<u64>,
    },
}

#[derive(Subcommand)]
enum BindCmd {
    /// Show the active bindings (from bindings.toml, or built-in defaults if none)
    List,
    /// Write the default bindings.toml so you can edit it
    Init {
        /// overwrite an existing bindings.toml
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum AudioCmd {
    /// List every active capture + render endpoint with its volume and mute state
    List,
    /// Watch Razer audio endpoints for volume/mute changes (knob/mute/tap surface here, not Raw Input)
    Monitor {
        #[arg(long, default_value_t = 20)]
        seconds: u64,
    },
    /// Show or control the mic (a capture endpoint). No flags = just show state.
    Mic {
        /// match the capture device by name substring (default: Razer/Seiren, else system default)
        #[arg(long)]
        device: Option<String>,
        /// set gain as a percentage, 0..=100
        #[arg(long)]
        gain: Option<f32>,
        /// nudge gain by +/- percentage points (e.g. --nudge -- -5)
        #[arg(long, allow_hyphen_values = true)]
        nudge: Option<f32>,
        /// mute control: on | off | toggle
        #[arg(long)]
        mute: Option<String>,
    },
    /// Show or control an OUTPUT endpoint (headphones / sound card / speakers). No flags = show.
    Out {
        /// match the render device by name substring (default: system's active output)
        #[arg(long)]
        device: Option<String>,
        /// set volume as a percentage, 0..=100
        #[arg(long)]
        vol: Option<f32>,
        /// nudge volume by +/- percentage points
        #[arg(long, allow_hyphen_values = true)]
        nudge: Option<f32>,
        /// mute control: on | off | toggle
        #[arg(long)]
        mute: Option<String>,
    },
}

/// Default `HyperShift` trigger: mouse thumb button 1 (`VK_XBUTTON1` = 0x05). Hold it,
/// draw, release. Override with --trigger <vk> (e.g. 0x02 right, 0x06 thumb2, 0x12 alt).
const DEFAULT_TRIGGER: i32 = 0x05;

#[derive(Subcommand)]
enum GestureCmd {
    /// Validate the eigenmotion engine against synthetic shapes (no device needed)
    Selftest,
    /// Record a named gesture: hold the trigger, draw, release
    Record {
        name: String,
        #[arg(long, default_value_t = DEFAULT_TRIGGER)]
        trigger: i32,
    },
    /// Recognize a gesture against the vault: hold the trigger, draw, release
    Match {
        #[arg(long, default_value_t = DEFAULT_TRIGGER)]
        trigger: i32,
    },
    /// List stored gestures and the current attunement
    List,
    /// Attune the three glyph properties + match threshold (persisted in the vault)
    Tune {
        /// weight on damping |λ| (open arc vs sustained loop)
        #[arg(long)]
        damping: Option<f64>,
        /// weight on signed curvature (handedness + tightness)
        #[arg(long)]
        curve: Option<f64>,
        /// weight on irregularity (residual)
        #[arg(long)]
        resid: Option<f64>,
        /// DTW match threshold (lower = stricter)
        #[arg(long)]
        threshold: Option<f64>,
        /// arc-length resample count
        #[arg(long)]
        resample: Option<usize>,
        /// weight on physical invariants (winding / bending / closure)
        #[arg(long)]
        invariant: Option<f64>,
    },
}

// ── KNOCKBACK — the rhythm familiar (headless proof + SVG export) ───────────

fn twin_default_path() -> std::path::PathBuf {
    neuron::runroot::run_root().join("runtime").join("twin.knbk")
}

/// A scripted, deterministic player — a mix that exercises the whole loop: a steady groove
/// that locks into sync, then a hot tryhard burst that earns a storm, then a wind-down. No
/// randomness; the same script always grows the same brain.
fn scripted_motif(i: usize) -> neuron::rhythm::Motif {
    use neuron::rhythm::{Motif, Onset, OnsetKind, Voice};
    let mk = |times: &[u64], e: f32| Motif {
        onsets: times
            .iter()
            .map(|&t| Onset {
                t_ms: t,
                energy: e,
                kind: OnsetKind::Tap,
                voice: Voice::neutral(),
            })
            .collect(),
    };
    match i % 8 {
        // the classic: da-da-da-DA-da
        0 | 1 => mk(&[0, 250, 500, 650, 900], 0.6),
        // steady groove (locks sync)
        2 | 3 => mk(&[0, 300, 600, 900], 0.55),
        // hot tryhard burst (builds heat → storm)
        4 | 5 => mk(&[0, 110, 220, 330, 440, 550, 660, 770], 0.95),
        // wind-down (mirror eases)
        _ => mk(&[0, 700, 1400], 0.35),
    }
}

fn twin_cmd(action: TwinCmd) -> Result<()> {
    use neuron::twin::{Emergent, Familiar, Judgment, TwinConfig};
    match action {
        TwinCmd::Demo { turns, svg } => {
            let mut fam = Familiar::new(TwinConfig::default());
            println!("KNOCKBACK — the rhythm familiar wakes. It has no rhythm of its own.\n");
            let mut last = None;
            for i in 0..turns {
                let m = scripted_motif(i);
                let turn = fam.receive(&m);
                let judged = match turn.judged {
                    Some(Judgment::Harmony) => " · you answered in harmony",
                    Some(Judgment::Counterpoint) => " · you twisted it (counterpoint)",
                    None => "",
                };
                let event = match &turn.event {
                    Some(Emergent::Storm { phrase_len }) => {
                        format!("  ⛈ STORM — it braids {phrase_len} beats and dares you")
                    }
                    Some(Emergent::Stillpoint { depth }) => {
                        format!("  ◦ STILLPOINT — time dilates (sync {depth:.2})")
                    }
                    Some(Emergent::Haunting { age }) => {
                        format!("  ‹ HAUNTING — a phrase from {age} exchanges ago")
                    }
                    None => String::new(),
                };
                println!(
                    "  knock {:>2}: you play {} beats → twin answers {} (flourish +{}){}{}",
                    i + 1,
                    m.len(),
                    turn.knockback.len(),
                    turn.knockback
                        .len()
                        .saturating_sub(turn.knockback.flourish_from),
                    judged,
                    event,
                );
                last = Some(turn);
            }
            let g = fam.signals();
            println!(
                "\nweave: {} exchanges · sync {:.2} · heat {:.2} · novelty {:.1}b · palette depth {}",
                g.exchanges, g.sync, g.heat, g.novelty, g.palette_depth
            );
            if let Some(svg_path) = svg {
                if let Some(turn) = &last {
                    let sc =
                        neuron::scene::knockback_scene(&turn.knockback, 1.0, turn.event.as_ref());
                    std::fs::write(&svg_path, sc.to_svg())?;
                    println!("storyboard SVG → {svg_path}");
                }
            }
            // persist the grown familiar — but NEVER over the real one: the app's live familiar
            // learns from the user's actual play for weeks, and a scripted 16-turn proof silently
            // replacing it is data loss (it happened). A demo brain gets its own file; only a
            // rig with no familiar yet seeds the real path so the demo remains the first-run hook.
            let real = twin_default_path();
            let path = if real.exists() {
                real.with_file_name("twin-demo.knbk")
            } else {
                real
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, fam.save())?;
            println!("familiar saved → {}", path.display());
            if path.file_name().and_then(|n| n.to_str()) == Some("twin-demo.knbk") {
                println!("(your real familiar was left untouched — this demo brain saved beside it)");
            }
        }
        TwinCmd::Stats { file } => {
            let path = file.map_or_else(twin_default_path, std::path::PathBuf::from);
            let fam = if let Some(f) = std::fs::read(&path).ok().and_then(|b| Familiar::load(&b)) { f } else {
                println!(
                    "no familiar at {} — play `neuron twin demo` first.",
                    path.display()
                );
                return Ok(());
            };
            let g = fam.signals();
            let c = fam.ceiling();
            let brain = fam.brain();
            println!("KNOCKBACK familiar — {}", path.display());
            println!("  exchanges absorbed : {}", g.exchanges);
            println!("  memory ring        : {} motifs", fam.memory_len());
            println!("  wells (eigen-modes): {}", brain.wells.len());
            println!("  palette depth      : {}", g.palette_depth);
            println!("  sync / heat        : {:.2} / {:.2}", g.sync, g.heat);
            println!(
                "  demonstrated peak  : ≤{:.0}ms IOI · {:.1} beats/s · {:.0} beats · energy {:.2}",
                c.min_ioi_ms, c.density, c.length, c.energy
            );
        }
        TwinCmd::Sigil { out, file, size } => {
            use neuron::twin::TwinConfig;
            let fam = if let Some(f) = file
                .map(std::path::PathBuf::from)
                .or_else(|| Some(twin_default_path()))
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| Familiar::load(&b)) { f } else {
                // grow one quickly so the export always works
                let mut f = Familiar::new(TwinConfig::default());
                for i in 0..40 {
                    f.receive(&scripted_motif(i));
                }
                f
            };
            let path = fam.sigil_path(360);
            let sc = neuron::scene::sigil_scene(&path, size as f32);
            std::fs::write(&out, sc.to_svg())?;
            println!("sigil → {out} ({} points)", path.len());
        }
        TwinCmd::Stage { out } => {
            use neuron::scene::{
                stage_scene, BraidSeg, StageBeat, HUSH, PHOSPHOR, STAGE_STAFF_HALF, TWIN,
            };
            // a real moment, generated by the real engine: play the classic, take the reply.
            let mut fam = Familiar::new(TwinConfig::default());
            for i in 0..6 {
                fam.receive(&scripted_motif(i));
            }
            let turn = fam.receive(&scripted_motif(0)); // shave-and-a-haircut
            let kb = &turn.knockback;
            let total = kb.duration_ms().max(1) as f32;
            // the session's fitted layout: phrase + one blueprint slot spans the staff.
            let x0 = -STAGE_STAFF_HALF + 26.0;
            let med = {
                let mut iois: Vec<u64> = kb
                    .onsets
                    .windows(2)
                    .map(|w| w[1].t_ms - w[0].t_ms)
                    .collect();
                if iois.is_empty() {
                    350
                } else {
                    iois.sort_unstable();
                    iois[iois.len() / 2].max(120)
                }
            };
            let span = (kb.duration_ms() + med).max(900) as f32;
            let scale = ((STAGE_STAFF_HALF - x0 - 26.0) / span).clamp(0.04, 0.30);
            let beats: Vec<StageBeat> = kb
                .onsets
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let flourish = i >= kb.flourish_from;
                    StageBeat {
                        x: (x0 + o.t_ms as f32 * scale).min(STAGE_STAFF_HALF),
                        y: 0.0,
                        r: (if flourish { 9.0 } else { 6.0 }) + 8.0 * o.energy,
                        color: if flourish { TWIN.lerp(HUSH, 0.4) } else { TWIN },
                        weight: o.energy,
                        phase: 0.5 * (1.0 - o.t_ms as f32 / total),
                        kind: if flourish { 2 } else { 1 },
                    }
                })
                .chain(std::iter::once(StageBeat {
                    x: (x0 + span * scale).min(STAGE_STAFF_HALF + 10.0),
                    y: 0.0,
                    r: 12.0,
                    color: PHOSPHOR,
                    weight: 1.0,
                    phase: 0.0,
                    kind: 3,
                }))
                .collect();
            let shards: Vec<BraidSeg> = (0..7)
                .map(|i| BraidSeg {
                    hue: 160.0 + i as f32 * 18.0,
                    shimmer: 0.5,
                    amp: 0.4 + 0.08 * i as f32,
                })
                .collect();
            let sc = stage_scene(
                &beats,
                -1.0,
                None,
                &shards,
                "answer it \u{2014} finish the line",
                1.0,
            );
            std::fs::write(&out, sc.to_svg())?;
            println!(
                "stage preview → {out} ({} beats incl. blueprint)",
                beats.len()
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // Before ANY config read: carry a build-tree config universe forward (see
    // `runroot::adopt_legacy_run_root`). Both binaries do this because either one can be the first
    // to start after an upgrade, and they share one config universe — whoever gets there first
    // migrates, the other no-ops.
    if let Some((from, to)) = neuron::runroot::adopt_legacy_run_root() {
        eprintln!("carried config forward: {} -> {}", from.display(), to.display());
    }
    let cli = Cli::parse();
    let reg = Registry::load()?;
    // Adoption is NOT a startup step (it would give read-only commands a hidden HID-probe + file
    // write): it fires lazily inside device resolution, the moment a command actually reaches for
    // the bus and comes up short. See `adopt_and_reload`.
    match cli.cmd {
        Cmd::List => list(&reg)?,
        Cmd::Discover { emit } => discover_cmd(emit),
        Cmd::Adopt { dry_run } => adopt_cmd(&reg, dry_run)?,
        Cmd::Info => info(&open_first(&reg)?),
        Cmd::Battery => {
            // Resolve by CAPABILITY, not enumeration order — "the device with a battery",
            // never "the first device" (which is a keyboard whenever one sorts first). Via the
            // local wrapper so a brand-new battery device is adopted-on-miss (see open_with_command).
            let d = open_with_command(&reg, "battery_level")?;
            let pct = cap::battery_percent(&d)?;
            let charging = cap::charging(&d).unwrap_or(false);
            println!(
                "Battery: {pct}%{}",
                if charging { " (charging)" } else { "" }
            );
        }
        Cmd::Dpi { value } => dpi_cmd(&reg, value)?,
        Cmd::Polling { hz } => polling_cmd(&reg, hz)?,
        Cmd::DpiStages {
            stages,
            active,
            persist,
        } => dpi_stages_cmd(&reg, &stages, active, persist)?,
        Cmd::Scroll { stage, volatile } => scroll_cmd(&reg, stage, volatile)?,
        Cmd::Lod { lift, landing, sym } => lod_cmd(&reg, lift, landing, sym)?,
        Cmd::Brightness { pct } => brightness_cmd(&reg, pct)?,
        Cmd::GameMode { state } => gamemode_cmd(&reg, state.as_deref())?,
        Cmd::Sniper { bind, dpi } => sniper_cmd(bind, dpi)?,
        Cmd::Storage { raw } => storage_status(&reg, raw)?,
        Cmd::Watch { seconds } => neuron::controls::watch(seconds),
        Cmd::Gesture { action } => gesture_cmd(action)?,
        Cmd::Audio { action } => audio_cmd(action)?,
        Cmd::Bind { action } => bind_cmd(action)?,
        Cmd::Run { seconds, safe } => run_daemon(&reg, seconds, safe),
        Cmd::Lighting { action } => lighting_cmd(&reg, action)?,
        Cmd::Mode { mode, pid } => mode_cmd(&reg, &mode, &pid)?,
        Cmd::Remap {
            key,
            button,
            to,
            reset,
        } => remap_cmd(&reg, key.as_deref(), button.as_deref(), to.as_deref(), reset)?,
        Cmd::Backup { pid } => backup_cmd(&reg, pid.as_deref())?,
        Cmd::Verify { file } => verify_cmd(&file)?,
        Cmd::Import { deep } => import_cmd(deep),
        Cmd::ImportExport { file, apply } => import_export_cmd(&file, apply)?,
        Cmd::Macro { action } => macro_cmd(action)?,
        Cmd::Radial { action } => radial_cmd(action),
        Cmd::Cast { action } => cast_cmd(action)?,
        Cmd::Profile { action } => profile_cmd(&reg, action)?,
        Cmd::Twin { action } => twin_cmd(action)?,
        Cmd::Probe {
            pid,
            class,
            id,
            scan,
        } => probe_cmd(&pid, class.as_deref(), id.as_deref(), scan)?,
        Cmd::Pocket {
            name,
            list,
            keep,
            sigil,
        } => pocket_cmd(name, list, keep, sigil)?,
        Cmd::Prof {
            action: ProfCmd::Pump,
        } => prof_pump_cmd(),
    }
    Ok(())
}

/// Portable clipboards from the CLI: list every pocket's contents, export a pocket's emergent
/// content-sigil, or MOVE the clipboard into/out of a named pocket. The move writes the clipboard
/// (a real mutation), so it arms input for this one-shot — the process exits right after, and the
/// list/sigil paths stay read-only (no arm).
fn pocket_cmd(name: Option<String>, list: bool, keep: bool, sigil: Option<String>) -> Result<()> {
    let disp = |s: &str| {
        if s.is_empty() {
            "(default)".to_string()
        } else {
            s.to_string()
        }
    };
    if list {
        let all = neuron::pocket::views();
        if all.is_empty() {
            println!("no pockets yet (move something: neuron pocket <name>)");
            return Ok(());
        }
        for (slot, durable, v) in all {
            let kept = if durable { " \u{00b7} kept" } else { "" };
            let extra = if let Some(t) = v.text {
                format!("  \u{201c}{t}\u{201d}")
            } else if !v.files.is_empty() {
                format!("  {}", v.files.join(", "))
            } else {
                String::new()
            };
            println!("  {:<16} {}{kept}{extra}", disp(&slot), v.summary);
        }
        return Ok(());
    }
    let slot = name.unwrap_or_default();
    if let Some(out) = sigil {
        let svg = neuron::pocket::sigil_svg_of(&slot, 480.0);
        if svg.is_empty() {
            println!("pocket {} is empty \u{2014} nothing to draw", disp(&slot));
            return Ok(());
        }
        std::fs::write(&out, svg)?;
        println!("sigil for pocket {} \u{2192} {out}", disp(&slot));
        return Ok(());
    }
    // The move mutates the clipboard — arm for this one-shot run.
    neuron::action::arm_input(true);
    // A CLI pocket is ALWAYS durable: this process exits on the next line, so a RAM-only slot
    // (the resident app's default) would take the user's clipboard with it — a stash that
    // reports success and then destroys the payload. `--keep` stays accepted (it's the app's
    // vocabulary) but the disk mirror is not optional here.
    let _ = keep;
    println!("{}", neuron::pocket::activate(&slot, true));
    // activate() persists on a worker thread; this process exits NOW — flush synchronously or
    // the worker dies mid-write and the stash evaporates (live-verified before this call existed).
    neuron::pocket::flush_durable_sync();
    Ok(())
}

fn profile_cmd(reg: &Registry, action: ProfileCmd) -> Result<()> {
    match action {
        ProfileCmd::List => {
            let names = neuron::profile::list();
            if names.is_empty() {
                println!("no profiles (create one: neuron profile save <name> --dpi 1600)");
            }
            for n in names {
                match Profile::load(&n) {
                    Ok(p) => println!("  {n:<16} {}", p.summary()),
                    Err(_) => println!("  {n:<16} (unreadable)"),
                }
            }
        }
        ProfileCmd::Show { name } => {
            let p = Profile::load(&name)?;
            println!("{name}: {}", p.summary());
        }
        ProfileCmd::Save {
            name,
            dpi,
            polling,
            brightness,
            disable_alt_tab,
            disable_win,
            disable_alt_f4,
            disable_alt_esc,
            idle_secs,
            in_game_wired,
            in_game_dongle,
            persist,
        } => {
            let p = Profile {
                name: name.clone(),
                dpi,
                dpi_stages: Vec::new(),
                polling_hz: polling,
                brightness,
                disable_alt_tab,
                disable_win,
                disable_alt_f4,
                disable_alt_esc,
                idle_secs,
                in_game_polling: match (in_game_wired, in_game_dongle) {
                    (Some(w), Some(d)) => Some((w, d)),
                    _ => None,
                },
                persist,
                ..Default::default()
            };
            if let Some(why) = neuron::profile::name_conflict(&name) {
                bail!("{why}");
            }
            if p.is_empty() {
                bail!("nothing to save — pass at least one setting flag (see 'neuron profile save --help')");
            }
            // The GUI says "overwrite" before it clobbers; the CLI used to overwrite in silence.
            // Same guard, said the way a terminal says it — the write still happens, you just know.
            if let Ok(existing) = Profile::load(&name) {
                println!("overwriting '{name}' ({})", existing.summary());
            }
            p.save().map_err(anyhow::Error::msg)?;
            println!("saved profile '{name}': {}", p.summary());
        }
        ProfileCmd::Apply { name } => profile_apply(reg, &name)?,
        ProfileCmd::Delete { name, yes } => {
            let p = Profile::load(&name)
                .map_err(|_| anyhow::anyhow!("no profile '{name}' (neuron profile list)"))?;
            let sidecar = Profile::rules_path(&name);
            let binds = std::fs::read_to_string(&sidecar)
                .ok()
                .and_then(|s| toml::from_str::<neuron::engine::RuleDoc>(&s).ok())
                .map_or(0, |d| d.rules.len());
            if !yes {
                println!("'{name}': {}", p.summary());
                if binds > 0 {
                    println!("  and {binds} bind(s) in {}", sidecar.display());
                }
                bail!("refusing to delete without --yes");
            }
            Profile::delete(&name).map_err(anyhow::Error::msg)?;
            match binds {
                0 => println!("deleted '{name}'"),
                n => println!("deleted '{name}' and its {n} bind(s)"),
            }
            // Rules are NOT pruned: they are the user's config, and a route is the thing you most
            // likely want to re-point rather than lose. But a dangling route can never fire, so
            // name each one and the exact repair instead of a bare count. (The live daemon no
            // longer churns on them either — it only reassembles the spine when a switch lands.)
            let rules = neuron::profile::AppRules::load();
            let dangling: Vec<&str> = rules
                .rules
                .iter()
                .filter(|r| r.profile == name)
                .map(|r| r.app.as_str())
                .collect();
            if !dangling.is_empty() {
                println!(
                    "  {} auto-switch route(s) still point at '{name}' and can no longer fire: {}",
                    dangling.len(),
                    dangling.join(", ")
                );
                println!(
                    "  re-point or remove them in {}",
                    neuron::profile::AppRules::path().display()
                );
            }
            if rules.default.as_deref() == Some(name.as_str()) {
                println!("  it was also the fallback profile · clear `default` in apps.toml");
            }
        }
        ProfileCmd::Rename { from, to } => {
            let landed = Profile::rename(&from, &to).map_err(anyhow::Error::msg)?;
            println!("renamed '{from}' to '{landed}'");
            // auto-switch rules name a profile by string, so a rename without this quietly breaks them.
            let mut rules = neuron::profile::AppRules::load();
            let mut moved = 0;
            for r in rules.rules.iter_mut().filter(|r| r.profile == from) {
                r.profile.clone_from(&landed);
                moved += 1;
            }
            if rules.default.as_deref() == Some(from.as_str()) {
                rules.default = Some(landed.clone());
                moved += 1;
            }
            if moved > 0 {
                // The profile has already moved by this point, so a failed save leaves apps.toml
                // pointing at a name that is gone. It can't be rolled back (rolling the rename back
                // could itself fail), so say exactly what is broken and how to repair it rather
                // than bailing with a bare IO error the user has to reverse-engineer.
                match rules.save() {
                    Ok(()) => println!("  {moved} auto-switch entr(y/ies) followed it"),
                    Err(e) => {
                        // Two files have to change together and only one can be written atomically,
                        // so on a failed apps.toml write put the PROFILE back rather than leave a
                        // rename that silently broke every route pointing at it. Renaming back is
                        // itself fallible; if it works the whole command is a clean no-op, and if
                        // it doesn't the user gets the exact repair instead of a bare IO error.
                        eprintln!("  apps.toml could not be written: {e}");
                        match neuron::profile::Profile::rename(&landed, &from) {
                            Ok(_) => bail!(
                                "rename rolled back · '{from}' is unchanged and its {moved} \
                                 auto-switch entr(y/ies) still work"
                            ),
                            Err(re) => {
                                eprintln!("  and rolling the rename back failed too: {re}");
                                eprintln!(
                                    "  '{from}' is now '{landed}', but {moved} auto-switch \
                                     entr(y/ies) still name '{from}' and will not fire."
                                );
                                eprintln!(
                                    "  repair: edit {} and change '{from}' to '{landed}'.",
                                    neuron::profile::AppRules::path().display()
                                );
                                bail!("apps.toml not updated");
                            }
                        }
                    }
                }
            }
        }
        ProfileCmd::Capture { name } => profile_capture(reg, &name)?,
        ProfileCmd::Autoswitch { app, profile } => {
            use neuron::profile::{AppRule, AppRules};
            if let (Some(a), Some(p)) = (app, profile) {
                // the rule is only as real as its target — the GUI refuses a typo'd profile,
                // and the CLI used to accept one and fail forever at focus-switch time.
                if Profile::load(&p).is_err() {
                    bail!("no profile '{p}' · save it first (neuron profile list)");
                }
                let mut rules = AppRules::load();
                rules.rules.push(AppRule {
                    app: a.clone(),
                    profile: p.clone(),
                });
                rules.save().map_err(|e| anyhow::anyhow!("saving apps.toml: {e}"))?;
                println!(
                    "rule added: focus '{a}' -> profile '{p}'  ({})",
                    AppRules::path().display()
                );
            } else {
                let rules = AppRules::load();
                let now = neuron::app::foreground_app();
                println!("focused app: {}", now.as_deref().unwrap_or("(unknown)"));
                if rules.rules.is_empty() {
                    println!(
                        "no rules yet — add one: neuron profile autoswitch <app> <profile>"
                    );
                }
                for r in &rules.rules {
                    let hit = now
                        .as_deref()
                        .is_some_and(|n| n.contains(&r.app.to_lowercase()));
                    println!(
                        "  '{}' -> {}{}",
                        r.app,
                        r.profile,
                        if hit { "   <= active" } else { "" }
                    );
                }
            }
        }
    }
    Ok(())
}

/// Capture the current live device state into a profile — the no-gimmick Synapse import: read
/// what's actually on the hardware (which Synapse configured) rather than decrypting its files.
fn profile_capture(reg: &Registry, name: &str) -> Result<()> {
    let p = neuron::profile::capture_from_devices(
        reg,
        name,
        neuron::writes::GamingMode::default(),
        false,
        0,  // the CLI is stateless — no selected device; capability-based first match
        "", // and no selected physical unit either
        "", // and no selected dialect plane — empty = any family, same no-selection semantics
    );

    if p.is_empty() {
        bail!(
            "nothing captured — the device(s) are asleep/unreadable (wiggle the mouse and retry)"
        );
    }
    p.save().map_err(anyhow::Error::msg)?;
    println!("captured '{name}' from live device state: {}", p.summary());
    Ok(())
}

/// Apply a profile — write each set field to whichever device owns that capability. The
/// orchestration spine: settings via the typed setters, lighting via the unified backend.
fn profile_apply(reg: &Registry, name: &str) -> Result<()> {
    let p = Profile::load(name)?;
    println!("applying '{name}': {}", p.summary());

    // Delegate to the ONE canonical apply orchestration in neuron-core (`Profile::apply`). It applies
    // the FULL DPI stage list (not just the single active DPI), polling/brightness, the per-LED frame
    // and named effect across all lit devices, plus the gated idle/in-game writes (reported honestly
    // as `[gated]`, never faked), and returns the host-side gaming-mode policy. The GUI renders apply
    // from this exact same report — no duplicated apply logic.
    let report = p.apply(reg);
    for line in &report.applied {
        println!("  {line}");
    }
    for line in &report.gated {
        println!("  {line} [gated — derived write off by default; enable via env flag]");
    }
    for line in &report.skipped {
        println!("  skipped: {line}");
    }
    // Push the applied profile's gaming-mode policy to the ONE shared carrier in `neuron::hook`
    // (the same cell the GUI drives). If the run-daemon's listener thread is live it picks this up on
    // its next `reconcile` and (de)installs the WH_KEYBOARD_LL suppression hook to match — so apply
    // works from the daemon's AppFocus->ProfileSwitch path AND the one-shot CLI `profile apply`.
    neuron::hook::set_policy(report.gaming_mode);
    if report.gaming_mode.any() {
        println!(
            "  gaming-mode -> policy active (host-side Alt+Tab/Win/Alt+F4 suppression {})",
            if neuron::hook::is_installed() {
                "hook live on the daemon listener thread"
            } else {
                "hook arms when the `run` daemon is listening"
            }
        );
    }
    Ok(())
}

fn cast_cmd(action: CastCmd) -> Result<()> {
    match action {
        CastCmd::Show => cast_show(),
        CastCmd::Init { force } => {
            if CastConfig::path().exists() && !force {
                bail!("cast.toml already exists (use --force to overwrite)");
            }
            neuron::salvage::atomic_write(&CastConfig::path(), neuron::cast::TEMPLATE_TOML.as_bytes())?;
            println!(
                "wrote {} — record glyphs with `neuron gesture record <name>`, then bind them.",
                CastConfig::path().display()
            );
        }
        CastCmd::Run { trigger } => cast_run(trigger),
    }
    Ok(())
}

fn cast_show() {
    let cfg = CastConfig::load();
    let custom = CastConfig::path().exists();
    println!(
        "cast config ({}):  trigger={}  mode={:?}  sectors={}  deadzone={}",
        if custom { "cast.toml" } else { "defaults" },
        cfg.trigger.label(),
        cfg.mode,
        cfg.sectors,
        cfg.deadzone
    );
    println!("  radial wheel:");
    if cfg.radial.is_empty() {
        println!("    (none bound)");
    }
    for (i, a) in cfg.radial.iter().enumerate() {
        println!(
            "    [{i}] {:>3}  ->  {}",
            radial::compass(i, cfg.sectors),
            a.describe()
        );
    }
    println!("  glyph spells:");
    if cfg.gestures.is_empty() {
        println!("    (none bound)");
    }
    for (name, a) in &cfg.gestures {
        println!("    {name:<14} ->  {}", a.describe());
    }
}

fn cast_run(trigger_override: Option<i32>) {
    let cfg = CastConfig::load();
    let vault = Vault::load();
    let trigger = trigger_override
        .map_or(cfg.trigger, neuron::controls::ControlRef::from_vk);
    println!(
        "Cast engine — mode={:?}, trigger={}, {} wheel wedge(s), {} glyph(s), {} recorded template(s).",
        cfg.mode,
        trigger.label(),
        cfg.radial.len(),
        cfg.gestures.len(),
        vault.templates.len()
    );
    println!("HOLD the trigger, FLICK a direction or DRAW a shape, release. ESC to stop.\n");
    loop {
        let path = glyph::capture_held(trigger, 4096);
        if path.is_empty() {
            break; // ESC / aborted
        }
        match cfg.resolve(&path, &vault) {
            Some(r) => {
                let res = r.action.run();
                println!(
                    "  [{}] {}  ->  {}   ({res})",
                    r.kind,
                    r.label,
                    r.action.describe()
                );
            }
            None => println!("  (no match / cancelled)"),
        }
    }
    println!("\nstopped.");
}

fn radial_cmd(action: RadialCmd) {
    match action {
        RadialCmd::Map { sectors } => radial_map(sectors),
        RadialCmd::Pick { trigger, sectors } => radial_pick(trigger, sectors),
    }
}

fn radial_map(sectors: usize) {
    let m = RadialMenu {
        sectors,
        deadzone: 40.0,
        items: vec![],
    };
    println!("Radial wheel — {sectors} sectors (sector 0 = North/up, clockwise):\n");
    for s in 0..sectors {
        let deg = (s as f64 * 360.0 / sectors.max(1) as f64).round() as i32;
        println!(
            "  [{s:>2}]  {:>5}  {}",
            format!("{deg}\u{00b0}"),
            m.label(s)
        );
    }
}

fn radial_pick(trigger: i32, sectors: usize) {
    let m = RadialMenu {
        sectors,
        deadzone: 40.0,
        items: vec![],
    };
    println!("Radial: HOLD trigger 0x{trigger:02X}, flick a direction, release (ESC aborts)...");
    let path = glyph::capture_held(neuron::controls::ControlRef::from_vk(trigger), 4096);
    if path.len() < 2 {
        println!("(no motion captured)");
        return;
    }
    let (dx, dy) = radial::net_displacement(&path);
    let dist = (dx * dx + dy * dy).sqrt();
    match m.select(&path) {
        Some(s) => println!("=> sector {s}: {}   (flick {dist:.0} units)", m.label(s)),
        None => println!(
            "=> cancelled (flick {dist:.0} < deadzone {:.0})",
            m.deadzone
        ),
    }
}

/// Recognized, connected devices that declare a `[lighting]` block.
fn lit_devices(reg: &Registry) -> Result<Vec<(DeviceDef, u16, lighting::LightingDef)>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for i in &transport::enumerate()? {
        // find_for_pipe: the def that DRIVES this pipe, so a two-family pid resolves each pipe to
        // the family that can actually paint it (find_by_pid + matches_control missed the second).
        if let Some(def) = reg.find_for_pipe(i) {
            // one lighting row per (pid, family) — two families on one pid are two independently-
            // drivable lighting planes, and the pid-only key silently dropped the second (review-caught).
            if let Some(light) = &def.lighting {
                if seen.insert((i.pid, def.dialect.clone())) {
                    out.push((def.clone(), i.pid, light.clone()));
                }
            }
        }
    }
    Ok(out)
}

fn lighting_cmd(reg: &Registry, action: Option<LightingCmd>) -> Result<()> {
    match action {
        None => lighting_show(reg),
        Some(LightingCmd::Run {
            name,
            color,
            seconds,
            fps,
            pid,
        }) => lighting_run(
            reg,
            &name,
            color.as_deref(),
            seconds,
            fps.max(1),
            pid.as_deref(),
        ),
        Some(LightingCmd::Effect {
            name,
            color,
            brightness,
            apply,
            pid,
            led,
            effect_id,
            raw,
            cmd_id,
            emulate,
            store,
        }) => lighting_effect(
            reg,
            &name,
            color.as_deref(),
            brightness,
            apply,
            pid.as_deref(),
            led.as_deref(),
            effect_id.as_deref(),
            raw.as_deref(),
            cmd_id.as_deref(),
            emulate,
            store,
        ),
        Some(LightingCmd::Mirror { seconds }) => lighting_mirror(reg, seconds),
        Some(LightingCmd::Keytest { pid, dwell, color }) => {
            lighting_keytest(reg, &pid, dwell, color.as_deref())
        }
        Some(LightingCmd::Cellsweep {
            pid,
            dwell,
            row,
            color,
        }) => lighting_cellsweep(reg, &pid, dwell, row, color.as_deref()),
        Some(LightingCmd::Cells {
            row,
            from,
            to,
            pid,
            color,
            seconds,
        }) => lighting_cells(reg, &pid, row, from, to, color.as_deref(), seconds),
    }
}

/// Animate a PRESET live: set custom-frame mode once, then stream the composited Pattern × Spectrum.
/// `name` is a preset slug (the same looks the GUI tile grid offers); `--color`, when given, repaints a
/// colour-driven look (one whose spectrum is a solid) in that single colour — the rainbow/ramp looks own
/// their palette and ignore it. This is the open-effects engine running live: one `Compositor::render`
/// per frame.
fn lighting_run(
    reg: &Registry,
    name: &str,
    color: Option<&str>,
    seconds: u64,
    fps: u64,
    pid_filter: Option<&str>,
) -> Result<()> {
    let mut layer = neuron::pattern::preset_layer(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown look '{name}' (try: {})",
            neuron::pattern::presets()
                .iter()
                .map(|p| p.slug)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    // a colour override repaints a colour-driven look (its spectrum is a single solid stop) as that
    // colour; a palette/ramp look keeps its own spectrum (the colour is intrinsic).
    if let Some(s) = color {
        let c = Rgb::parse(s).ok_or_else(|| anyhow::anyhow!("bad colour '{s}'"))?;
        if layer.spectrum.is_solid() {
            layer.spectrum = neuron::spectrum::Spectrum::solid(c);
        } else {
            eprintln!("note: '{name}' owns its palette — --color ignored");
        }
    }
    let want = match pid_filter {
        Some(s) => Some(parse_hex16(s)?),
        None => None,
    };
    let (def, pid, l) = lit_devices(reg)?
        .into_iter()
        .find(|(_, p, light)| {
            want.is_none_or(|w| w == *p) && light.custom_frame.is_some()
        })
        .ok_or_else(|| anyhow::anyhow!("no custom-frame-capable lit device found"))?;
    let d = Device::open(def.clone(), pid)?;

    // The orchestration (driver mode, frame streaming, fire-and-forget) is the backend's job; the
    // visuals are the compositor's (a single-layer stack here). The CLI just wires them together.
    let lights = lighting::Lights::new(&d, l);
    println!(
        "streaming '{name}' on {} — {seconds}s @ {fps}fps (Pattern × Spectrum engine)...",
        def.name
    );
    let mut comp = neuron::pattern::Compositor::from_defs(&[layer]);
    lights.animate(&mut comp, || fps as u32, seconds, || false)?;
    println!("done.");
    Ok(())
}

/// Read the mouse's current active DPI stage as (`active_idx` 0-based, `stage_count`). The active stage
/// is the reply to the GET dpi-stages command `0x04/0x86` (`dpi_stages_active`): `args[1]` is the
/// active index, `args[2]` the count — live-confirmed. Falls back to `dpi_stages` (0x04/0x83) if the
/// user-configured-stage getter isn't present. Returns None if neither answers (e.g. asleep mouse).
fn read_active_dpi_stage(d: &Device) -> Option<(u8, u8)> {
    let s = d
        .run("dpi_stages_active")
        .or_else(|_| d.run("dpi_stages"))
        .ok()?;
    let active = decode_dpi_active(&s).unwrap_or(0);
    let count = decode_dpi_stages(&s).len() as u8;
    Some((active, count))
}

/// Read the mouse's full live vitals: battery %, charging, and the active DPI stage. Each read is
/// independent + best-effort — a wireless mouse can be asleep, so a failed sub-read falls back to the
/// last known value rather than aborting the whole surface.
fn read_mouse_vitals(d: &Device, last: Option<lighting::Vitals>) -> lighting::Vitals {
    let prev = last.unwrap_or(lighting::Vitals {
        battery_pct: 0,
        charging: false,
        active_stage: 0,
        stage_count: 0,
    });
    let battery_pct = cap::battery_percent(d).unwrap_or(prev.battery_pct);
    let charging = cap::charging(d).unwrap_or(prev.charging);
    let (active_stage, stage_count) =
        read_active_dpi_stage(d).unwrap_or((prev.active_stage, prev.stage_count));
    lighting::Vitals {
        battery_pct,
        charging,
        active_stage,
        stage_count,
    }
}

/// CROSS-DEVICE DATA SURFACE — the on-thesis flagship. neuron is ONE process speaking BOTH the Naga
/// (data SOURCE) and the `BlackWidow` (data SINK), so it can paint the mouse's live battery / charge /
/// active-DPI-stage onto the keyboard's LED matrix — a cross-device layer Synapse (siloed) and
/// `OpenRazer` (no cross-device layer) structurally cannot do.
///
/// The loop runs at ~1s cadence: read the mouse vitals -> render the vitals frame (`render_vitals`,
/// the SAME reusable core the GUI will call) -> paint it to the keyboard via the ACK'd custom-frame
/// path (`Lights::paint` -> `apply_lighting`), NOT the fast stream. The slow legacy V2 is repainted
/// ON-DEMAND — only when the vitals changed (or, while charging, each tick so the cyan sweep animates).
fn lighting_mirror(reg: &Registry, seconds: Option<u64>) -> Result<()> {
    use std::time::{Duration, Instant};

    // SINK: the keyboard — the first custom-frame-capable lit device. (lit_devices yields every lit
    // device; we want one that can paint a per-LED frame, which is the keyboard's legacy matrix.)
    let (kbd_def, kbd_pid, kbd_l) = lit_devices(reg)?
        .into_iter()
        .find(|(_, _, light)| light.custom_frame.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!("no custom-frame-capable lit device found (is the keyboard connected?)")
        })?;
    let (rows, cols) = (kbd_l.rows, kbd_l.cols);
    let kbd = Device::open(kbd_def.clone(), kbd_pid)?;
    let lights = lighting::Lights::new(&kbd, kbd_l);
    lights.ensure_control()?; // take host control of the keyboard (driver mode) once.

    // SOURCE: the mouse — the first device exposing the battery getter. Handle it being absent/asleep
    // gracefully: without it we can't mirror, so report clearly instead of painting a phantom surface.
    let mouse = open_with_command(reg, "battery_level").map_err(|e| {
        anyhow::anyhow!(
            "no battery-capable mouse found to mirror from ({e}). The keyboard ({}) is ready as the \
             sink — connect/wake the Naga and re-run.",
            kbd_def.name
        )
    })?;

    println!(
        "MIRROR — painting {} vitals onto {} ({rows}x{cols} matrix). ~1s cadence; {}.",
        mouse.def.name,
        kbd_def.name,
        match seconds {
            Some(n) => format!("{n}s then stop"),
            None => "ESC to stop".into(),
        }
    );

    let start = Instant::now();
    let mut last: Option<lighting::Vitals> = None;
    let mut phase: f32 = 0.0;
    loop {
        if key_down(0x1B) || seconds.is_some_and(|s| start.elapsed().as_secs() >= s) {
            break;
        }
        let v = read_mouse_vitals(&mouse, last);
        let changed = last != Some(v);
        // Repaint when the vitals changed, on the first tick, OR every tick while charging (so the
        // cyan charging sweep animates). A stable, discharging surface is left untouched — the slow
        // board isn't hammered.
        if changed || last.is_none() || v.charging {
            phase = (phase + 0.18) % 1.0; // advance the charging sweep
            let frame = lighting::render_vitals(v, rows, cols, phase);
            match lights.paint_frame(&frame) {
                Ok(()) => println!(
                    "  battery {}%{} \u{00b7} stage {}/{} \u{2192} painted{}",
                    v.battery_pct,
                    if v.charging { " (charging)" } else { "" },
                    v.active_stage + 1,
                    v.stage_count.max(1),
                    if changed { " [changed]" } else { "" }
                ),
                Err(e) => println!("  paint failed: {e}"),
            }
            last = Some(v);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("stopped.");
    Ok(())
}

/// OPTIONAL developer aid — re-verify the standard Razer key map by eye (NEVER a required user step).
/// Walks [`lighting::razer_keyboard_keys`] in reading order and lights ONLY each key's mapped cell (a
/// single-cell custom frame, painted through the ACK'd on-demand path — `Lights::paint_frame` ->
/// `apply_lighting`, NOT the fast stream), holding it for `dwell` ms so the user can eyeball whether the
/// LIT key matches its printed name. Any mismatch is a map entry to correct in
/// [`lighting::razer_key_cell`]. ESC interrupts between (and during) keys.
fn lighting_keytest(reg: &Registry, pid: &str, dwell: u64, color: Option<&str>) -> Result<()> {
    use std::time::{Duration, Instant};
    let want = parse_hex16(pid)?;
    let accent = match color {
        Some(s) => Rgb::parse(s).ok_or_else(|| anyhow::anyhow!("bad colour '{s}', want RRGGBB"))?,
        None => Rgb::new(0, 200, 255), // bright cyan accent
    };
    let (def, pid, l) = lit_devices(reg)?
        .into_iter()
        .find(|(_, p, light)| *p == want && light.custom_frame.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no custom-frame-capable lit device with pid {want:04x} (is the keyboard connected?)"
            )
        })?;
    let (rows, cols) = (l.rows as usize, l.cols as usize);
    let n = rows * cols;
    let d = Device::open(def.clone(), pid)?;
    let lights = lighting::Lights::new(&d, l);
    lights.ensure_control()?; // take host control (driver mode) once so the paint renders.

    let keys = lighting::razer_keyboard_keys();
    println!(
        "KEYTEST — walking {} keys on {} ({rows}x{cols}). Each key's mapped cell lights for {dwell}ms; \
         watch for any key whose LIT POSITION doesn't match its name. ESC to stop.\n",
        keys.len(),
        def.name
    );
    let mut stopped = false;
    'walk: for &name in keys {
        if key_down(0x1B) {
            stopped = true;
            break;
        }
        let Some((ry, cx)) = lighting::razer_key_cell(name) else {
            continue;
        };
        let (ry, cx) = (ry as usize, cx as usize);
        if ry >= rows || cx >= cols {
            continue; // a cell outside this board's matrix — skip rather than paint out of bounds.
        }
        let mut frame = vec![Rgb::BLACK; n];
        frame[ry * cols + cx] = accent;
        lights.paint_frame(&frame)?;
        println!("lighting: {name} (row {ry}, col {cx})");
        // dwell, staying interruptible: poll ESC in small slices instead of one blocking sleep.
        let until = Instant::now() + Duration::from_millis(dwell);
        while Instant::now() < until {
            if key_down(0x1B) {
                stopped = true;
                break 'walk;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // always clear the board at the end (interrupted or complete).
    lights.paint_frame(&vec![Rgb::BLACK; n])?;
    println!("\n{} — board cleared.", if stopped { "stopped (ESC)" } else { "done" });
    Ok(())
}

/// REVERSE-ENGINEER wide-key LED footprints. Walks the FULL device matrix — every `(row, col)` for
/// row in `0..rows`, col in `0..cols`, INCLUDING the unmapped "gap" cells under wide keys that the
/// keymap and [`lighting_keytest`] skip — lighting ONLY that single cell in a bright accent via the
/// ACK'd on-demand custom-frame paint (`Lights::paint_frame` -> `apply_lighting`, NOT the fast
/// stream), holding each for `dwell` ms so the user can note which cells a wide key (space, the
/// shifts, enter, backspace) physically spans. `--row` restricts the sweep to one row (e.g. row 5
/// for the space bar) so you needn't walk all the cells. ESC interrupts between/during cells; the
/// board is cleared at the end.
fn lighting_cellsweep(
    reg: &Registry,
    pid: &str,
    dwell: u64,
    only_row: Option<u8>,
    color: Option<&str>,
) -> Result<()> {
    use std::time::{Duration, Instant};
    let want = parse_hex16(pid)?;
    let accent = match color {
        Some(s) => Rgb::parse(s).ok_or_else(|| anyhow::anyhow!("bad colour '{s}', want RRGGBB"))?,
        None => Rgb::new(0, 200, 255), // bright cyan accent (same as keytest)
    };
    let (def, pid, l) = lit_devices(reg)?
        .into_iter()
        .find(|(_, p, light)| *p == want && light.custom_frame.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no custom-frame-capable lit device with pid {want:04x} (is the keyboard connected?)"
            )
        })?;
    let (rows, cols) = (l.rows as usize, l.cols as usize);
    let n = rows * cols;
    let d = Device::open(def.clone(), pid)?;
    let lights = lighting::Lights::new(&d, l);
    lights.ensure_control()?; // take host control (driver mode) once so the paint renders.

    // which rows to walk: a single requested row, or all of them.
    let row_range = match only_row {
        Some(r) => {
            let r = r as usize;
            if r >= rows {
                bail!("row {r} is out of range — this matrix has {rows} rows (0..{})", rows - 1);
            }
            r..r + 1
        }
        None => 0..rows,
    };
    let cell_count = row_range.len() * cols;
    println!(
        "CELLSWEEP — walking {cell_count} cell(s) of {} ({rows}x{cols}{}). Each cell lights for \
         {dwell}ms; note which cells a wide key (space/shift/enter/backspace) spans. ESC to stop.\n",
        def.name,
        match only_row {
            Some(r) => format!(", row {r} only"),
            None => String::new(),
        }
    );
    let mut stopped = false;
    'walk: for ry in row_range {
        for cx in 0..cols {
            if key_down(0x1B) {
                stopped = true;
                break 'walk;
            }
            let mut frame = vec![Rgb::BLACK; n];
            frame[ry * cols + cx] = accent;
            lights.paint_frame(&frame)?;
            println!("cell (row {ry}, col {cx})");
            // dwell, staying interruptible: poll ESC in small slices instead of one blocking sleep.
            let until = Instant::now() + Duration::from_millis(dwell);
            while Instant::now() < until {
                if key_down(0x1B) {
                    stopped = true;
                    break 'walk;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    // always clear the board at the end (interrupted or complete).
    lights.paint_frame(&vec![Rgb::BLACK; n])?;
    println!("\n{} — board cleared.", if stopped { "stopped (ESC)" } else { "done" });
    Ok(())
}

/// BLOCK-VERIFY a wide key's LED span: light a CONTIGUOUS BLOCK of cells (row N, colA..=colB) ALL AT
/// ONCE and HOLD, so the cells a wide key physically covers can be confirmed on hardware. Paints the
/// whole block simultaneously through the ACK'd on-demand custom-frame path (`Lights::paint_frame` ->
/// `apply_lighting`, NOT the fast stream), holds it (ESC-interruptible, or `--seconds N`), then clears
/// the board. E.g. `--row 5 --from 4 --to 10` lights exactly the space bar (7 cells).
fn lighting_cells(
    reg: &Registry,
    pid: &str,
    row: u8,
    from: u8,
    to: u8,
    color: Option<&str>,
    seconds: Option<u64>,
) -> Result<()> {
    use std::time::{Duration, Instant};
    let want = parse_hex16(pid)?;
    let accent = match color {
        Some(s) => Rgb::parse(s).ok_or_else(|| anyhow::anyhow!("bad colour '{s}', want RRGGBB"))?,
        None => Rgb::new(0, 200, 255), // bright cyan accent (same as keytest/cellsweep)
    };
    let (lo, hi) = (from.min(to), from.max(to)); // tolerate --from/--to given in either order
    let (def, pid, l) = lit_devices(reg)?
        .into_iter()
        .find(|(_, p, light)| *p == want && light.custom_frame.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no custom-frame-capable lit device with pid {want:04x} (is the keyboard connected?)"
            )
        })?;
    let (rows, cols) = (l.rows as usize, l.cols as usize);
    let n = rows * cols;
    let row_u = row as usize;
    if row_u >= rows {
        bail!("row {row} is out of range — this matrix has {rows} rows (0..{})", rows - 1);
    }
    if hi as usize >= cols {
        bail!("col {hi} is out of range — this matrix has {cols} cols (0..{})", cols - 1);
    }
    let d = Device::open(def.clone(), pid)?;
    let lights = lighting::Lights::new(&d, l);
    lights.ensure_control()?; // take host control (driver mode) once so the paint renders.

    // paint the whole contiguous block at once and hold it.
    let mut frame = vec![Rgb::BLACK; n];
    for cx in lo as usize..=hi as usize {
        frame[row_u * cols + cx] = accent;
    }
    lights.paint_frame(&frame)?;
    let k = (hi - lo) as usize + 1;
    println!(
        "CELLS — {} ({rows}x{cols}). lit: row {row}, cols {lo}..={hi} ({k} cells). holding ({}).",
        def.name,
        match seconds {
            Some(s) => format!("{s}s then clear"),
            None => "ESC to clear".into(),
        }
    );

    // hold the block lit: ESC-interruptible, or until --seconds elapses, polling in small slices.
    let start = Instant::now();
    while !key_down(0x1B) && seconds.is_none_or(|s| start.elapsed().as_secs() < s) {
        std::thread::sleep(Duration::from_millis(20));
    }
    // clear the board on the way out.
    lights.paint_frame(&vec![Rgb::BLACK; n])?;
    println!("board cleared.");
    Ok(())
}

fn lighting_show(reg: &Registry) -> Result<()> {
    let devs = lit_devices(reg)?;
    if devs.is_empty() {
        println!("No lighting-capable Razer devices connected.");
        return Ok(());
    }
    for (def, pid, l) in devs {
        let native: Vec<&str> = l.effects.keys().map(std::string::String::as_str).collect();
        let avail: Vec<&str> = l.available().iter().map(|e| e.name()).collect();
        println!("{} [{}]  pid={pid:04x}", def.name, def.codename);
        println!(
            "  protocol: {:?} ({} dialect)   matrix {}x{} = {} LEDs",
            l.protocol,
            match l.protocol {
                lighting::Protocol::Legacy => "class 0x03",
                lighting::Protocol::Matrix => "class 0x0F",
            },
            l.rows,
            l.cols,
            l.led_count()
        );
        println!("  native effects:  {}", native.join(", "));
        println!("  available (native + emulated):  {}", avail.join(", "));
        if let Ok(d) = Device::open(def.clone(), pid) {
            match cap::brightness_percent(&d) {
                Ok(b) => println!("  brightness: {b}% (live)"),
                Err(_) => println!("  brightness: n/a"),
            }
        }
        println!();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn lighting_effect(
    reg: &Registry,
    name: &str,
    color: Option<&str>,
    brightness: Option<u8>,
    apply: bool,
    pid_filter: Option<&str>,
    led_override: Option<&str>,
    effect_id_override: Option<&str>,
    raw: Option<&str>,
    cmd_id_override: Option<&str>,
    emulate: bool,
    store: bool,
) -> Result<()> {
    let cmd_id_byte = match cmd_id_override {
        Some(s) => Some(parse_hex16(s)? as u8),
        None => None,
    };
    let raw_bytes: Option<Vec<u8>> = match raw {
        Some(s) => Some(
            s.split(|c: char| c.is_whitespace() || c == ',')
                .filter(|t| !t.is_empty())
                .map(|t| u8::from_str_radix(t.trim_start_matches("0x"), 16))
                .collect::<std::result::Result<Vec<u8>, _>>()
                .map_err(|_| anyhow::anyhow!("bad --raw hex bytes"))?,
        ),
        None => None,
    };
    let eff = Effect::from_name(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown effect '{name}' (try: {})",
            Effect::ALL
                .iter()
                .map(|e| e.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let color = match color {
        Some(s) => {
            Some(Rgb::parse(s).ok_or_else(|| anyhow::anyhow!("bad colour '{s}', want RRGGBB"))?)
        }
        None => None,
    };
    let led_byte = match led_override {
        Some(s) => Some(parse_hex16(s)? as u8),
        None => None,
    };
    let effect_id_byte = match effect_id_override {
        Some(s) => Some(parse_hex16(s)? as u8),
        None => None,
    };
    let want_pid = match pid_filter {
        Some(s) => Some(parse_hex16(s)?),
        None => None,
    };
    let devs: Vec<_> = lit_devices(reg)?
        .into_iter()
        .filter(|(_, pid, _)| want_pid.is_none_or(|w| w == *pid))
        .collect();
    if devs.is_empty() {
        println!("No matching lighting-capable Razer devices connected.");
        return Ok(());
    }

    let mode = if apply { "APPLY" } else { "DRY-RUN" };
    println!(
        "{mode} '{}' — exact bytes per device (one command, each dialect):\n",
        eff.name()
    );
    for (def, pid, l) in devs {
        println!("{}  pid={pid:04x}  [{:?}]", def.name, l.protocol);

        // EMULATION PATH: compute a frame host-side and stream it as custom-frame rows, then
        // display it. This is the open-effects engine — any effect is just a frame generator.
        if emulate {
            let Some(_) = &l.custom_frame else {
                println!("  no custom_frame command — can't emulate on this device");
                continue;
            };
            let base = color.unwrap_or(Rgb::new(0, 255, 0));
            let frame = lighting::render_frame(eff, l.rows, l.cols, 0.0, base);
            let reports = l.frame_reports(&frame);
            let disp = l.custom_display_report();
            println!(
                "  emulated: {} frame-row write(s) + custom display {}",
                reports.len(),
                disp.preview()
            );
            if apply {
                let d = Device::open(def.clone(), pid)?;
                if let Ok(m) = d.run("device_mode") {
                    if m[0] != 0x03 {
                        println!("  ! not in driver mode (0x{:02X}) — run: neuron mode driver --pid {pid:04x}", m[0]);
                    }
                }
                for r in &reports {
                    d.apply_lighting(r)?;
                }
                d.apply_lighting(&disp)?;
                println!(
                    "  -> painted {} rows + displayed custom frame. ACKed.",
                    reports.len()
                );
            } else {
                for r in reports.iter().take(2) {
                    println!("    {}", r.preview());
                }
                if reports.len() > 2 {
                    println!("    ... (+{} more rows)", reports.len() - 2);
                }
            }
            println!();
            continue;
        }

        let native = if let Some(rb) = &raw_bytes {
            Some(lighting::Report {
                class: l.effect.class,
                id: l.effect.id,
                args: rb.clone(),
                tx: l.effect.transaction_id,
                size: None, // --raw is a transparent probe: data_size = exactly the bytes given
            })
        } else {
            // persist=false: this is the raw PROBE path — the `store` override below pokes the
            // varstore byte manually (even on legacy, deliberately) rather than via the
            // translation layer's Matrix-only persist.
            l.native_effect_report(eff, color, false)
        }
        .map(|mut rep| {
            // prefix is [varstore, led_id, ...]; allow probing overrides.
            if store && !rep.args.is_empty() {
                rep.args[0] = 0x01;
            }
            if let Some(led) = led_byte {
                if rep.args.len() > 1 {
                    rep.args[1] = led;
                }
            }
            if let Some(fid) = effect_id_byte {
                if rep.args.len() > 2 {
                    rep.args[2] = fid;
                }
            }
            if let Some(cid) = cmd_id_byte {
                rep.id = cid;
            }
            rep
        });
        if let Some(rep) = &native {
            println!("  native firmware effect:  {}", rep.preview());
        } else if eff.is_emulatable() && l.custom_frame.is_some() {
            let base = color.unwrap_or(Rgb::new(0, 255, 0));
            let frame = lighting::render_frame(eff, l.rows, l.cols, 0.0, base);
            let reps = l.frame_reports(&frame);
            println!(
                "  emulated via {} streamed frame-row report(s):",
                reps.len()
            );
            for r in reps.iter().take(2) {
                println!("    {}", r.preview());
            }
            if reps.len() > 2 {
                println!("    ... (+{} more rows per frame)", reps.len() - 2);
            }
        } else {
            println!("  not supported on this device");
        }
        let bright = brightness.and_then(|b| l.brightness_report(b));
        if let (Some(value), Some(rep)) = (brightness, &bright) {
            println!("  brightness {value}%:  {}", rep.preview());
        }

        if apply {
            // Safety: only volatile NOSTORE devices, and only native single-report effects
            // for this first validation path (frame streaming comes after opcodes are proven).
            if l.varstore != 0 {
                println!("  -> REFUSED: this device persists to onboard (varstore != 0). Validate the NOSTORE mouse first.\n");
                continue;
            }
            let Some(rep) = native else {
                println!(
                    "  -> REFUSED: apply currently supports native single-report effects only.\n"
                );
                continue;
            };
            let d = Device::open(def.clone(), pid)?;
            // Host effects only render in DRIVER mode — warn loudly if not (this is the trap
            // that made a successful write look like a dead device).
            if let Ok(m) = d.run("device_mode") {
                if m[0] != 0x03 {
                    println!(
                        "  ! device is in mode 0x{:02X}, not driver (0x03) — the write will be",
                        m[0]
                    );
                    println!("    ACKed but WON'T render. First run:  neuron mode driver --pid {pid:04x}");
                }
            }
            // backup-before-write: capture prior lighting state for the restore reference.
            let prior = d
                .run("lighting_state")
                .or_else(|_| d.run("brightness"))
                .ok();
            if let Some(p) = &prior {
                let mut hex = String::new();
                for b in &p[..8] {
                    let _ = write!(hex, "{b:02X} ");
                }
                println!("     prior state (restore ref): {hex}");
            }
            print!("  -> applying… ");
            d.apply_lighting(&rep)?;
            if let Some(rep) = &bright {
                d.apply_lighting(rep)?;
            }
            println!("ACKed.");
            // verify-after-write — but only where the getter is REAL. The matrix era (Naga,
            // 0x0F/0x82) returns a true effect-state whose arg[2] is the effect-id we wrote, so
            // we read it back. The LEGACY board (Chroma V2) has NO reliable lighting getter: its
            // 0x03/0x0A matrix-effect write does NOT update the old per-LED-effect registers
            // (0x03/0x82, 0x03/0x88 return junk), so a "verify" there would be a lie. Be honest:
            // legacy writes are ACK-confirmed only.
            match l.protocol {
                lighting::Protocol::Matrix => {
                    if let Ok(st) = d.run("lighting_state") {
                        // matrix args = [varstore, led, effect_id, ...]; effect-id is arg[2].
                        match rep.args.get(2).copied() {
                            Some(want) if st[2] == want => {
                                println!("     verified: device state shows effect 0x{:02X}.", st[2]);
                            }
                            Some(want) => println!(
                                "     ? state shows effect 0x{:02X} but we wrote 0x{want:02X} — check region/mode.",
                                st[2]
                            ),
                            None => {}
                        }
                    }
                }
                lighting::Protocol::Legacy => {
                    println!(
                        "     ACK-confirmed (no read-back: this board exposes no reliable lighting getter)."
                    );
                }
            }
        }
        println!();
    }
    if !apply {
        println!(
            "(nothing sent — add --apply --pid <pid> to fire one gated NOSTORE write when ready.)"
        );
    }
    Ok(())
}

/// Read-only: snapshot a device's full getter space (all 91-byte interfaces).
fn snapshot_device(
    vid: u16,
    pid: u16,
    name: String,
    infos: &[transport::HidDeviceInfo],
    now: u64,
) -> backup::Snapshot {
    let tid = 0x1Fu8;
    let mut ifaces = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for info in infos
        .iter()
        .filter(|i| i.vid == vid && i.pid == pid && i.feature_len == neuron::synth::RAZER_FEATURE_LEN)
    {
        if !seen.insert((info.usage_page, info.usage)) {
            continue;
        }
        let Ok(t) = transport::open_path(&info.path) else {
            continue;
        };
        let mut getters = Vec::new();
        for c in 0x00u8..=0x0F {
            for id in 0x80u8..=0x8F {
                if let Some(a) = discover::exec(&*t, tid, c, id, 0x20, &[]) {
                    let kind = discover::classify(&a);
                    if kind != "empty" {
                        getters.push(backup::GetterSnap {
                            class: c,
                            id,
                            kind: kind.into(),
                            raw: backup::hex80(&a),
                        });
                    }
                }
            }
        }
        ifaces.push(backup::IfaceSnap {
            usage_page: info.usage_page,
            usage: info.usage,
            getters,
        });
    }
    backup::Snapshot {
        vid,
        pid,
        name,
        unix_time: now,
        interfaces: ifaces,
    }
}

fn backup_cmd(reg: &Registry, pid_filter: Option<&str>) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let want = match pid_filter {
        Some(s) => Some(parse_hex16(s)?),
        None => None,
    };
    let infos = transport::enumerate()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let mut seen = std::collections::BTreeSet::new();
    let mut targets: Vec<(u16, u16, String)> = Vec::new();
    for i in &infos {
        if i.vid != neuron::synth::RAZER_VID || i.feature_len != neuron::synth::RAZER_FEATURE_LEN {
            continue;
        }
        if want.is_some_and(|w| w != i.pid) {
            continue;
        }
        if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
            if seen.insert(i.pid) {
                targets.push((i.vid, i.pid, def.name.clone()));
            }
        }
    }
    if targets.is_empty() {
        bail!("no recognized razer_report devices found to back up");
    }
    let backups_dir = neuron::runroot::run_root().join("backups");
    std::fs::create_dir_all(&backups_dir)?;
    for (vid, pid, name) in targets {
        let snap = snapshot_device(vid, pid, name, &infos, now);
        let path = backups_dir.join(snap.filename());
        std::fs::write(&path, snap.to_json())?;
        println!(
            "backed up {} (pid {pid:04x}): {} getters -> {}",
            snap.name,
            snap.getter_count(),
            path.display()
        );
    }
    println!("\n(read-only snapshot — restore/verify uses this as the known-good reference for gated writes)");
    Ok(())
}

/// Set the device control mode (the switch Synapse flips to take host control). Reversible.
/// Standard `razer_report`: class 0x00 / id 0x04, args = [mode, 0x00]; driver=0x03, hardware=0x00.
fn mode_cmd(reg: &Registry, mode_str: &str, pid_str: &str) -> Result<()> {
    let mode: u8 = parse_device_mode(mode_str)?;
    let pid = parse_hex16(pid_str)?;
    let def = reg
        .find_by_pid(neuron::synth::RAZER_VID, pid)
        .ok_or_else(|| anyhow::anyhow!("no registry device for pid {pid:04x}"))?
        .clone();
    let d = Device::open(def, pid)?;
    println!("setting pid {pid:04x} -> {mode_str} mode  (class=00 id=04 args=[{mode:02X} 00])");
    // EXEMPT from the visitor-restore discipline: this verb's ENTIRE PURPOSE is to leave the device in
    // the mode the user asked for. Restoring a "prior" here would undo the command — the opposite of
    // every other write path (which only flips to driver as a transient means to an end).
    writes::set_device_mode(&d, mode)?;
    println!("  device ACKed.");
    match d.run("device_mode") {
        Ok(a) => println!("  device_mode now reads: 0x{:02X}", a[0]),
        Err(_) => println!("  (device_mode getter didn't respond — expected in some states)"),
    }
    Ok(())
}

/// DEVICE-SIDE thumb-button remap (Razer 15/02). Resolves the target Razer mouse by DPI capability,
/// confirms it speaks the class-0x15 button-map protocol, then writes the reassignment so the button
/// emits the new key at the source. Volatile — see the printed note.
fn remap_cmd(
    reg: &Registry,
    key: Option<&str>,
    button: Option<&str>,
    to: Option<&str>,
    reset: bool,
) -> Result<()> {
    let d = open_with_command(reg, "dpi")
        .context("no DPI-capable Razer mouse found for a device-side remap")?;
    if !writes::supports_button_remap(&d) {
        bail!(
            "{} does not speak the class-0x15 button-map protocol — device-side remap unsupported \
             on this device",
            d.def.name
        );
    }
    // REFUSE contradictory invocations instead of silently picking one. This verb rewrites what a
    // physical button emits, so resolving an ambiguous request could remap the WRONG control and
    // still print success — the one outcome a device-write path must never produce.
    if reset && (key.is_some() || button.is_some() || to.is_some()) {
        bail!(
            "--reset restores the WHOLE thumb grid, so it can't be combined with \
             --key/--button/--to; run the reset on its own, or drop --reset to remap one button"
        );
    }
    if reset {
        writes::reset_thumb_buttons(&d)?;
        println!("thumb grid restored to stock: 1 2 3 4 5 6 7 8 9 0 - =");
        return Ok(());
    }
    if key.is_some() && button.is_some() {
        bail!("--key and --button both name the button to remap — pass exactly one, not both");
    }
    let button_id = match (key, button) {
        (Some(k), _) => {
            let usage = neuron::action::hid_usage_for_key(k)
                .ok_or_else(|| anyhow::anyhow!("'{k}' is not a key I can resolve to a HID usage"))?;
            writes::thumb_button_id_for_usage(usage).ok_or_else(|| {
                anyhow::anyhow!(
                    "'{k}' is not one of the stock thumb keys (1..9 0 - =); use --button <hex> \
                     to target a raw button id"
                )
            })?
        }
        (None, Some(b)) => u8::from_str_radix(b.trim_start_matches("0x"), 16)
            .context("--button must be a hex byte, e.g. 4b")?,
        (None, None) => bail!(
            "say what to remap: --key <thumb key> or --button <hex>, plus --to <key> (or --reset)"
        ),
    };
    let to = to.ok_or_else(|| anyhow::anyhow!("--to <key> is required (e.g. --to g)"))?;
    let usage = neuron::action::hid_usage_for_key(to).ok_or_else(|| {
        anyhow::anyhow!(
            "target '{to}' can't be expressed as a single device key (chords / media / numpad \
             aren't supported device-side)"
        )
    })?;
    writes::set_mouse_button_key(&d, button_id, usage)?;
    println!(
        "remapped button 0x{button_id:02X} -> emits '{to}' (HID usage 0x{usage:02X}) at the source.\n\
         NOTE: volatile — it holds while a host keeps the mouse in driver mode (the neuron app does); \
         the mouse reverts to its onboard profile otherwise. `neuron remap --reset` restores stock."
    );
    Ok(())
}

fn verify_cmd(file: &str) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let json = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let snap = backup::Snapshot::from_json(&json)
        .ok_or_else(|| anyhow::anyhow!("{file} is not a valid neuron backup"))?;
    let infos = transport::enumerate()?;
    if !infos
        .iter()
        .any(|i| i.vid == snap.vid && i.pid == snap.pid && i.feature_len == neuron::synth::RAZER_FEATURE_LEN)
    {
        bail!(
            "device pid {:04x} ({}) is not connected — can't verify",
            snap.pid,
            snap.name
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let current = snapshot_device(snap.vid, snap.pid, snap.name.clone(), &infos, now);
    let changed = snap.diff(&current);
    println!(
        "verify {} (pid {:04x}) vs {file}  [{} getters]",
        snap.name,
        snap.pid,
        snap.getter_count()
    );
    if changed.is_empty() {
        println!("  OK — no drift, device matches the backup.");
    } else {
        for c in &changed {
            println!("  ~ {:02X}/{:02X} changed", c.class, c.id);
            println!(
                "      backup: {}",
                c.before
                    .split_whitespace()
                    .take(16)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!(
                "      now:    {}",
                c.after
                    .unwrap_or("<gone>")
                    .split_whitespace()
                    .take(16)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        println!("  {} getter(s) differ from the backup.", changed.len());
    }
    Ok(())
}

fn import_cmd(deep: bool) {
    let roots = neuron::synapse::locate_roots();
    if roots.is_empty() {
        println!("No Razer config root found (looked for a 'Razer' folder under the standard data dirs).");
        return;
    }
    println!("Synapse config roots (version-agnostic — vendor root only):");
    for r in &roots {
        println!("  {}", r.display());
    }
    let found = neuron::synapse::harvest(&roots, 2500, 16384);
    println!("\nclassified {} config file(s) by content:", found.len());
    let mut by_kind: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for f in &found {
        for k in &f.kinds {
            *by_kind.entry(k.label()).or_default() += 1;
        }
        if f.kinds.is_empty() {
            *by_kind.entry("(unrecognized)").or_default() += 1;
        }
    }
    for (k, n) in &by_kind {
        println!("  {k:<14} {n}");
    }
    if f_skipped(&found) > 0 {
        println!(
            "  (encrypted/account-synced profiles are skipped — Razer's lock-in, not readable)"
        );
    }

    if deep {
        println!("\nsample extracted values (the eatable surface):");
        let mut shown = 0;
        for f in &found {
            let vals = neuron::synapse::extract(f);
            if vals.is_empty() {
                continue;
            }
            println!(
                "  {}",
                f.path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
            );
            for (k, v) in vals.iter().take(8) {
                println!("      {k} = {v}");
            }
            shown += 1;
            if shown >= 12 {
                println!("  ... (more)");
                break;
            }
        }
    } else {
        println!("\n(run `neuron import --deep` to extract sample values. Writing into Neuron config = next.)");
    }
}

fn f_skipped(found: &[neuron::synapse::Found]) -> usize {
    found.iter().filter(|f| f.encrypted).count()
}

/// `neuron import-export <file.synapse3|.ChromaEffects> [--apply]` — the clean, plaintext
/// migration path: unzip a Synapse EXPORT, parse its XML, normalize into a Neuron Profile + spine
/// rules, and (with --apply) write them to disk. Without --apply it's a dry preview.
fn import_export_cmd(file: &str, apply: bool) -> Result<()> {
    use std::path::Path;
    let mut imported = neuron::import::import_export(Path::new(file))
        .with_context(|| format!("importing {file}"))?;
    // Resolve a blank/whitespace name to "imported" up front so the preview shows the real
    // landing name a raw blank would otherwise hide (`profiles/.toml`). Filesystem DE-COLLISION is
    // deliberately deferred to the --apply branch below: a dry preview must not invent a "(2)"
    // suffix — or fail outright — based on destination state when it writes nothing.
    imported.profile.name =
        neuron::profile::Profile::resolve_import_name(&imported.profile.name);

    println!("Imported '{}':", imported.profile.name);
    println!("  profile : {}", imported.profile.summary());
    println!("  rules   : {}", imported.rules.len());
    for r in imported.rules.iter().take(24) {
        println!("      {}", r.summary());
    }
    if imported.rules.len() > 24 {
        println!("      ... ({} more)", imported.rules.len() - 24);
    }
    if !imported.notes.is_empty() {
        println!("  notes   :");
        for n in &imported.notes {
            println!("      - {n}");
        }
    }

    if !apply {
        println!(
            "\n(preview only — re-run with --apply to write profiles/{}.toml + its rules sidecar)",
            imported.profile.name
        );
        return Ok(());
    }

    // De-collide against profiles already on disk ONLY now that we're actually writing — the SAME
    // resolution the GUI wizard applies (`Profile::de_collide_import_name`), so `--apply` can never
    // silently clobber an existing profile (or orphan rules sidecar) whose display name merely
    // differs from this one but sanitizes to the same file key.
    imported.profile.name = neuron::profile::Profile::de_collide_import_name(&imported.profile.name)
        .map_err(|e| anyhow::anyhow!(e))?;

    // Write the profile (the settings bundle) and a rules sidecar (the spine Rule set the
    // run-daemon loads). Rules are the engine's serde `Rule`, so they get their own
    // `<name>.rules.toml` next to the profile.
    if imported.profile.is_empty() {
        println!("\n(profile carries no settable fields — skipping profiles/*.toml write)");
    } else {
        // The lighting stack rides in the profile itself now (the `lighting` layer list), so a plain
        // save() persists everything in one TOML write — no frame sidecar to reconcile.
        imported
            .profile
            .save()
            .map_err(|e| anyhow::anyhow!("saving imported profile: {e}"))?;
        println!(
            "\nwrote {}",
            neuron::profile::Profile::path(&imported.profile.name).display()
        );
    }
    if !imported.rules.is_empty() {
        let path = rules_sidecar_path(&imported.profile.name);
        let doc = RuleDoc {
            rules: imported.rules.clone(),
        };
        std::fs::create_dir_all(neuron::profile::profiles_dir()).ok();
        neuron::salvage::atomic_write(&path, toml::to_string_pretty(&doc)?.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        println!(
            "wrote {} ({} spine rule(s))",
            path.display(),
            imported.rules.len()
        );
    }
    Ok(())
}

use neuron::engine::RuleDoc;

fn rules_sidecar_path(name: &str) -> std::path::PathBuf {
    // Same canonical derivation as the profile file — never the raw name (a separator would aim it
    // at a nonexistent nested dir; see `Profile::rules_path`).
    neuron::profile::Profile::rules_path(name)
}

// ───────────────────────────────────────── macros ────────────────────────────────────────────

/// `neuron macro <add|run|list|check|prelude>` — drive the Python Macro Host.
fn macro_cmd(action: MacroCmd) -> Result<()> {
    use neuron::macros;
    use neuron::macros::macro_host;
    match action {
        MacroCmd::Prelude => {
            // print the host-module reference straight from the BUNDLED neuron.py source (embedded
            // in the binary — always matches the interpreter this build ships, no disk lookup).
            print!("{}", neuron::macros::pyruntime::host_module_source());
        }
        MacroCmd::List => {
            let dir = macro_host::macros_dir();
            println!("python macros (in {}):", dir.display());
            let names = macro_host::list_macros();
            if names.is_empty() {
                println!("  (none yet — `neuron macro add <name> <file.py>`)");
            }
            for n in names {
                println!("  {n}");
            }
            if !macro_host().available() {
                println!(
                    "note: no python runtime resolved — macros are stored but cannot run here."
                );
            }
        }
        MacroCmd::Add { name, file } => {
            let src = std::fs::read_to_string(&file)
                .with_context(|| format!("reading macro source {file}"))?;
            match macro_host().register(&name, &src) {
                Ok(()) => println!("registered macro '{name}' (warm + ready)"),
                Err(e) => bail!("register '{name}': {e}"),
            }
        }
        MacroCmd::Run { name, file, arm } => {
            // Arm the helper input layer for this run if requested (raw ctypes is unaffected either way).
            neuron::action::arm_input(arm);
            macro_host().set_armed(arm);
            let (id, src) = match (name, file) {
                (_, Some(f)) => {
                    let src =
                        std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?;
                    let id = std::path::Path::new(&f)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("adhoc")
                        .to_string();
                    (id, Some(src))
                }
                (Some(n), None) => (n, None),
                (None, None) => bail!("pass a macro --name or a --file to run"),
            };
            // Service BEACONS on the terminal: a macro's neuron.ask() becomes a y/n prompt here
            // (the GUI presents the same event as the binary radial). Daemon-style thread — it
            // blocks on stdin between prompts and dies with the process.
            let beacons = macro_host().beacon_events();
            std::thread::spawn(move || {
                use neuron::macros::BeaconEvent;
                while let Ok(ev) = beacons.recv() {
                    match ev {
                        BeaconEvent::Ask {
                            pid,
                            macro_id,
                            text,
                            ..
                        } => {
                            eprintln!("[beacon] {macro_id} asks: {text}  [y/n, enter = dismiss]");
                            let mut line = String::new();
                            let ans = match std::io::stdin().read_line(&mut line) {
                                // tolerate the BOM PowerShell prepends to piped stdin (U+FEFF is
                                // not whitespace, so a plain trim leaves it and "y" stops matching)
                                Ok(_) => match line
                                    .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                                    .to_ascii_lowercase()
                                    .as_str()
                                {
                                    // answer() now takes the chosen OPTION INDEX; the default ask is
                                    // ["yes","no"] (beacon: west=yes=0, east=no=1), so y→0, n→1.
                                    "y" | "yes" => Some(0_usize),
                                    "n" | "no" => Some(1_usize),
                                    _ => None,
                                },
                                Err(_) => None,
                            };
                            macro_host().answer(pid, ans);
                        }
                        BeaconEvent::Notify { macro_id, text } => eprintln!("[{macro_id}] {text}"),
                        BeaconEvent::Retire { .. } | BeaconEvent::RetireDomain { .. } => {}
                    }
                }
            });
            let ctx = macros::Context::capture();
            // A file is an ad-hoc candidate, not an implicit `macro add`: run its exact source
            // without writing macros/scripts or replacing a registered macro of the same stem.
            let result = match src.as_deref() {
                Some(source) => macro_host().invoke_source_with_budget(
                    &id,
                    source,
                    &ctx,
                    std::time::Duration::from_mins(10),
                ),
                None => macro_host().invoke_with_budget(
                    &id,
                    &ctx,
                    std::time::Duration::from_mins(10),
                ),
            };
            println!("{result}");
            // surface any macro print()/traceback the sidecar logged.
            for line in macro_host().drain_log() {
                println!("  | {line}");
            }
        }
        MacroCmd::Check { file } => {
            let src = std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?;
            match macro_host().check(&src) {
                Ok(defs) => {
                    println!(
                        "ok — defines: {}",
                        if defs.is_empty() {
                            "(none)".into()
                        } else {
                            defs.join(", ")
                        }
                    );
                    if !defs.iter().any(|d| d == "macro" || d == "main") {
                        println!("warning: no `def macro(ctx):` (or `def main(ctx):`) entry point");
                    }
                }
                Err(e) => bail!("check: {e}"),
            }
        }
    }
    Ok(())
}

fn parse_hex16(s: &str) -> Result<u16> {
    u16::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|_| anyhow::anyhow!("invalid hex '{s}'"))
}

/// A parsed `--mute on|off|toggle` request — the pure decode shared by `audio mic` and `audio out`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(windows, target_os = "linux", test))]
enum MuteAction {
    On,
    Off,
    Toggle,
}

/// Parse a `--mute` value string into a [`MuteAction`]. Accepts the friendly synonyms the CLI has
/// always taken (on/true/1/mute, off/false/0/unmute, toggle). Pure + case-insensitive, so the whole
/// accepted-vocabulary is unit-testable without touching Core Audio.
#[cfg(any(windows, target_os = "linux", test))]
fn parse_mute(s: &str) -> Result<MuteAction> {
    match s.to_lowercase().as_str() {
        "on" | "true" | "1" | "mute" => Ok(MuteAction::On),
        "off" | "false" | "0" | "unmute" => Ok(MuteAction::Off),
        "toggle" => Ok(MuteAction::Toggle),
        other => bail!("unknown mute value '{other}' (use on|off|toggle)"),
    }
}

/// Parse a `mode driver|hardware` string into the device-mode byte (driver=0x03, hardware=0x00),
/// accepting the synonyms the CLI takes. Pure, so the mode mapping is unit-testable.
fn parse_device_mode(s: &str) -> Result<u8> {
    match s.to_lowercase().as_str() {
        "driver" | "host" | "on" => Ok(0x03),
        "hardware" | "onboard" | "normal" | "off" => Ok(0x00),
        other => bail!("mode must be 'driver' or 'hardware' (got '{other}')"),
    }
}

/// Parse an explicit `probe <class> <id>` target. Both hex; the id MUST be a getter (>= 0x80) —
/// `probe` is strictly read-only, so a setter-range id is rejected. `None`/missing class+id means
/// "sweep" (returns `Ok(None)`). Pure (no I/O) so the read-only guard is unit-testable.
fn parse_probe_target(class: Option<&str>, id: Option<&str>) -> Result<Option<(u8, u8)>> {
    match (class, id) {
        (Some(c), Some(i)) => {
            let c = parse_hex16(c)? as u8;
            let i = parse_hex16(i)? as u8;
            if i < 0x80 {
                bail!("id 0x{i:02X} is in the setter range (<0x80); probe is read-only");
            }
            Ok(Some((c, i)))
        }
        _ => Ok(None),
    }
}

// The DPI stage-table decode (`decode_dpi_stages` / `decode_dpi_active`) moved to
// `neuron::writes` beside its encoder — the ONE inverse of `build_dpi_stages_payload`, imported at
// the top of this file. The tests below still exercise the CLI-visible behavior through those
// shared fns.

// ── "never fake success" input guards ─────────────────────────────────────────────────────────
// Pure validators run BEFORE any device write, so an out-of-range / no-op value is rejected with a
// clear error instead of being written (or silently clamped) and then reported as if it took. These
// are the inverse of Synapse's "looks applied" lie — neuron never claims a write it didn't make.

/// Hard DPI ceiling — the Focus Pro 30K's max; mirrors the cycle/intent clamp band (`100..30_000`).
const DPI_CEILING: u16 = 30_000;
/// A sane DPI floor — Razer sensors bottom out around 100.
const DPI_FLOOR: u16 = 100;

/// Reject a DPI the device can't honour. `capability::set_dpi` has no clamp, so writing e.g. 65000
/// would PERSIST garbage and THEN print MISMATCH; bail before the write instead.
fn validate_dpi(v: u16) -> Result<()> {
    if !(DPI_FLOOR..=DPI_CEILING).contains(&v) {
        bail!("DPI {v} is out of range ({DPI_FLOOR}..={DPI_CEILING}) — refusing to write a value the device can't honour");
    }
    Ok(())
}

/// Reject scroll stage 0 — wheel stages are 1-based, so 0 is a no-op the device ignores (we'd write
/// `[store, 0x00]` and falsely print "stage 0 active"). Caller passes the user's 1-based stage.
fn validate_scroll_stage(s: u8) -> Result<()> {
    if s == 0 {
        bail!("scroll stage is 1-based — stage 0 is not a real stage (try `neuron scroll 1`)");
    }
    Ok(())
}

/// Reject a DPI-stages `--active` outside the stage list. `writes` silently clamps it, so `--active 9`
/// on 2 stages would write stage 2 yet echo "active 9"; bail instead of clamping-and-lying. `active`
/// is the user's 1-based value; `count` is the number of stages supplied.
fn validate_dpi_stages_active(active: u8, count: usize) -> Result<()> {
    if active < 1 || (active as usize) > count {
        bail!(
            "--active {active} is out of range — there {} only {count} stage{} (use 1..={count})",
            if count == 1 { "is" } else { "are" },
            if count == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Apply a DPI stage LIST (the cycle) to the mouse via the verify-gated `set_dpi_stages` write.
/// With no values, just decode + print the current configured stages (read-only).
fn dpi_stages_cmd(reg: &Registry, stages: &[u16], active: u8, persist: bool) -> Result<()> {
    let d = open_with_command(reg, "set_dpi_stages")
        .or_else(|_| open_with_command(reg, "dpi_stages"))?;
    if stages.is_empty() {
        // read-only: show what the device currently cycles through.
        let s = d
            .run("dpi_stages_active")
            .or_else(|_| d.run("dpi_stages"))?;
        let cur = decode_dpi_stages(&s);
        let act = decode_dpi_active(&s).unwrap_or(0);
        if cur.is_empty() {
            println!("DPI stages: (none reported)");
        } else {
            let list: Vec<String> = cur
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    if i as u8 == act {
                        format!("[{v}]")
                    } else {
                        v.to_string()
                    }
                })
                .collect();
            println!(
                "DPI stages: {}  (active marked, {} stage(s))",
                list.join(" "),
                cur.len()
            );
        }
        return Ok(());
    }

    // active is 1-based on the CLI (matching Synapse's stage numbering); the write API is 0-based.
    // Reject an out-of-range active up front — writes::set_dpi_stages would silently clamp it and we'd
    // echo the raw (lying) value. Also reject any DPI the device can't honour.
    validate_dpi_stages_active(active, stages.len())?;
    for &v in stages {
        validate_dpi(v)?;
    }
    let active_idx = active.saturating_sub(1);
    let st: Vec<DpiStage> = stages.iter().map(|&v| DpiStage::symmetric(v)).collect();
    // DUAL-PLANE always (2026-07-23): volatile so the mouse acts right now, PLUS onboard so
    // hardware truth survives power-cycles — the volatile-only default is how the factory table
    // kept resurrecting. `--persist` is still accepted but is now the standing behaviour.
    let _ = persist;
    let list: Vec<String> = stages.iter().map(std::string::ToString::to_string).collect();
    println!(
        "setting DPI stages [{}] active {} (live + onboard)...",
        list.join("/"),
        active,
    );
    // VISITOR discipline: `set_dpi_stages` flips to driver mode INTERNALLY (no prior returned to us),
    // so read the mode BEFORE and restore it after by the same rule — a one-shot CLI never leaves the
    // driver lease held (the `dpi_trap` self-poisoning loop). `unwrap_or(0)` = treat an unanswered
    // getter as non-driver, matching `ensure_driver`'s own "flip when unsure".
    let prior = writes::device_mode(&d).unwrap_or(0);
    // Name WHICH plane landed if the pair breaks apart. A bare propagated error can't distinguish
    // "nothing was written" from "the live plane took it and onboard didn't", and those need
    // different reactions from the user — the second leaves the mouse acting correctly right now but
    // liable to revert on a power-cycle.
    let res = writes::set_dpi_stages(&d, &st, active_idx, cap::Store::Volatile, neuron::dpi_origin::Cause::UserApplied).and_then(|()| {
        writes::set_dpi_stages(&d, &st, active_idx, cap::Store::Persist, neuron::dpi_origin::Cause::UserApplied).map_err(|e| {
            anyhow::anyhow!(
                "the LIVE plane accepted the stage table but the ONBOARD write failed, so the two \
                 planes are now DIVERGED (the mouse behaves correctly now, but may revert to its \
                 old table on a power-cycle). Re-run to converge them: {e}"
            )
        })
    });
    restore_custody_if_visitor(&d, prior);
    res?;
    println!("  done — write verified against the device's stage-table read-back.");
    Ok(())
}

/// Sensor lift-off distance: read the current, or write a symmetric level / asymmetric split.
/// Every write read-back-verifies (bails on mismatch, never a false success) — the point here is
/// to HARDWARE-CONFIRM the asymmetric path on the Naga via the shared `0x0B/0x85` getter.
fn lod_cmd(reg: &Registry, lift: Option<u8>, landing: Option<u8>, sym: Option<u8>) -> Result<()> {
    // The DPI-capable mouse (the Naga); its sensor class rides the same control interface.
    let d = open_with_command(reg, "dpi")?;
    let show = |d: &Device| match writes::lift_off_async(d) {
        Some((lf, la)) => println!("  LOD now: ASYMMETRIC — lift {lf} / landing {la}"),
        None => println!(
            "  LOD now: symmetric level {}",
            writes::lift_off_distance(d).map_or_else(|_| "?".into(), |v| v.to_string())
        ),
    };
    println!("current lift-off state:");
    show(&d);
    match (sym, lift, landing) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            bail!("--sym sets a symmetric level; it can't be combined with --lift/--landing (an asymmetric split). Pass one or the other.");
        }
        (Some(level), None, None) => {
            if level > 2 {
                bail!("symmetric lift-off level is 0, 1, or 2 (low/med/high); got {level}");
            }
            // VISITOR discipline: restore the found custody on exit (the write verifies internally
            // BEFORE we hand the lease back, so the round-trip is unaffected).
            let prior = ensure_driver(&d);
            println!("setting SYMMETRIC lift-off level {level}...");
            let res = writes::set_lift_off_distance(&d, level);
            restore_custody_if_visitor(&d, prior);
            res?;
            println!("  ACCEPTED + read-back VERIFIED (0x0B/0x85 echoed mode=symmetric + level).");
            show(&d);
        }
        (None, Some(lf), Some(la)) => {
            if !(2..=26).contains(&lf) {
                bail!("--lift is 2..=26 (Focus Pro level); got {lf}");
            }
            if !(1..=25).contains(&la) {
                bail!("--landing is 1..=25 (Focus Pro level); got {la}");
            }
            // VISITOR discipline: restore the found custody on exit (verify happens inside the write).
            let prior = ensure_driver(&d);
            println!("setting ASYMMETRIC lift-off: lift {lf} / landing {la}...");
            // The write itself re-reads 0x0B/0x85 and bails unless the device echoes
            // mode=async + this exact lift/landing pair — so reaching the line below IS the
            // hardware round-trip.
            let res = writes::set_lift_off_asymmetric(&d, lf, la);
            restore_custody_if_visitor(&d, prior);
            res?;
            println!("  ACCEPTED + read-back VERIFIED on the shared 0x0B/0x85 getter — hardware round-trip.");
            show(&d);
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            bail!("an asymmetric split needs BOTH --lift and --landing (e.g. `neuron lod --lift 12 --landing 3`)");
        }
        (None, None, None) => {
            println!("(pass --lift N --landing M to set a split, or --sym 0|1|2 to set/restore symmetric)");
        }
    }
    Ok(())
}

/// Select the active `HyperScroll` wheel stage via the wire-confirmed `set_scroll_stage` write
/// (class 0x15/0x00). With no value, explains there's no read getter for the active stage.
fn scroll_cmd(reg: &Registry, stage: Option<u8>, volatile: bool) -> Result<()> {
    let d = open_with_command(reg, "set_scroll_stage")?;
    match stage {
        None => {
            println!(
                "scroll stage select: pass a 1-based stage to set it (e.g. `neuron scroll 2`)."
            );
            println!("  (the device exposes no getter for the active scroll stage — set-only.)");
        }
        Some(s) => {
            // stages are 1-based; reject 0 before writing a no-op the device silently ignores.
            validate_scroll_stage(s)?;
            // Synapse sends store=persist for this command; default to that, --volatile opts out.
            let store = if volatile {
                cap::Store::Volatile
            } else {
                cap::Store::Persist
            };
            println!(
                "selecting scroll stage {s} ({})...",
                if volatile {
                    "volatile"
                } else {
                    "persist/onboard"
                }
            );
            // VISITOR discipline: `set_scroll_stage` flips to driver mode INTERNALLY, so read the mode
            // before and restore after by the same rule (see restore_custody_if_visitor).
            let prior = writes::device_mode(&d).unwrap_or(0);
            let res = writes::set_scroll_stage(&d, s, store);
            restore_custody_if_visitor(&d, prior);
            res?;
            println!("  done — scroll stage {s} active.");
        }
    }
    Ok(())
}

/// Read-only `razer_report` getter probe. Opens the device by PID (its 91-byte feature pipe)
/// and fires getter commands, dumping raw replies — the transparent way to see exactly what
/// a device exposes (and what it does NOT, e.g. onboard storage).
fn probe_cmd(pid: &str, class: Option<&str>, id: Option<&str>, scan: bool) -> Result<()> {
    let pid = parse_hex16(pid)?;
    let tid = 0x1Fu8;
    // A composite device exposes several 91-byte collections; a capability may live on only
    // one. Probe every distinct control interface, not just the first.
    let mut ifaces: Vec<_> = transport::enumerate()?
        .into_iter()
        .filter(|i| i.vid == neuron::synth::RAZER_VID && i.pid == pid && i.feature_len == neuron::synth::RAZER_FEATURE_LEN)
        .collect();
    ifaces.sort_by_key(|i| (i.usage_page, i.usage));
    ifaces.dedup_by_key(|i| (i.usage_page, i.usage));
    if ifaces.is_empty() {
        bail!("no razer_report (91-byte) interface for PID {pid:04x} — connected?");
    }
    println!(
        "PID {pid:04x}: {} razer_report interface(s), read-only getters\n",
        ifaces.len()
    );

    let dump = |up: u16, us: u16, class: u8, id: u8, a: &[u8; 80]| {
        let mut hex = String::new();
        for b in &a[..24] {
            let _ = write!(hex, "{b:02X} ");
        }
        println!(
            "  [{up:04x}/{us:04x}] {:02X}/{:02X} {:<16} {:<13} {hex}",
            class,
            id,
            discover::class_hint(class),
            discover::classify(a)
        );
    };

    let explicit = parse_probe_target(class, id)?;
    let _ = scan; // scan and no-class both mean "sweep"; explicit class/id means "one getter"

    let mut any = false;
    for info in &ifaces {
        let t = match transport::open_path(&info.path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        match explicit {
            Some((c, i)) => {
                if let Some(a) = discover::exec(&*t, tid, c, i, 0x20, &[]) {
                    if discover::classify(&a) != "empty" {
                        dump(info.usage_page, info.usage, c, i, &a);
                        any = true;
                    }
                }
            }
            None => {
                for c in 0x00u8..=0x0F {
                    for i in 0x80u8..=0x8F {
                        if let Some(a) = discover::exec(&*t, tid, c, i, 0x20, &[]) {
                            if discover::classify(&a) != "empty" {
                                dump(info.usage_page, info.usage, c, i, &a);
                                any = true;
                            }
                        }
                    }
                }
            }
        }
    }
    if !any {
        match explicit {
            Some((c, i)) => {
                println!("  {c:02X}/{i:02X} not supported on any interface of this device");
            }
            None => println!("  (no getters responded)"),
        }
    }
    Ok(())
}

fn bind_cmd(action: BindCmd) -> Result<()> {
    match action {
        BindCmd::List => {
            let b = Bindings::load();
            let custom = Bindings::path().exists();
            println!(
                "bindings ({}):",
                if custom {
                    "from bindings.toml"
                } else {
                    "built-in defaults"
                }
            );
            if b.bindings.is_empty() {
                println!("  (none)");
            }
            for bind in &b.bindings {
                if !bind.desc.is_empty() {
                    println!("  # {}", bind.desc);
                }
                println!("  {}", bind.summary());
            }
        }
        BindCmd::Init { force } => {
            if Bindings::path().exists() && !force {
                bail!("bindings.toml already exists (use --force to overwrite)");
            }
            neuron::salvage::atomic_write(&Bindings::path(), neuron::bindings::TEMPLATE_TOML.as_bytes())?;
            println!(
                "wrote {} — edit it to customize.",
                Bindings::path().display()
            );
        }
    }
    Ok(())
}

/// Print the pump's measurement-harness counters (`neuron::prof::pump`): wake counts by reason,
/// the wake -> first-edge latency histogram, and the tick-starvation watchdog's event count.
/// Read-only, in-process — see `Cmd::Prof`'s doc for what that means for a bare invocation vs.
/// letting `run_daemon` print its own session's snapshot on exit.
fn prof_pump_cmd() {
    let s = neuron::prof::pump::snapshot();
    println!("pump wakes: input={}  tick_only={}  total={}", s.wake_input, s.wake_tick_only, s.wake_total);
    println!("tick-starvation events: {}", s.starvation_events);
    println!("wake -> first-edge latency (us):");
    let mut any = false;
    for (bound_us, count) in s.latency_buckets_us {
        if count > 0 {
            any = true;
            println!("  <= {bound_us:>8} us : {count}");
        }
    }
    if !any {
        println!("  (no samples yet)");
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn run_daemon(reg: &Registry, seconds: Option<u64>, safe: bool) {
    // ARM real input synthesis for live use. This is the one place the CLI daemon flips the
    // process-wide safety gate ON so bound key/click/macro actions actually fire. `--safe` keeps
    // it DISARMED: the Engine still resolves every trigger and prints what it WOULD do, but
    // SendInput stays a no-op. Tests never reach this path, so they remain disarmed.

    if safe {
        neuron::action::arm_input(false);
        // SAFE MODE also freezes device writes (DPI/scroll/profile intents) so an observe-only run
        // touches neither the host (SendInput) nor the device — a fully read-only dry run.
        neuron::writes::set_writes_paused(true);
        println!("Neuron daemon — SAFE MODE: input DISARMED + device writes PAUSED (observe + dry-run only).");
    } else {
        let available = neuron::controls::live_input_available();
        neuron::action::arm_input(available);
        neuron::writes::set_writes_paused(false);
        if !available { println!("live input unavailable: check /dev/input and /dev/uinput access"); }
    }

    // Restore the remembered profile cursor FIRST — before the spine is built. A profile's
    // `<name>.rules.toml` binds are in scope only while that profile is active, so a daemon that
    // started with an empty cursor would fold in none of them and silently run without the binds
    // the GUI shows as live. Same file the GUI reads, so the two agree. Read-only: no device write
    // happens because a daemon started.
    let restored = neuron::profile::restore_active_once();
    if !restored.is_empty() {
        println!("Neuron daemon · profile '{restored}' (remembered; settings not re-applied)");
    }

    // Live-wire the unified spine: bindings.toml, cast.toml (radial + glyph), and the ACTIVE
    // profile's rules sidecar fold into ONE Engine. Auto-switch routing (apps.toml) rides beside it
    // as a table, not as rules — see controls::build_runtime_from. The event loop translates each
    // device event into a Trigger and dispatches through the Engine (Engine::fire).
    let rt = neuron::controls::build_runtime();
    println!(
        "Neuron daemon — {} spine rule(s) across base + {} HyperShift layer(s):",
        rt.rule_count(),
        rt.engine.layers.len()
    );
    for rule in rt.engine.to_rules() {
        println!("  {}", rule.summary());
    }
    match seconds {
        Some(s) => println!("\nListening for {s}s (ESC to stop) — buttons / mic-tap / app-switch / HyperShift, all through the Engine:\n"),
        None => println!("\nListening until ESC — buttons / mic-tap / app-switch / HyperShift, all through the Engine:\n"),
    }
    run_listen(reg, seconds, rt);
    println!("\nstopped.");
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run_daemon(_reg: &Registry, _seconds: Option<u64>, _safe: bool) {
    println!(
        "the remap daemon is Windows-only for now: control events and action dispatch have no \
         backend on this platform, so it would listen forever and fire nothing."
    );
    println!("device control works here — try `neuron list`, `dpi`, `lighting`, `profile`, `remap`.");
}

/// Carry out a daemon [`Intent`] — the typed work an `Action` delegates because the stateless
/// Action layer has no device handle / profile store / stage cursor. The Engine reports the
/// intent (a clean dry-run line); HERE is where it becomes real device/profile state.
#[cfg(any(windows, target_os = "linux"))]
fn run_intent(devices: &mut DeviceSession<'_>, intent: &neuron::action::Intent) -> String {
    let mut cursor = CliProfileCursor;
    neuron::intent::run_shared_intent(
        devices,
        &mut cursor,
        intent,
        neuron::dpi_origin::Cause::UserApplied,
    )
        .unwrap_or_else(|| "app intent needs the resident app - run neuron-app".into())
}

#[cfg(any(windows, target_os = "linux"))]
struct CliProfileCursor;

#[cfg(any(windows, target_os = "linux"))]
impl neuron::intent::ProfileCursor for CliProfileCursor {
    fn active_profile(&self) -> String {
        daemon_active_profile()
    }

    fn set_active_profile(&mut self, name: &str) {
        set_daemon_active_profile(name);
    }
}
/// The daemon's current active-profile cursor (for `ProfileCycle`). Backed by the SAME process-global
/// `profile::active()` cell the GUI and every other apply path use — so a button-cycle and a manual
/// `profile apply` never disagree about "what's applied now". (This used to be a thread-local that
/// diverged from the global, so a cycle could step from a stale cursor.)
#[cfg(any(windows, target_os = "linux"))]
fn daemon_active_profile() -> String {
    neuron::profile::active()
}
#[cfg(any(windows, target_os = "linux"))]
fn set_daemon_active_profile(name: &str) {
    neuron::profile::set_active(name);
}

/// Fire one [`Trigger`] through the unified [`Engine`] and carry out the result: run every matching
/// rule's host action, route any daemon [`Intent`] (DPI / scroll / profile) to real device/profile
/// state, and drive a held-down [`Action::Turbo`] autofire while the trigger stays down. This is
/// the single dispatch entry the run-daemon uses for buttons, the mic tap, gestures, radial flicks
/// and app focus alike — the "one dispatcher" made live.
#[cfg(any(windows, target_os = "linux"))]
struct CliIntentRunner<'a, 'reg> {
    devices: &'a mut DeviceSession<'reg>,
}

#[cfg(any(windows, target_os = "linux"))]
impl IntentRunner for CliIntentRunner<'_, '_> {
    fn run_intent(&mut self, intent: &neuron::action::Intent) -> String {
        run_intent(self.devices, intent)
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn fire_trigger(
    devices: &mut DeviceSession<'_>,
    exec: &mut DispatchExecutor,
    rt: &mut neuron::controls::Runtime,
    trigger: &neuron::engine::Trigger,
) -> Option<DispatchOutcome> {
    #[cfg(target_os = "linux")]
    if let neuron::engine::Trigger::Input { page, usage, pid: Some(pid) } = trigger {
        if neuron::intercept::owns(*page, *usage, pid.get()) { return None; }
    }
    let mut intents = CliIntentRunner { devices };
    let outcome = exec.fire(&rt.engine, trigger, &mut intents)?;
    rt.note_fired(trigger);
    println!("  {}: {}", outcome.trigger, outcome.action);
    Some(outcome)
}

/// Run the event loop, dispatching EVERY input through the one [`Engine`]. On Windows we also poll
/// the mic's capture-endpoint mute state every ~50 ms (the Seiren's tap-to-mute reflects into Core
/// Audio, so a toggle = a physical tap -> a [`Trigger::MicTap`]) and the foreground app (a focus
/// change -> a [`Trigger::AppFocus`]). Backward compatible: the same bindings/cast/app configs,
/// now executed through the spine instead of three separate dispatchers.
#[cfg(any(windows, target_os = "linux"))]
fn run_listen(reg: &Registry, seconds: Option<u64>, rt: neuron::controls::Runtime) {
    use neuron::controls::{HoldEdges, InputEdge, MIC_TAP};
    use neuron::engine::Trigger;
    use std::cell::RefCell;

    #[cfg(target_os = "linux")]
    neuron::intercept::configure_from_engine_with(&rt.engine, Some(rt.cast_trigger));

    let mic = audio::resolve_capture(None);
    let ctl = mic.as_ref().and_then(|e| audio::VolumeCtl::open(&e.id));
    if let Some(e) = &mic {
        if ctl.is_some() {
            println!("  (watching '{}' for taps via mute-toggle)\n", e.name);
        }
    }
    let mut last_mute = ctl.as_ref().map(neuron::audio::VolumeCtl::get_mute);
    let mut switcher = neuron::app_focus::AppFocusSwitch::new();
    let mut tick = 0u32;
    // The profile the spine was last assembled for. A profile carries its `<name>.rules.toml`
    // binds, in scope only while it is active, so ANY path that moves the cursor — auto-switch, a
    // bound ProfileSwitch key, a ProfileCycle — has to be followed by a rebuild. Watching the
    // cursor itself covers all of them, instead of a rebuild bolted onto each switch site (the GUI
    // does the equivalent through its reload command).
    let mut spine_profile = neuron::profile::active();

    // GamingMode suppression hook (Alt+Tab / Win / Alt+F4 / Alt+Esc). Installed HERE — on the thread that
    // pumps the Raw-Input message loop inside `controls::listen` — because a WH_KEYBOARD_LL hook only
    // fires while its installing thread pumps messages. Policy comes from the ONE shared carrier in
    // `neuron::hook` (set by `profile_apply` -> ProfileSwitch intent, and from a one-shot `profile
    // apply`), the SAME source the GUI runtime uses — no duplicate engine/policy build. The handle is
    // held for the daemon's lifetime so Drop uninstalls on exit (desktop returns to normal). If no
    // gaming-mode profile is ever applied the policy stays all-false and nothing is installed.
    let mut gaming_hook: Option<neuron::hook::Hook> = None;
    neuron::hook::reconcile(&mut gaming_hook);

    // The Engine needs `&mut` (HyperShift hold edges + intent state); both closures share it via a
    // RefCell — sound because `listen` runs them serially on one thread, never re-entrantly.
    let rt = RefCell::new(rt);
    let devices = RefCell::new(DeviceSession::new(reg));
    let exec = RefCell::new(DispatchExecutor::new());
    let turbos = RefCell::new(TurboRuntime::new());
    // Edge-detector: a Razer report is the SET of buttons currently down, so we DIFF successive
    // reports into per-button down/up edges. This fixes (a) multi-button chords (every newly-pressed
    // control dispatches, not just the first hit) and (b) precise HyperShift release (only the
    // input that actually went up releases ITS layer — no blanket release_all on any empty report).
    let edges = RefCell::new(HoldEdges::new());

    neuron::controls::listen(
        seconds,
        |ev| {
            for edge in edges.borrow_mut().edges(ev) {
                match edge {
                    InputEdge::Down(trigger) => {
                        // Hold any HyperShift layer THIS input activates (tracked per-input so its
                        // release drops only its own layer), then dispatch the input's action.
                        rt.borrow_mut().hold_for_input(&trigger);
                        if let Some(outcome) = fire_trigger(
                            &mut devices.borrow_mut(),
                            &mut exec.borrow_mut(),
                            &mut rt.borrow_mut(),
                            &trigger,
                        ) {
                            turbos.borrow_mut().start(outcome.turbo);
                        }
                    }
                    InputEdge::Up(trigger) => {
                        rt.borrow_mut().release_for_input(&trigger);
                        turbos.borrow_mut().release(&trigger);
                    }
                }
            }
        },
        || {
            tick += 1;
            // Re-reconcile the gaming-mode hook periodically (cheap; only (de)installs on a policy
            // change) so a profile applied mid-session — e.g. via the AppFocus->ProfileSwitch intent
            // — (de)activates Alt+Tab/Win/Alt+F4 suppression on this very (message-pumping) thread.
            if tick.is_multiple_of(20) {
                neuron::hook::reconcile(&mut gaming_hook);
            }
            // The active profile moved (any path) — reassemble the spine so the newly active
            // profile's binds are live and the previous one's are not. Held-layer state resets with
            // it, the same guarantee the GUI's reload makes: a config swap must not strand a held
            // layer. A string compare per tick; the rebuild only runs on an actual switch.
            {
                let now = neuron::profile::active();
                if now != spine_profile {
                    spine_profile = now;
                    *rt.borrow_mut() = neuron::controls::build_runtime();
                    #[cfg(target_os = "linux")]
                    {
                        let runtime = rt.borrow();
                        neuron::intercept::configure_from_engine_with(&runtime.engine, Some(runtime.cast_trigger));
                    }
                }
            }
            {
                let mut intents = CliIntentRunner {
                    devices: &mut devices.borrow_mut(),
                };
                turbos
                    .borrow_mut()
                    .tick(&mut exec.borrow_mut(), &mut intents);
            }
            if !tick.is_multiple_of(10) {
                return; // throttle the periodic polls to ~50 ms
            }
            // mic tap (Core Audio mute toggle) -> a MicTap trigger AND its raw Input usage, so a
            // binding written either way (Trigger::MicTap or Input 0xF000/0x01) still fires.
            if let (Some(c), Some(prev)) = (ctl.as_ref(), last_mute) {
                let now = c.get_mute();
                if now != prev {
                    last_mute = Some(now);
                    println!(
                        "  mic tap detected (mute -> {})",
                        if now { "ON" } else { "off" }
                    );
                    fire_trigger(
                        &mut devices.borrow_mut(),
                        &mut exec.borrow_mut(),
                        &mut rt.borrow_mut(),
                        &Trigger::MicTap,
                    );
                    let (p, u) = MIC_TAP;
                    // pid: None — the CLI detector watches the OS default-capture mute, so the
                    // device behind the edge is unknowable here (same reasoning as the app's
                    // dispatch poll): only pid-less rules match; device-pinned rules belong to the
                    // HID-edge path, which knows the true pid.
                    fire_trigger(
                        &mut devices.borrow_mut(),
                        &mut exec.borrow_mut(),
                        &mut rt.borrow_mut(),
                        &Trigger::Input {
                            page: p,
                            usage: u,
                            pid: None,
                        },
                    );
                }
            }
            // app-aware switch — the same two-part shape the GUI dispatcher uses:
            //   1. ROUTING (apps.toml) resolves through `AppRules::resolve`: first match wins, else
            //      the configured fallback. It is not a set of engine rules, because the executor
            //      fires EVERY matching rule and two overlapping needles would apply two profiles
            //      back-to-back with the last one winning.
            //   2. The AppFocus TRIGGER still fires, so a hand-authored `app focus 'x' -> …` bind
            //      keeps working. Those are real binds; routing isn't.
            if let Some(app) = switcher.poll() {
                // `switch_target` is the SHARED decision (first match, else fallback, and nothing
                // to do when it names the profile already active) — the same call the GUI
                // dispatcher makes, so the two front ends cannot drift on what to switch to.
                let target = rt
                    .borrow()
                    .app_rules
                    .switch_target(&app, &neuron::profile::active());
                if let Some(name) = target {
                    let line = run_intent(
                        &mut devices.borrow_mut(),
                        &neuron::action::Intent::ProfileSwitch(name),
                    );
                    println!("  {line}");
                    // Only rebuild when the switch actually landed — the cursor is the authority.
                    // A rule pointing at a deleted profile fails on every focus into that app, and
                    // reassembling the whole spine on each failure is churn for state that never
                    // changed.
                }
                fire_trigger(
                    &mut devices.borrow_mut(),
                    &mut exec.borrow_mut(),
                    &mut rt.borrow_mut(),
                    &Trigger::AppFocus { app },
                );
            }
        },
    );
    #[cfg(target_os = "linux")]
    neuron::intercept::deactivate();
    // measurement harness read-out: this session's pump counters (see `neuron prof pump`).
    prof_pump_cmd();
}

#[cfg(any(windows, target_os = "linux"))]
fn audio_cmd(action: AudioCmd) -> Result<()> {
    match action {
        AudioCmd::List => audio_list(),
        AudioCmd::Monitor { seconds } => audio_monitor(seconds),
        AudioCmd::Mic {
            device,
            gain,
            nudge,
            mute,
        } => audio_mic(device, gain, nudge, mute)?,
        AudioCmd::Out {
            device,
            vol,
            nudge,
            mute,
        } => audio_out(device, vol, nudge, mute)?,
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn audio_cmd(_action: AudioCmd) -> Result<()> {
    anyhow::bail!(
        "audio endpoints are Windows-only for now: neuron reads them through Core Audio and \
         has no PipeWire/ALSA backend yet, so it can see none of yours."
    );
}

/// Poll Razer audio endpoints for volume/mute changes. The `BlackShark` knob/mute and the
/// Seiren tap surface as Core Audio changes (UAC feature-unit), not Raw Input HID — this is
/// the channel a Core-Audio-based remap listens on.
#[cfg(any(windows, target_os = "linux"))]
fn audio_monitor(seconds: u64) {
    use std::time::{Duration, Instant};
    let mut watched: Vec<(String, audio::VolumeCtl, f32, bool)> = Vec::new();
    for flow in [audio::Flow::Capture, audio::Flow::Render] {
        for e in audio::endpoints(flow) {
            if let Some(ctl) = audio::VolumeCtl::open(&e.id) {
                let (v, m) = (ctl.get_volume(), ctl.get_mute());
                println!(
                    "  watching [{}] {}  (gain {}%, mute {})",
                    flow.label(),
                    e.name,
                    (v * 100.0).round() as i32,
                    if m { "ON" } else { "off" }
                );
                watched.push((e.name, ctl, v, m));
            }
        }
    }
    if watched.is_empty() {
        println!("no Razer audio endpoints found");
        return;
    }
    println!(
        "\nMonitoring {} endpoint(s) for {seconds}s — turn the knob / press mute / tap mic:\n",
        watched.len()
    );
    let start = Instant::now();
    while start.elapsed().as_secs() < seconds {
        for (name, ctl, v, m) in &mut watched {
            let nv = ctl.get_volume();
            let nm = ctl.get_mute();
            if (nv - *v).abs() > 0.001 {
                println!(
                    "  VOL   {name}  {:>3}% -> {:>3}%",
                    (*v * 100.0).round() as i32,
                    (nv * 100.0).round() as i32
                );
                *v = nv;
            }
            if nm != *m {
                println!(
                    "  MUTE  {name}  {} -> {}",
                    if *m { "ON" } else { "off" },
                    if nm { "ON" } else { "off" }
                );
                *m = nm;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    println!("\ndone.");
}

#[cfg(any(windows, target_os = "linux"))]
fn audio_list() {
    for flow in [audio::Flow::Capture, audio::Flow::Render] {
        let eps = audio::endpoints(flow);
        println!("== {} endpoints ==", flow.label());
        if eps.is_empty() {
            println!("  (none)");
        }
        for e in &eps {
            println!(
                "  {:>3}%{}  {}",
                (e.volume * 100.0).round() as i32,
                if e.muted { " [MUTED]" } else { "       " },
                e.name
            );
        }
        println!();
    }
}

/// Resolve the capture endpoint to act on (shared logic in `audio::resolve_capture`):
/// explicit needle, else the user's Razer/Seiren mic, else the first capture endpoint.
#[cfg(any(windows, target_os = "linux"))]
fn resolve_mic(device: Option<&str>) -> Result<neuron::audio::Endpoint> {
    audio::resolve_capture(device).ok_or_else(|| match device {
        Some(n) => anyhow::anyhow!("no capture device matching '{n}'"),
        None => anyhow::anyhow!("no capture (microphone) endpoints found"),
    })
}

#[cfg(any(windows, target_os = "linux"))]
fn audio_mic(
    device: Option<String>,
    gain: Option<f32>,
    nudge: Option<f32>,
    mute: Option<String>,
) -> Result<()> {
    let ep = resolve_mic(device.as_deref())?;
    let ctl = audio::VolumeCtl::open(&ep.id)
        .ok_or_else(|| anyhow::anyhow!("failed to open volume control for '{}'", ep.name))?;

    let mut acted = false;
    if let Some(g) = gain {
        ctl.set_volume(g / 100.0);
        acted = true;
    }
    if let Some(d) = nudge {
        ctl.nudge(d / 100.0);
        acted = true;
    }
    if let Some(m) = mute {
        match parse_mute(&m)? {
            MuteAction::On => {
                ctl.set_mute(true);
            }
            MuteAction::Off => {
                ctl.set_mute(false);
            }
            MuteAction::Toggle => {
                ctl.toggle_mute();
            }
        }
        acted = true;
    }

    let verb = if acted { "now" } else { "is" };
    println!(
        "{}\n  gain {verb} {:>3}%   mute {verb} {}",
        ep.name,
        (ctl.get_volume() * 100.0).round() as i32,
        if ctl.get_mute() { "ON" } else { "off" }
    );
    Ok(())
}

/// Show or control an OUTPUT endpoint (headphones / sound card / speakers) — the render mirror of
/// `audio_mic`, resolving generically by name substring or the system's active output.
#[cfg(any(windows, target_os = "linux"))]
fn audio_out(
    device: Option<String>,
    vol: Option<f32>,
    nudge: Option<f32>,
    mute: Option<String>,
) -> Result<()> {
    let ep = audio::resolve_render(device.as_deref())
        .ok_or_else(|| anyhow::anyhow!("no output endpoint found"))?;
    let ctl = audio::VolumeCtl::open(&ep.id)
        .ok_or_else(|| anyhow::anyhow!("failed to open volume control for '{}'", ep.name))?;

    let mut acted = false;
    if let Some(g) = vol {
        ctl.set_volume(g / 100.0);
        acted = true;
    }
    if let Some(d) = nudge {
        ctl.nudge(d / 100.0);
        acted = true;
    }
    if let Some(m) = mute {
        match parse_mute(&m)? {
            MuteAction::On => {
                ctl.set_mute(true);
            }
            MuteAction::Off => {
                ctl.set_mute(false);
            }
            MuteAction::Toggle => {
                ctl.toggle_mute();
            }
        }
        acted = true;
    }

    let verb = if acted { "now" } else { "is" };
    println!(
        "{}\n  vol {verb} {:>3}%   mute {verb} {}",
        ep.name,
        (ctl.get_volume() * 100.0).round() as i32,
        if ctl.get_mute() { "ON" } else { "off" }
    );
    Ok(())
}

/// Device writes only take in DRIVER mode (Razer gates host control behind it). Ensure it —
/// reversible, idempotent, no-op if already there. Delegates to the canonical
/// [`neuron::writes::ensure_driver`] (one driver-mode sequence, no CLI-local copy of the magic
/// bytes that could drift from core). RETURNS the PRIOR mode byte so a one-shot command can hand
/// custody back on the way out (see [`restore_custody_if_visitor`]) — a CLI is a VISITOR.
fn ensure_driver(d: &Device) -> u8 {
    neuron::writes::ensure_driver(d)
}

/// The device-mode byte for DRIVER (host-control) custody. Kept here only so the visitor-restore
/// can ask "was the app ALREADY holding the lease when I arrived?" — mirror of core's private const.
const DRIVER_MODE: u8 = 0x03;

/// Hand back the custody state a one-shot CLI command FOUND before it flipped the device into driver
/// mode. THE CLI IS A VISITOR, NOT A RESIDENT: a process that takes the driver lease and then exits
/// leaves the device in ABANDONED custody — onboard buttons go dead and the next wake restores stale
/// volatile state. That was the 2026-07-07 `dpi_trap` self-poisoning loop (trap round 3): every
/// "repair" verb (`dpi`, `brightness`, …) silently re-armed the very trap it was meant to fix, because
/// it grabbed the lease and never let go. So each write path captures the PRIOR mode and, on the way
/// out (success OR error), restores it. EXCEPTION: when the prior mode was ALREADY driver (0x03) some
/// resident holds the lease (the app) — a visitor must NOT yank it, so we leave it exactly as found.
/// (Callers that read the prior via [`writes::device_mode`] pass its `unwrap_or(0)` — an unanswered
/// getter reads as non-driver, matching `ensure_driver`'s own "flip when unsure" assumption, so we
/// restore in that case too.)
fn restore_custody_if_visitor(d: &Device, prior: u8) {
    if prior != DRIVER_MODE {
        let _ = d.release_custody();
    }
}

/// Adoption is a property of DEVICE RESOLUTION, not process startup: the first time a command
/// actually reaches for the bus and comes up short (or enumerates it), unknown Razer hardware is
/// learned (`synth::adopt_unknown` → devices/auto/<pid>.toml) and the registry reloaded — so
/// `neuron battery` works out of the box on brand-new hardware, while read-only surfaces
/// (pocket --list, profile list, …) never probe HID or write files. Once per process.
fn adopt_and_reload() -> Option<Registry> {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Once per process: the FIRST bus-facing miss learns; every later miss is a genuine
    // "not connected", so we don't re-probe the whole unknown set on each retry.
    static ADOPTED: AtomicBool = AtomicBool::new(false);
    if ADOPTED.swap(true, Ordering::SeqCst) {
        return None;
    }
    // Best-effort throughout: any failure (load, probe) degrades to None, and the caller returns
    // its original resolution error — adoption never turns a clean "no device" into a crash.
    let reg = Registry::load().ok()?;
    let a = neuron::synth::adopt_unknown(&reg).ok()?;
    for s in &a.skipped {
        eprintln!("adoption skipped {s}");
    }
    if a.adopted.is_empty() {
        return None;
    }
    for d in &a.adopted {
        eprintln!(
            "learned new device: {} (pid {:04x}) -> {}",
            d.name,
            d.pid,
            d.path.display()
        );
    }
    // Fresh load so the just-written devices/auto/*.toml are in the registry the caller retries on.
    Registry::load().ok()
}

/// Open the first connected recognized device whose registry def exposes `cmd`. Thin wrapper over
/// the shared core resolver [`neuron::device::Device::open_with_command`] (one implementation, no
/// CLI/GUI drift). On a MISS, learn brand-new hardware once and retry against the fresh registry —
/// the happy path is zero-cost (no adoption when the device already resolves).
fn open_with_command(reg: &Registry, cmd: &str) -> Result<Device> {
    Device::open_with_command(reg, cmd).or_else(|e| match adopt_and_reload() {
        Some(reg2) => Device::open_with_command(&reg2, cmd),
        None => Err(e),
    })
}

fn dpi_cmd(reg: &Registry, value: Option<u16>) -> Result<()> {
    // validate BEFORE opening/writing — cap::set_dpi has no clamp, so an out-of-range value would
    // PERSIST garbage then print MISMATCH. Bail with a clear message instead of half-succeeding.
    if let Some(v) = value {
        validate_dpi(v)?;
    }
    let d = open_with_command(reg, "dpi")?;
    if let Some(v) = value {
        // VISITOR discipline: capture the mode we found, write, then restore it whatever happens —
        // the whole reason `dpi_trap` looped was this verb leaving the driver lease held on exit.
        let prior = ensure_driver(&d);
        let res = cap::set_dpi(&d, v, v, cap::Store::Persist, neuron::dpi_origin::Cause::UserApplied);
        // volatile too — a Persist write lands onboard; the LIVE plane must match right now
        // (dual-plane discipline, same as the app's apply_dpi).
        // Name WHICH plane landed if the pair breaks apart (see `dpi_stages_cmd`). Here the order is
        // reversed, so a partial failure means onboard holds the new DPI while the live plane does not.
        let res_vol = res.and_then(|()| {
            cap::set_dpi(&d, v, v, cap::Store::Volatile, neuron::dpi_origin::Cause::UserApplied).map_err(|e| {
                anyhow::anyhow!(
                    "the ONBOARD plane accepted DPI {v} but the LIVE write failed, so the two planes \
                     are now DIVERGED (the mouse may keep its old sensitivity until a reconnect or \
                     the app's reassert). Re-run to converge them: {e}"
                )
            })
        });
        restore_custody_if_visitor(&d, prior);
        res_vol?;
    }
    // Read-back runs AFTER the restore — getters are mode-independent, so verified/MISMATCH still holds.
    let (x, y) = cap::dpi(&d)?;
    match value {
        Some(v) => println!(
            "DPI -> {x} x {y}  [{}]",
            if x == v && y == v {
                "verified"
            } else {
                "MISMATCH"
            }
        ),
        None => println!("DPI: {x} x {y}"),
    }
    Ok(())
}

fn polling_cmd(reg: &Registry, hz: Option<u32>) -> Result<()> {
    let d = open_with_command(reg, "polling_rate")?;
    if let Some(h) = hz {
        // VISITOR discipline: restore the found custody state on exit (success or error).
        let prior = ensure_driver(&d);
        let res = cap::set_polling_hz(&d, h);
        restore_custody_if_visitor(&d, prior);
        let target = res?;
        let got = cap::polling_rate_hz(&d)?;
        println!(
            "polling -> {got} Hz  [{}]",
            if got == target {
                "verified"
            } else {
                "MISMATCH"
            }
        );
    } else {
        println!("polling: {} Hz", cap::polling_rate_hz(&d)?);
    }
    Ok(())
}

fn brightness_cmd(reg: &Registry, pct: Option<u8>) -> Result<()> {
    // brightness is DUAL-DIALECT (matrix top-level command vs legacy lighting-block spec), so resolve
    // it by CAPABILITY — the command-name path skipped legacy boards (the BlackWidow) that can only
    // write brightness through the lighting block.
    let d = Device::open_with_capability(reg, neuron::registry::Capability::SetBrightness)
        // MISS: adopt brand-new hardware once, retry against the fresh registry (zero-cost when
        // the device already resolves).
        .or_else(|e| match adopt_and_reload() {
            Some(reg2) => {
                Device::open_with_capability(&reg2, neuron::registry::Capability::SetBrightness)
            }
            None => Err(e),
        })?;
    if let Some(p) = pct {
        // lighting writes need driver mode; ensure it (reversible) via the canonical core helper.
        // VISITOR discipline: capture the prior mode and restore it on exit — don't leave the driver
        // lease held from a one-shot command (the `dpi_trap` self-poisoning class of bug).
        let prior = neuron::writes::ensure_driver(&d);
        let res = cap::set_brightness(&d, p, cap::Store::Persist);
        restore_custody_if_visitor(&d, prior);
        res?;
    }
    // Read-back is BEST-EFFORT: the legacy dialect (the BlackWidow) can SET brightness but has
    // no getter — a write there is honest-but-unverifiable, and saying so beats erroring after
    // a write that landed. Devices with the getter keep the full verified/MISMATCH report.
    match (pct, d.def.has_command("brightness")) {
        (Some(p), true) => {
            let got = cap::brightness_percent(&d)?;
            println!(
                "brightness -> {got}%  [{}]",
                if got.abs_diff(p) <= 1 {
                    "verified"
                } else {
                    "MISMATCH"
                }
            );
        }
        (Some(p), false) => println!(
            "brightness -> {p}%  [sent — '{}' has no brightness getter to verify against]",
            d.def.name
        ),
        (None, true) => println!("brightness: {}%", cap::brightness_percent(&d)?),
        (None, false) => println!(
            "brightness: unreadable — '{}' can set but not report it",
            d.def.name
        ),
    }
    Ok(())
}

/// Keyboard FIRMWARE game mode — the FN+F10 Win-key kill (`GAME_LED` state). No arg reads it; `on`/
/// `off` sets it. This is the DEVICE-side kill (firmware, zero software) — the hardware sibling of
/// the host-side KEY GUARD chord swallows; it's what silently ate the user's Win key. Resolves the
/// keyboard by CAPABILITY (never enumeration order) — the `SetGameMode` setter for a write, the
/// `GameMode` getter for a bare read — mirroring `brightness_cmd`'s adopt-on-miss retry and its
/// read/write capability split. The write is read-back verified inside `cap::set_game_mode` (bails on a
/// MISMATCH), so a returned Ok already means the board reports the state we asked for.
fn gamemode_cmd(reg: &Registry, state: Option<&str>) -> Result<()> {
    // Parse + validate BEFORE opening/writing — reject junk with the valid choices, never half-run.
    let want: Option<bool> = match state {
        None => None,
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "on" | "1" | "true" => Some(true),
            "off" | "0" | "false" => Some(false),
            other => bail!("unknown game-mode state '{other}' — use: on | off"),
        },
    };
    // Resolve by the capability the operation ACTUALLY needs: a WRITE (`on`/`off`) demands the
    // SetGameMode setter, but a bare READ (`neuron game-mode`, no arg) must resolve through the
    // GameMode getter — resolving a read through the write capability would make a getter-only
    // board unreadable, which is exactly the split's purpose (same rule as Brightness vs
    // SetBrightness: read surfaces never demand write paths).
    let needed = if want.is_some() {
        neuron::registry::Capability::SetGameMode
    } else {
        neuron::registry::Capability::GameMode
    };
    let d = Device::open_with_capability(reg, needed)
        // MISS: adopt brand-new hardware once, retry against the fresh registry (zero-cost when
        // the device already resolves).
        .or_else(|e| match adopt_and_reload() {
            Some(reg2) => Device::open_with_capability(&reg2, needed),
            None => Err(e),
        })?;
    if let Some(on) = want {
        cap::set_game_mode(&d, on)?;
    }
    // Report the verified state (a re-read: after a set it confirms the write, else it's the plain
    // readout). ON means the board is eating the Win key in firmware, right now.
    if cap::game_mode(&d)? {
        println!("game mode: ON — the keyboard is eating the Win key in firmware");
    } else {
        println!("game mode: off");
    }
    Ok(())
}

#[cfg(windows)]
fn key_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
fn key_down(_vk: i32) -> bool {
    false
}

// ── SNIPER binding — a held Action on the shared rule spine (profiles/gui.rules.toml) ─────────
// Sniper is `Trigger::Input -> Action::Sniper { dpi }` in the SAME rule store the GUI authors and
// the resident app dispatches. This command just authors that one rule; the app enforces the hold
// (its immortal listener owns the DPI drop/restore, like every other bind). No `sniper.toml`, no
// standalone GetAsyncKeyState loop — one config, one dispatcher, CLI and GUI can't drift.

#[cfg(windows)]
fn gui_rules_path() -> std::path::PathBuf {
    neuron::profile::profiles_dir().join("gui.rules.toml")
}

#[cfg(windows)]
fn load_gui_rules() -> Vec<neuron::engine::Rule> {
    std::fs::read_to_string(gui_rules_path())
        .ok()
        .and_then(|s| toml::from_str::<neuron::engine::RuleDoc>(&s).ok())
        .map(|d| d.rules)
        .unwrap_or_default()
}

#[cfg(windows)]
fn save_gui_rules(rules: Vec<neuron::engine::Rule>) -> Result<()> {
    std::fs::create_dir_all(neuron::profile::profiles_dir())?;
    let doc = neuron::engine::RuleDoc { rules };
    let body = toml::to_string_pretty(&doc)?;
    neuron::salvage::atomic_write(&gui_rules_path(), body.as_bytes())?;
    Ok(())
}

/// Headless press-to-bind: listen for the first HID control (any key / button / knob) and return its
/// `(page, usage, pid)` — the native `Trigger::Input` form the GUI captures too. ESC or a 30 s
/// timeout cancels. Skips left-mouse so a stray click can't self-bind.
#[cfg(windows)]
fn capture_sniper_control() -> Option<(u16, u16, Option<neuron::registry::CanonicalPid>)> {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;
    // TWIN-EVENT PREFERENCE, mirroring the GUI capture: a side-plate press can emit BOTH its
    // keyboard usage and a receiver-echoed macro-page code; park the macro hit briefly and
    // prefer the same-device keyboard identity if it follows.
    const TWIN_SETTLE: std::time::Duration = std::time::Duration::from_millis(80);
    let stop = AtomicBool::new(false);
    let found: Cell<Option<(u16, u16, Option<neuron::registry::CanonicalPid>)>> = Cell::new(None);
    let parked: Cell<Option<(u16, u16, Option<neuron::registry::CanonicalPid>, Instant)>> =
        Cell::new(None);
    neuron::controls::listen_until(
        Some(30),
        &stop,
        true, // interactive: ESC cancels
        |ev| {
            if let Some(&(page, usage)) = ev.hits.first() {
                if (page, usage) == (0x09, 1) {
                    return; // left mouse operates the terminal, not a bindable control
                }
                // Device-scoped, macro page included. Nothing to unpack: the deferred-button
                // stream is its own field on the event now, so `ev.pid` is already the canonical
                // device whichever stream carried the press.
                let pid = ev.pid;
                match parked.get() {
                    None if page == neuron::controls::RAZER_MACRO_PAGE => {
                        parked.set(Some((page, usage, pid, Instant::now())));
                    }
                    Some((pp, pu, ppid, at)) => {
                        if at.elapsed() >= TWIN_SETTLE {
                            found.set(Some((pp, pu, ppid))); // the macro press WAS the bind
                            stop.store(true, Ordering::Relaxed);
                        } else if page != neuron::controls::RAZER_MACRO_PAGE && pid == ppid {
                            found.set(Some((page, usage, pid))); // the keyboard twin wins
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                    None => {
                        found.set(Some((page, usage, pid)));
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        },
        // on_tick: commit a parked macro candidate once its settle window closes with no twin.
        || {
            if let Some((pp, pu, ppid, at)) = parked.get() {
                if at.elapsed() >= TWIN_SETTLE {
                    found.set(Some((pp, pu, ppid)));
                    stop.store(true, Ordering::Relaxed);
                }
            }
            std::time::Duration::from_millis(5)
        },
    );
    found.get()
}

/// Sniper / on-the-fly DPI: author the held `Action::Sniper` rule (hold a control -> precision DPI,
/// release -> restore). `--bind` — or a first run with nothing bound yet — captures the hold control
/// by PRESSING it; `--dpi` sets the precision DPI. The bind lives in `profiles/gui.rules.toml` (the
/// same store the GUI edits) and the resident neuron app enforces the hold. Nothing hardcoded.
#[cfg(windows)]
fn sniper_cmd(rebind: bool, dpi_override: Option<u16>) -> Result<()> {
    use neuron::action::Action;
    use neuron::engine::{Rule, Trigger};

    let mut rules = load_gui_rules();
    let existing = rules
        .iter()
        .position(|r| matches!(r.action, Action::Sniper { .. }));
    let mut dpi = existing
        .and_then(|i| match rules[i].action {
            Action::Sniper { dpi } => Some(dpi),
            _ => None,
        })
        .filter(|d| *d != 0)
        .unwrap_or(400);
    if let Some(d) = dpi_override {
        if d != 0 {
            dpi = d;
        }
    }

    if let Some(i) = existing.filter(|_| !rebind) {
        rules[i].action = Action::Sniper { dpi };
        let label = match &rules[i].trigger {
            Trigger::Input { page, usage, .. } => neuron::controls::control_label(*page, *usage),
            other => other.describe(),
        };
        save_gui_rules(rules)?;
        println!("sniper: hold {label} -> {dpi} DPI.");
    } else {
        println!("Press the control you want as your sniper hold button (ESC to cancel)...");
        let Some((page, usage, pid)) = capture_sniper_control() else {
            bail!("cancelled — sniper unchanged");
        };
        let label = neuron::controls::control_label(page, usage);
        // keep it to exactly ONE sniper rule — a re-bind MOVES the button, never stacks a second.
        rules.retain(|r| !matches!(r.action, Action::Sniper { .. }));
        rules.push(Rule::new(Trigger::Input { page, usage, pid }, Action::Sniper { dpi }));
        save_gui_rules(rules)?;
        println!("sniper armed: hold {label} -> {dpi} DPI.");
    }
    println!("The neuron app enforces this hold while it runs (the resident dispatcher).");
    Ok(())
}

#[cfg(not(windows))]
fn sniper_cmd(_rebind: bool, _dpi_override: Option<u16>) -> Result<()> {
    anyhow::bail!(
        "sniper is Windows-only for now: it needs control capture to learn your button, and a \
         running neuron to hold the DPI while you press it. Neither exists on this platform yet."
    );
}

fn storage_status(reg: &Registry, raw: bool) -> Result<()> {
    // Resolve by CAPABILITY, not enumeration order (same rule as `battery`): "the device with
    // onboard storage", never "the first device" — which is a storage-less keyboard whenever
    // one sorts first, making the command unusable on a multi-device rig.
    let d = open_with_command(reg, "storage_info")?;
    let s = cap::storage(&d)?;
    let (macros, profiles) = cap::storage_counts(&d).unwrap_or((0, 0));
    let pct = s.pct_remaining();
    let filled = (pct as usize * 20) / 100;
    let mut bar = "#".repeat(filled);
    bar.push_str(&"-".repeat(20 - filled));

    println!(
        "Onboard pool — {} ({} KB, one shared space)",
        d.def.name,
        s.max_bytes / 1024
    );
    println!("  remaining  {pct:>3}%  [{bar}]");
    println!(
        "    free   {:>7} B   ({} immediately available + {} reclaimable)",
        s.free_bytes(),
        s.avail_bytes,
        s.recycle_bytes
    );
    println!("    used   {:>7} B", s.used_bytes());
    println!("  ------------------------------------------------");
    println!("  macros     {macros} / {} slots", s.max_macros);
    println!("  profiles   {profiles}");
    println!("  files      0            (Neuron blobs — same pool)");
    println!(
        "  -> one pool: macros, files, and profiles all draw from the same {} KB.",
        s.max_bytes / 1024
    );

    if raw {
        let info = d.run("storage_info")?;
        let dir = d.run("storage_directory")?;
        println!(
            "\n--- raw 06/8E storage_info (no translation) ---\n{}",
            hex_dump(&info, 16)
        );
        println!("--- raw 06/8D directory ---\n{}", hex_dump(&dir, 32));
    }
    Ok(())
}

fn hex_dump(a: &[u8; 80], n: usize) -> String {
    a[..n.min(80)]
        .chunks(16)
        .map(|c| {
            c.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn gesture_cmd(action: GestureCmd) -> Result<()> {
    match action {
        GestureCmd::Selftest => gesture_selftest()?,
        GestureCmd::Record { name, trigger } => gesture_record(&name, trigger)?,
        GestureCmd::Match { trigger } => gesture_match(trigger)?,
        GestureCmd::List => gesture_list(),
        GestureCmd::Tune {
            damping,
            curve,
            resid,
            threshold,
            resample,
            invariant,
        } => gesture_tune(damping, curve, resid, threshold, resample, invariant)?,
    }
    Ok(())
}

fn report_fit(label: &str, f: &glyph::GlyphFit, expect: Option<(f64, f64)>) {
    let s = glyph::signature(f);
    print!(
        "{:<22} K=({:+.3},{:+.3}) G=({:+.3},{:+.3})  |lambda|={:.4} rot={:+.4} rad/smp  rnorm={:.3}",
        label, f.k.re, f.k.im, f.g.re, f.g.im, s.mag, s.rot, s.resid_norm
    );
    if let Some((emag, erot)) = expect {
        // rotation sign is arbitrary (conjugate eigenvalue) — compare |rot|.
        let ok = (s.mag - emag).abs() < 0.03 && (s.rot.abs() - erot.abs()).abs() < 0.03;
        println!(
            "   expect |lambda|={:.3} rot={:.3}  [{}]",
            emag,
            erot,
            if ok { "PASS" } else { "FAIL" }
        );
    } else {
        println!();
    }
}

fn gesture_selftest() -> Result<()> {
    use glyph::{add_noise, analyze, fit, fit_sequence, synth_circle, synth_line, synth_line_then_circle, synth_spiral, signature, C, GlyphConfig};
    let cfg = GlyphConfig::default();
    println!("glyph eigenmotion self-test\n");

    println!("== single-block fit vs known eigenvalues ==");
    report_fit("line", &fit(&synth_line(64)).ok_or_else(|| anyhow::anyhow!("line fit failed"))?, Some((1.0, 0.0)));
    for omega in [0.20_f64, 0.50, 1.00] {
        report_fit(
            &format!("circle w={omega:.2}"),
            &fit(&synth_circle(256, 500.0, omega)).ok_or_else(|| anyhow::anyhow!("circle fit failed"))?,
            Some((1.0, omega)),
        );
    }
    for (rho, omega) in [(0.99_f64, 0.30_f64), (0.97, 0.60)] {
        report_fit(
            &format!("spiral p={rho:.2} w={omega:.2}"),
            &fit(&synth_spiral(256, 600.0, rho, omega)).ok_or_else(|| anyhow::anyhow!("spiral fit failed"))?,
            Some((rho, omega)),
        );
    }

    println!("\n== segmentation (line -> loop) ==");
    let compound = synth_line_then_circle(24, 48, 300.0, 0.45);
    for (i, f) in fit_sequence(&compound).iter().enumerate() {
        let s = signature(f);
        println!(
            "  block {i}: |lambda|={:.3} rot={:+.3} rnorm={:.3}",
            s.mag, s.rot, s.resid_norm
        );
    }

    println!("\n== recognition demo (1-NN; direction/speed/size invariant) ==");
    let tau = std::f64::consts::TAU;
    let mut vault = Vault::default();
    vault.upsert(
        "circle_cw",
        analyze(&synth_circle(140, 400.0, tau / 140.0), &cfg),
    );
    vault.upsert(
        "circle_ccw",
        analyze(&synth_circle(140, 400.0, -tau / 140.0), &cfg),
    );
    vault.upsert("line", analyze(&synth_line(160), &cfg));
    let probes: [(&str, Vec<C>); 4] = [
        ("cw small fast", synth_circle(70, 180.0, tau / 70.0)),
        ("ccw big slow", synth_circle(220, 700.0, -tau / 220.0)),
        ("noisy line", add_noise(&synth_line(140), 2.0, 9)),
        ("double loop cw", synth_circle(160, 300.0, tau / 80.0)),
    ];
    for (label, path) in probes {
        let q = analyze(&path, &cfg);
        let r = vault.recognize(&q);
        println!(
            "  {:<14} -> {:<12} score={:.4}",
            label,
            r.name.unwrap_or_else(|| "<unknown>".into()),
            r.score
        );
    }
    Ok(())
}

fn gesture_record(name: &str, trigger: i32) -> Result<()> {
    let mut vault = Vault::load();
    println!("Recording '{name}'. HOLD trigger 0x{trigger:02X}, draw, release (ESC aborts)...");
    let stroke = glyph::capture_held(neuron::controls::ControlRef::from_vk(trigger), 8192);
    if stroke.len() < 8 {
        bail!("not enough motion captured ({} pts)", stroke.len());
    }
    let word = glyph::analyze(&stroke, &vault.config);
    if word.sigs.is_empty() {
        bail!("no eigen-blocks extracted from {} pts", stroke.len());
    }
    println!(
        "captured {} pts -> {} eigen-blocks  (winding {:+.2}, bending {:.2}, closure {:.2})",
        stroke.len(),
        word.sigs.len(),
        word.inv.winding,
        word.inv.bending,
        word.inv.closure
    );
    vault.upsert(name, word);
    vault.save().map_err(anyhow::Error::msg)?;
    println!("saved '{name}' to {}", Vault::path().display());
    Ok(())
}

fn gesture_match(trigger: i32) -> Result<()> {
    let vault = Vault::load();
    if vault.templates.is_empty() {
        println!("vault is empty — record gestures first: neuron gesture record <name>");
        return Ok(());
    }
    println!("HOLD trigger 0x{trigger:02X}, draw, release...");
    let stroke = glyph::capture_held(neuron::controls::ControlRef::from_vk(trigger), 8192);
    if stroke.len() < 8 {
        bail!("not enough motion captured ({} pts)", stroke.len());
    }
    let q = glyph::analyze(&stroke, &vault.config);
    let r = vault.recognize(&q);
    match r.name {
        Some(n) => println!(
            "=> {n}   (score {:.4}, runner-up {:.4})",
            r.score,
            r.runner_up.unwrap_or(f64::INFINITY)
        ),
        None => println!(
            "=> <unknown>   (nearest score {:.4} > threshold {:.2})",
            r.score, vault.config.threshold
        ),
    }
    Ok(())
}

fn gesture_list() {
    let v = Vault::load();
    let c = v.config;
    println!(
        "attunement: w_damping={} w_curve={} w_resid={} w_invariant={} threshold={} resample={}",
        c.w_damping, c.w_curve, c.w_resid, c.w_invariant, c.threshold, c.resample
    );
    if v.templates.is_empty() {
        println!("(no gestures recorded)");
        return;
    }
    for t in &v.templates {
        println!(
            "  {:<16} {} blocks  winding {:+.2} bending {:.2} closure {:.2}",
            t.name,
            t.word.sigs.len(),
            t.word.inv.winding,
            t.word.inv.bending,
            t.word.inv.closure
        );
    }
}

fn gesture_tune(
    damping: Option<f64>,
    curve: Option<f64>,
    resid: Option<f64>,
    threshold: Option<f64>,
    resample: Option<usize>,
    invariant: Option<f64>,
) -> Result<()> {
    let mut v = Vault::load();
    if let Some(x) = damping {
        v.config.w_damping = x;
    }
    if let Some(x) = curve {
        v.config.w_curve = x;
    }
    if let Some(x) = resid {
        v.config.w_resid = x;
    }
    if let Some(x) = threshold {
        v.config.threshold = x;
    }
    if let Some(x) = invariant {
        v.config.w_invariant = x;
    }
    if let Some(x) = resample {
        v.config.resample = x;
        println!("note: changing resample affects new captures — re-record existing gestures.");
    }
    v.save().map_err(anyhow::Error::msg)?;
    let c = v.config;
    println!(
        "attunement saved: w_damping={} w_curve={} w_resid={} w_invariant={} threshold={} resample={}",
        c.w_damping, c.w_curve, c.w_resid, c.w_invariant, c.threshold, c.resample
    );
    Ok(())
}

fn list(reg: &Registry) -> Result<()> {
    // list ENUMERATES the bus, so it must SEE brand-new hardware. If any unknown Razer razer_report
    // pipe is present, adopt once and list against the fresh registry. unknown_present is a single
    // HID enumeration (no per-device probes) — cheap enough for list's "show me what's here"
    // semantics, unlike the resolution helpers that only adopt on an actual miss. The recursion
    // terminates: once adopted the pid is known (unknown_present drains), and adopt_and_reload is
    // once-per-process anyway.
    if !neuron::synth::unknown_present(reg).is_empty() {
        if let Some(reg2) = adopt_and_reload() {
            return list(&reg2);
        }
    }
    let infos = transport::enumerate()?;
    let mut found = false;
    for i in &infos {
        // find_for_pipe: one line per DRIVEN control pipe — on a two-family pid each pipe lists
        // under the family that frames it, rather than both collapsing to find_by_pid's first def.
        if let Some(def) = reg.find_for_pipe(i) {
            let mode = def.mode_for(i.pid).map_or("?", |m| m.name.as_str());
            println!(
                "{}  [{}]  pid={:04x}  mode={}",
                def.name, def.codename, i.pid, mode
            );
            found = true;
        }
    }
    if !found {
        println!("No recognized Razer devices found.");
    }
    Ok(())
}

fn discover_cmd(emit: bool) {
    use std::collections::BTreeMap;
    println!("Probing Razer razer_report devices (self-emergent; no registry)...\n");
    let devs = discover::discover();
    if devs.is_empty() {
        println!("No razer_report control pipes found.");
        return;
    }
    let reg = Registry::load().ok();

    // Differential: a class present on >1 device is generic; on exactly one, it's that
    // device's signature feature. Semantics emerge from the cross-device comparison.
    let mut freq: BTreeMap<u8, usize> = BTreeMap::new();
    for d in &devs {
        for c in &d.classes {
            *freq.entry(*c).or_default() += 1;
        }
    }
    let multi = devs.len() > 1;

    for d in &devs {
        let known = reg
            .as_ref()
            .and_then(|r| r.find_by_pid(d.vid, d.pid).map(|x| x.name.clone()));
        println!(
            "VID {:04x} PID {:04x}  {}",
            d.vid,
            d.pid,
            known.as_deref().unwrap_or("(unknown — discovered blind)")
        );
        let classes: Vec<String> = d
            .classes
            .iter()
            .map(|c| {
                let tag = if multi && freq[c] > 1 {
                    "generic"
                } else {
                    "device-specific"
                };
                format!("0x{c:02X} {} [{tag}]", discover::class_hint(*c))
            })
            .collect();
        println!("  capability classes:");
        for c in &classes {
            println!("    {c}");
        }
        println!();
    }

    if multi {
        let generic: Vec<String> = freq
            .iter()
            .filter(|(_, n)| **n > 1)
            .map(|(c, _)| format!("0x{c:02X}"))
            .collect();
        println!(
            "differential: classes on >1 device (generic device protocol) = {}",
            generic.join(", ")
        );
        println!("  -> everything else is that device's distinguishing capability — emerged, not hardcoded.");
    }

    if emit {
        // Full adoption (not a skeleton): probe the getter space, synthesize a complete def,
        // write devices/auto/<pid>.toml — the same path as `neuron adopt`.
        match reg.as_ref().map(neuron::synth::adopt_unknown) {
            Some(Ok(a)) => {
                for s in &a.skipped {
                    println!("skipped {s}");
                }
                if a.adopted.is_empty() {
                    println!("nothing to adopt: every connected device is already in the registry.");
                }
                for d in &a.adopted {
                    println!("adopted {} (pid {:04x}) -> {}", d.name, d.pid, d.path.display());
                }
            }
            Some(Err(e)) => println!("adoption failed: {e}"),
            None => println!("adoption skipped: registry failed to load"),
        }
    }
}

/// `neuron adopt`: synthesize full defs for unknown devices (write) or print the synthesis for
/// EVERY connected device (`--dry-run`) — diffing a dry-run against a curated TOML is how the
/// generalized prober is verified on proven hardware.
fn adopt_cmd(reg: &Registry, dry_run: bool) -> Result<()> {
    if dry_run {
        let infos = transport::enumerate()?;
        let mut seen = std::collections::BTreeSet::new();
        let mut any = false;
        for i in &infos {
            if i.vid != neuron::synth::RAZER_VID
                || i.feature_len != neuron::synth::RAZER_FEATURE_LEN
            {
                continue;
            }
            let Ok(t) = transport::open_path(&i.path) else {
                continue;
            };
            let ctx = neuron::synth::SynthCtx::from_info(i);
            let Some(s) = neuron::synth::synthesize(&*t, &ctx) else {
                continue; // mute collection of a device another pipe already answered for
            };
            if !seen.insert(i.pid) {
                continue;
            }
            any = true;
            let known = reg
                .find_by_pid(i.vid, i.pid).map_or_else(|| "unknown: `neuron adopt` would write this".into(), |d| format!("known: curated def '{}' would shadow this", d.name));
            println!(
                "# ── pid {:04x} · round-trip ~{}ms · {} ──────────────────────\n",
                i.pid, s.roundtrip_ms, known
            );
            println!("{}", neuron::synth::emit_toml(&s));
        }
        if !any {
            println!("no talking razer_report pipes found.");
        }
        return Ok(());
    }
    let a = neuron::synth::adopt_unknown(reg)?;
    for s in &a.skipped {
        println!("skipped {s}");
    }
    if a.adopted.is_empty() {
        println!("nothing to adopt: every connected device is already in the registry.");
    }
    for d in &a.adopted {
        println!("adopted {} (pid {:04x}) -> {}", d.name, d.pid, d.path.display());
        println!("  it is live config now (devices/auto/) — edit to refine; curated devices/*.toml shadows it.");
    }
    Ok(())
}

fn open_first(reg: &Registry) -> Result<Device> {
    let infos = transport::enumerate()?;
    for i in &infos {
        // find_for_pipe: open the def that DRIVES the first resolvable control pipe (family-aware),
        // not find_by_pid's first-by-pid def which could be a different family on a shared pid.
        if let Some(def) = reg.find_for_pipe(i) {
            return Device::open(def.clone(), i.pid);
        }
    }
    // MISS: the connected hardware may just be unknown to the registry — adopt once and retry
    // against the fresh registry (the recursion terminates: adopt_and_reload is once-per-process,
    // so the second miss returns None and we bail for real).
    if let Some(reg2) = adopt_and_reload() {
        return open_first(&reg2);
    }
    bail!("no recognized Razer device connected")
}

fn info(d: &Device) {
    println!("{} [{}]", d.def.name, d.def.codename);
    println!("  pid:      {:04x}", d.pid);
    if let Ok(fw) = cap::firmware(d) {
        println!("  firmware: v{fw}");
    }
    if let Ok(m) = cap::device_mode(d) {
        println!("  mode:     0x{m:02x}");
    }
    if let Ok((x, y)) = cap::dpi(d) {
        println!("  dpi:      {x} x {y}");
    }
    if let Ok(hz) = cap::polling_rate_hz(d) {
        println!("  polling:  {hz} Hz");
    }
    if let Ok(br) = cap::brightness_percent(d) {
        println!("  lighting: {br}% brightness");
    }
    if let Ok(b) = cap::battery_percent(d) {
        let c = cap::charging(d).unwrap_or(false);
        println!("  battery:  {b}%{}", if c { " (charging)" } else { "" });
    }
    if let Ok(s) = cap::storage(d) {
        println!(
            "  onboard:  {}% free ({} KB of {} KB, {} macro slots)",
            s.pct_remaining(),
            s.free_bytes() / 1024,
            s.max_bytes / 1024,
            s.max_macros
        );
    }
}

// ───────────────────────────────────────── tests ──────────────────────────────────────────────
//
// Pure-logic coverage of the CLI's non-IO surface: clap arg-parsing, the value
// decoders/formatters, the hex parsing in `probe`, the DPI-stage decode used by `profile capture`
// and `dpi-stages`. These tests deliberately touch NO hardware and NEVER
// arm input (`action::input_armed()` stays DISARMED) — they only exercise pure functions and the
// clap parser, exactly the layers a frontend will rely on having a stable contract for.
#[cfg(test)]
mod tests {
    use super::*;

    /// Sever the wire for the ENTIRE test binary, before any test runs — neuron-core's
    /// deny-by-default transport policy only covers its own `cfg(test)` build, and this crate links
    /// it as a plain dependency (see neuron-core/src/transport.rs). Leaked deliberately: the denial
    /// is process-lifetime; a hardware probe opts back in with `transport::allow_real_hardware()`.
    #[ctor::ctor]
    fn deny_hardware_for_all_tests() {
        std::mem::forget(neuron::transport::deny_hardware());
    }

    // Convenience: parse an argv (with the leading "neuron") into a `Cmd`, panicking on a clap error
    // so the assertions read cleanly. Uses the same derive the real binary uses.
    fn parse(args: &[&str]) -> Cmd {
        Cli::try_parse_from(args).expect("args should parse").cmd
    }

    // ── clap parsing: the new commands ───────────────────────────────────────────────────────

    #[test]
    fn parses_scroll_with_stage() {
        match parse(&["neuron", "scroll", "2"]) {
            Cmd::Scroll { stage, volatile } => {
                assert_eq!(stage, Some(2));
                assert!(!volatile, "persist is the default (matches Synapse)");
            }
            _ => panic!("expected Scroll"),
        }
    }

    #[test]
    fn parses_scroll_no_stage_is_read_only() {
        match parse(&["neuron", "scroll"]) {
            Cmd::Scroll { stage, volatile } => {
                assert_eq!(stage, None);
                assert!(!volatile);
            }
            _ => panic!("expected Scroll"),
        }
    }

    #[test]
    fn parses_scroll_volatile_flag() {
        match parse(&["neuron", "scroll", "1", "--volatile"]) {
            Cmd::Scroll { stage, volatile } => {
                assert_eq!(stage, Some(1));
                assert!(volatile);
            }
            _ => panic!("expected Scroll"),
        }
    }

    #[test]
    fn parses_dpi_stages_list() {
        match parse(&["neuron", "dpi-stages", "800", "1600", "3200"]) {
            Cmd::DpiStages {
                stages,
                active,
                persist,
            } => {
                assert_eq!(stages, vec![800, 1600, 3200]);
                assert_eq!(active, 1, "active defaults to the first (1-based)");
                assert!(!persist);
            }
            _ => panic!("expected DpiStages"),
        }
    }

    #[test]
    fn parses_dpi_stages_with_active_and_persist() {
        match parse(&[
            "neuron",
            "dpi-stages",
            "400",
            "800",
            "--active",
            "2",
            "--persist",
        ]) {
            Cmd::DpiStages {
                stages,
                active,
                persist,
            } => {
                assert_eq!(stages, vec![400, 800]);
                assert_eq!(active, 2);
                assert!(persist);
            }
            _ => panic!("expected DpiStages"),
        }
    }

    #[test]
    fn parses_dpi_stages_empty_is_read_only() {
        match parse(&["neuron", "dpi-stages"]) {
            Cmd::DpiStages { stages, .. } => assert!(stages.is_empty()),
            _ => panic!("expected DpiStages"),
        }
    }

    // ── "never fake success" guards: validate BEFORE the write ───────────────────────────────

    #[test]
    fn validate_dpi_accepts_in_range_rejects_out_of_range() {
        assert!(validate_dpi(100).is_ok(), "floor is valid");
        assert!(validate_dpi(1600).is_ok());
        assert!(validate_dpi(30_000).is_ok(), "ceiling is valid");
        // the bug case: 65000 must be rejected, not written-then-MISMATCH.
        assert!(validate_dpi(65_000).is_err(), "above the 30k ceiling");
        assert!(validate_dpi(0).is_err(), "below the floor");
        assert!(validate_dpi(50).is_err(), "below the 100 floor");
    }

    #[test]
    fn validate_scroll_stage_rejects_zero() {
        // stages are 1-based; 0 is a no-op the device ignores — must be an error, not a fake success.
        assert!(validate_scroll_stage(0).is_err());
        assert!(validate_scroll_stage(1).is_ok());
        assert!(validate_scroll_stage(2).is_ok());
    }

    #[test]
    fn validate_dpi_stages_active_rejects_out_of_range() {
        // the bug case: --active 9 on 2 stages was clamped-and-lied; now it bails.
        assert!(validate_dpi_stages_active(9, 2).is_err(), "9 > 2 stages");
        assert!(validate_dpi_stages_active(0, 2).is_err(), "active is 1-based");
        assert!(validate_dpi_stages_active(3, 2).is_err(), "one past the end");
        assert!(validate_dpi_stages_active(1, 2).is_ok());
        assert!(validate_dpi_stages_active(2, 2).is_ok(), "last stage");
        assert!(validate_dpi_stages_active(1, 1).is_ok());
    }

    // ── clap parsing: existing commands stay intact ──────────────────────────────────────────

    #[test]
    fn parses_existing_dpi_set_and_show() {
        match parse(&["neuron", "dpi", "1600"]) {
            Cmd::Dpi { value } => assert_eq!(value, Some(1600)),
            _ => panic!("expected Dpi"),
        }
        match parse(&["neuron", "dpi"]) {
            Cmd::Dpi { value } => assert_eq!(value, None),
            _ => panic!("expected Dpi"),
        }
    }

    #[test]
    fn parses_audio_out_subcommand() {
        match parse(&["neuron", "audio", "out", "--vol", "50"]) {
            Cmd::Audio {
                action: AudioCmd::Out { vol, .. },
            } => assert_eq!(vol, Some(50.0)),
            _ => panic!("expected Audio Out"),
        }
    }

    #[test]
    fn parses_audio_mic_negative_nudge() {
        // --nudge takes hyphenated values (allow_hyphen_values) for "-5".
        match parse(&["neuron", "audio", "mic", "--nudge", "-5"]) {
            Cmd::Audio {
                action: AudioCmd::Mic { nudge, .. },
            } => assert_eq!(nudge, Some(-5.0)),
            _ => panic!("expected Audio Mic"),
        }
    }

    #[test]
    fn parses_probe_explicit_and_scan() {
        match parse(&["neuron", "probe", "0221", "06", "8e"]) {
            Cmd::Probe {
                pid,
                class,
                id,
                scan,
            } => {
                assert_eq!(pid, "0221");
                assert_eq!(class.as_deref(), Some("06"));
                assert_eq!(id.as_deref(), Some("8e"));
                assert!(!scan);
            }
            _ => panic!("expected Probe"),
        }
        match parse(&["neuron", "probe", "00a8", "--scan"]) {
            Cmd::Probe { scan, .. } => assert!(scan),
            _ => panic!("expected Probe"),
        }
    }

    #[test]
    fn parses_profile_save_flags() {
        match parse(&["neuron", "profile", "save", "fps", "--dpi", "1600"]) {
            Cmd::Profile {
                action: ProfileCmd::Save { name, dpi, .. },
            } => {
                assert_eq!(name, "fps");
                assert_eq!(dpi, Some(1600));
            }
            _ => panic!("expected Profile Save"),
        }
    }

    #[test]
    fn parses_run_safe_flag() {
        match parse(&["neuron", "run", "--safe", "--seconds", "5"]) {
            Cmd::Run { seconds, safe } => {
                assert_eq!(seconds, Some(5));
                assert!(safe);
            }
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn rejects_unknown_command() {
        assert!(Cli::try_parse_from(["neuron", "frobnicate"]).is_err());
    }

    #[test]
    fn rejects_non_numeric_dpi() {
        // clap enforces the u16 type on `dpi <value>`.
        assert!(Cli::try_parse_from(["neuron", "dpi", "notanumber"]).is_err());
    }

    // ── parse_hex16 ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn hex16_parses_plain_and_prefixed() {
        assert_eq!(parse_hex16("00a8").unwrap(), 0x00a8);
        assert_eq!(parse_hex16("0x0221").unwrap(), 0x0221);
        assert_eq!(parse_hex16("FF").unwrap(), 0xff);
    }

    #[test]
    fn hex16_rejects_garbage() {
        assert!(parse_hex16("zz").is_err());
        assert!(parse_hex16("").is_err());
        assert!(parse_hex16("0x10000").is_err(), "overflows u16");
    }

    // ── parse_probe_target: the read-only getter guard ───────────────────────────────────────

    #[test]
    fn probe_target_accepts_getter_id() {
        let t = parse_probe_target(Some("06"), Some("8e")).unwrap();
        assert_eq!(t, Some((0x06, 0x8e)));
    }

    #[test]
    fn probe_target_rejects_setter_id() {
        // ids < 0x80 are setters; probe is strictly read-only and must refuse them.
        let r = parse_probe_target(Some("06"), Some("02"));
        assert!(r.is_err(), "0x02 is a setter id");
        assert!(
            parse_probe_target(Some("00"), Some("7f")).is_err(),
            "0x7f is the last setter id"
        );
    }

    #[test]
    fn probe_target_missing_class_means_sweep() {
        assert_eq!(parse_probe_target(None, None).unwrap(), None);
        assert_eq!(
            parse_probe_target(Some("06"), None).unwrap(),
            None,
            "class without id = sweep"
        );
    }

    #[test]
    fn probe_target_propagates_bad_hex() {
        assert!(parse_probe_target(Some("zz"), Some("8e")).is_err());
    }

    // ── decode_dpi_stages / decode_dpi_active: the capture/dpi-stages decode ──────────────────

    #[test]
    fn dpi_stages_decode_reads_the_table() {
        // [varstore, active, count, {id, Xhi, Xlo, Yhi, Ylo, 0, 0} * count]. Two stages: 800, 16000.
        let mut buf = [0u8; 80];
        buf[0] = 0x01; // varstore
        buf[1] = 2; // active byte — 1-BASED on the wire, so 2 = the second stage
        buf[2] = 2; // count
                    // stage 0: id=1, X=800 (0x0320)
        buf[3..10].copy_from_slice(&[0x01, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00]);
        // stage 1: id=2, X=16000 (0x3E80)
        buf[10..17].copy_from_slice(&[0x02, 0x3E, 0x80, 0x3E, 0x80, 0x00, 0x00]);
        assert_eq!(decode_dpi_stages(&buf), vec![800, 16000]);
        assert_eq!(decode_dpi_active(&buf), Some(1), "wire 2 -> 0-based index 1");
        // a wire 0 (no active reported) decodes to None, never a fake stage 1
        buf[1] = 0;
        assert_eq!(decode_dpi_active(&buf), None);
    }

    #[test]
    fn dpi_stages_decode_round_trips_the_write_payload() {
        // The CLI decode must invert the core write builder exactly (shared wire layout).
        let stages = [
            DpiStage::symmetric(400),
            DpiStage::symmetric(3200),
            DpiStage::symmetric(6400),
        ];
        let payload = writes::build_dpi_stages_payload(&stages, 2, cap::Store::Volatile).unwrap();
        assert_eq!(decode_dpi_stages(&payload), vec![400, 3200, 6400]);
        assert_eq!(decode_dpi_active(&payload), Some(2));
    }

    #[test]
    fn dpi_stages_decode_skips_zero_slots_and_short_buffers() {
        // count says 3 but only 1 real stage; zeros are skipped (empty hardware slots).
        let mut buf = vec![0u8; 24];
        buf[2] = 3;
        buf[3..10].copy_from_slice(&[0x01, 0x06, 0x40, 0x06, 0x40, 0x00, 0x00]); // 1600
        assert_eq!(decode_dpi_stages(&buf), vec![1600]);
        // too short to hold the header at all → empty, no panic.
        assert!(decode_dpi_stages(&[0x01, 0x00]).is_empty());
        assert!(decode_dpi_stages(&[]).is_empty());
        assert_eq!(decode_dpi_active(&[0x00]), None);
    }

    // ── parse_mute / parse_device_mode ───────────────────────────────────────────────────────

    #[test]
    fn mute_parses_all_synonyms() {
        for s in ["on", "ON", "true", "1", "mute"] {
            assert_eq!(parse_mute(s).unwrap(), MuteAction::On, "{s}");
        }
        for s in ["off", "OFF", "false", "0", "unmute"] {
            assert_eq!(parse_mute(s).unwrap(), MuteAction::Off, "{s}");
        }
        assert_eq!(parse_mute("toggle").unwrap(), MuteAction::Toggle);
        assert_eq!(parse_mute("Toggle").unwrap(), MuteAction::Toggle);
    }

    #[test]
    fn mute_rejects_unknown() {
        assert!(parse_mute("maybe").is_err());
        assert!(parse_mute("").is_err());
    }

    #[test]
    fn device_mode_maps_synonyms_to_bytes() {
        for s in ["driver", "host", "on", "DRIVER"] {
            assert_eq!(parse_device_mode(s).unwrap(), 0x03, "{s}");
        }
        for s in ["hardware", "onboard", "normal", "off"] {
            assert_eq!(parse_device_mode(s).unwrap(), 0x00, "{s}");
        }
        assert!(parse_device_mode("sideways").is_err());
    }

    // ── the input-safety invariant: tests never arm input ────────────────────────────────────

    #[test]
    fn tests_never_arm_input() {
        // The non-negotiable rule: the CLI test suite must leave the process-wide arm gate DISARMED
        // (default). No test calls arm_input(true); this asserts the invariant holds so a future
        // test can't silently start injecting keystrokes.
        assert!(
            !neuron::action::input_armed(),
            "input must stay DISARMED in tests"
        );
    }
}
