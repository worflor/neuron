// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::float_cmp, clippy::drop_non_drop, clippy::field_reassign_with_default))]

//! Neuron CLI — the lightweight, open replacement for Razer Synapse.

mod cast_cmd;
mod control;
mod device_cmds;
mod light;
mod live;
mod macro_cmd;
mod out;
mod profile_cmd;
mod setup_cmd;
mod spec;

use anyhow::{bail, Context, Result};
use cast_cmd::{CastCmd, GestureCmd};
use clap::{CommandFactory as _, Parser, Subcommand};
use macro_cmd::MacroCmd;
use profile_cmd::ProfileCmd;
use spec::BindCmd;
use std::fmt::Write as _;
#[cfg(windows)]
use neuron::{
    audio,
    device::DeviceSession,
    executor::{DispatchExecutor, DispatchOutcome, IntentRunner, TurboRuntime},
};
#[cfg(windows)]
use neuron::intent::ProfileCursor;
use neuron::{
    backup,
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
    /// one JSON document on stdout; errors go to stderr as {"error": "..."}
    #[arg(long, global = true)]
    json: bool,
    /// do not signal a running app after a config edit (then run `neuron reload`)
    #[arg(long, global = true)]
    no_live: bool,
    /// show engine chatter (also `NEURON_DEBUG=1`)
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per process; boxing every variant buys nothing
enum Cmd {
    /// List connected, recognized Razer devices
    ///
    /// A device no definition covers yet is reported as `new device ...`; `neuron adopt` learns
    /// it. This verb never writes.
    List,
    /// Probe any Razer device with no registry (debug)
    ///
    /// Probes every `razer_report` device and compares their capability classes. `--emit` adopts
    /// unknown devices, the same as `neuron adopt`.
    #[command(hide = true)]
    Discover {
        /// adopt unknown devices: synthesize a FULL device def per unknown device and write it
        /// to devices/auto/ in the run root (same as `neuron adopt`)
        #[arg(long)]
        emit: bool,
    },
    /// Learn unknown Razer devices into devices/auto/
    ///
    /// Probes the getter space of each unknown device, synthesizes a complete definition
    /// (commands, lighting dialect, measured link pacing) and writes devices/auto/<pid>.toml in
    /// the run root. The file is plain per-device config from then on: editable, never
    /// overwritten.
    Adopt {
        /// print the synthesized TOML instead of writing files, and include ALREADY-KNOWN
        /// devices — diff against a curated def to verify synthesis on proven hardware
        #[arg(long)]
        dry_run: bool,
    },
    /// Show device info (firmware, mode, battery)
    Info {
        /// device pid (hex) when several are connected; default is the first
        #[arg(long)]
        pid: Option<String>,
    },
    /// Show battery level and charging state
    Battery,
    /// Mouse DPI: show, or set (same as `feel dpi`)
    ///
    /// A value sets it, verified by read-back.
    #[command(hide = true)]
    Dpi { value: Option<u16> },
    /// Polling rate in Hz: show, or set (`feel polling`)
    ///
    /// Snaps to 1000/500/250/125.
    #[command(hide = true)]
    Polling { hz: Option<u32> },
    /// DPI stage list: show, or set (`feel stages`)
    ///
    /// A list of values sets the whole table via the verify-gated `set_dpi_stages` write, e.g.
    /// `neuron dpi-stages 800 1600 3200`. `--active` picks the live stage, counting from 1.
    /// `--persist` flashes the table to onboard memory.
    #[command(hide = true)]
    DpiStages {
        /// the DPI values that make up the cycle, in order (omit to just read the current table)
        stages: Vec<u16>,
        /// which stage is active, counting from 1 (bind indexes count from 0); default 1
        #[arg(long, default_value_t = 1)]
        active: u8,
        /// flash the stage table to onboard memory (survives with no software running)
        #[arg(long)]
        persist: bool,
    },
    /// HyperScroll wheel stage: show, or set (`feel scroll`)
    ///
    /// A value selects the stage, counting from 1, via the wire-confirmed `set_scroll_stage`
    /// write.
    #[command(hide = true)]
    Scroll {
        /// the 1-based stage to make active (omit to just read the current stage)
        stage: Option<u8>,
        /// flash the choice to onboard memory (default: persist, matching what Synapse sends)
        #[arg(long)]
        volatile: bool,
    },
    /// Lift-off distance: show, or set (`feel lod`)
    ///
    /// `--lift N --landing M` sets an asymmetric split (verify-gated); `--sym L` sets a symmetric
    /// level (0=low, 1=med, 2=high).
    #[command(hide = true)]
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
    /// Lighting brightness 0-100 (`feel brightness`)
    #[command(hide = true)]
    Brightness { pct: Option<u8> },
    /// Keyboard firmware game mode (`feel game-mode`)
    ///
    /// No argument reads it; `on`/`off` sets it, verified by read-back. This is the firmware Win-
    /// key kill (FN+F10), distinct from the host-side key guard.
    #[command(hide = true)]
    GameMode {
        /// on | off (omit to just read the current state)
        state: Option<String>,
    },
    /// Hold a control for precision DPI (`feel sniper`)
    ///
    /// Authors the bind into the shared rule store; the running app enforces the hold. `--trigger
    /// mouse:5 --dpi 400` sets it; a bare `--trigger` presses a control to pick it; no flags
    /// shows it.
    #[command(hide = true)]
    Sniper(setup_cmd::SniperArgs),
    /// Onboard memory: one pool for macros, files, profiles
    ///
    /// Shows the unified 124 KB pool. `--raw` also dumps the raw 06/8E and 06/8D device bytes.
    Storage {
        /// also dump the raw 06/8E + 06/8D device bytes (full transparency)
        #[arg(long)]
        raw: bool,
    },
    /// Vibrate a pad's motors, or `--tour` every motor
    Rumble {
        #[arg(long, default_value_t = 0.0)]
        low: f32,
        #[arg(long, default_value_t = 0.0)]
        high: f32,
        #[arg(long, default_value_t = 0.0)]
        left_trigger: f32,
        #[arg(long, default_value_t = 0.0)]
        right_trigger: f32,
        #[arg(long, default_value_t = 500)]
        ms: u64,
        /// walk every motor through 25%, 50% and 100%, one at a time
        #[arg(long)]
        tour: bool,
    },
    /// Live view of gamepads: sticks, battery, motion
    Pads {
        /// how long to listen, seconds
        #[arg(long, default_value_t = 20)]
        seconds: u64,
        /// pulse every pad's motors once (`neuron rumble` is the documented spelling)
        #[arg(long, hide = true)]
        rumble: bool,
    },
    /// Watch headset knob, mute and consumer-control events
    Watch {
        /// how long to listen, seconds
        #[arg(long, default_value_t = 20)]
        seconds: u64,
        /// instead: what a Razer mouse pushes on its own (side-plate swaps, DPI and scroll-stage
        /// changes, deferred buttons), raw bytes and decoded
        #[arg(long)]
        device: bool,
    },
    /// Glyph gestures: record, list, rename, delete, bind, tune
    Gesture {
        #[command(subcommand)]
        action: GestureCmd,
    },
    /// Mic and speaker volume and mute (Windows Core Audio)
    ///
    /// The OS audio path, so it works on any endpoint, not only Razer ones.
    Audio {
        #[command(subcommand)]
        action: AudioCmd,
    },
    /// The Trigger -> Action binds: list, add, edit, remove
    ///
    /// List, add, edit, remove and reorder binds, the hypershift hold key, and the live rule set.
    /// Triggers and actions are `kind:value` shorthands or JSON/TOML. Bind indexes count from 0.
    Bind {
        #[command(subcommand)]
        action: BindCmd,
    },
    /// The controls a bind can name, and press-to-capture
    Control {
        #[command(subcommand)]
        action: control::ControlCmd,
    },
    /// Every action a bind can run: ids, examples, checks
    ///
    /// Shorthand ids, a JSON example of each variant, and validation.
    Action {
        #[command(subcommand)]
        action: spec::ActionCmd,
    },
    /// Every trigger kind, its shorthand, and validation
    Trigger {
        #[command(subcommand)]
        action: spec::TriggerCmd,
    },
    /// Firmware button functions: plan, read, apply, restore
    ///
    /// Shows the plan the binds imply, reads the device, applies it (verified) or restores stock.
    Button {
        #[command(subcommand)]
        action: device_cmds::ButtonCmd,
    },
    /// The emblem and name every surface shows for a device
    Badge {
        #[command(subcommand)]
        action: device_cmds::BadgeCmd,
    },
    /// Timing windows, hypershift, sniper, and device feel verbs
    ///
    /// Timing windows, the hypershift stance, sniper, and the device feel verbs (dpi, polling,
    /// stages, scroll, lod, brightness, game-mode, idle). The old top-level spellings, like
    /// `neuron dpi`, still work.
    Feel {
        #[command(subcommand)]
        action: setup_cmd::FeelCmd,
    },
    /// LED idle-off timeout in seconds (`feel idle`)
    ///
    /// 0 means never. A value sets it, verified by read-back.
    #[command(hide = true)]
    Idle { secs: Option<u32> },
    /// Config file locations and the app's preferences
    Config {
        #[command(subcommand)]
        action: setup_cmd::ConfigCmd,
    },
    /// Write the whole authored setup as one document
    ///
    /// Binds, cast, feel, routes, profiles with lighting and binds, badges, macros and
    /// preferences.
    Dump(setup_cmd::DumpArgs),
    /// Make the machine match a setup document from `dump`
    ///
    /// Validated first, idempotent.
    Apply(setup_cmd::ApplyArgs),
    /// Tell a running app to re-read its config
    Reload,
    /// Version, run root, app running, active profile, counts
    Status,
    /// Everything an agent can name: actions, triggers, lighting
    ///
    /// Pass --json for the full machine-readable form.
    Catalog,
    /// Run the remap daemon (ESC stops it)
    ///
    /// Listens for control events and fires bound actions.
    Run {
        /// stop after N seconds (default: run until ESC)
        #[arg(long)]
        seconds: Option<u64>,
        /// safe mode — observe + dry-run only: keep real input synthesis DISARMED so no
        /// keystrokes/clicks are actually injected (the engine still resolves & reports).
        #[arg(long)]
        safe: bool,
    },
    /// Lighting: capabilities, layers, presets, live effects
    ///
    /// No subcommand shows capabilities (live). `lighting effect spectrum` dry-runs the exact
    /// bytes per device (writes gated). `lighting catalog` and `lighting stack` author the look
    /// as data.
    #[command(alias = "light")]
    Lighting {
        #[command(subcommand)]
        action: Option<LightingCmd>,
    },
    /// Radial menu: hold, flick a direction, release
    Radial {
        #[command(subcommand)]
        action: RadialCmd,
    },
    /// Cast engine: one trigger for glyphs, flicks and rhythms
    ///
    /// One trigger fires gestures (drawn glyphs), radial flicks and tap rhythms. `cast show`,
    /// `set`, `wedge`, `rhythm` and `glyph` edit it.
    Cast {
        #[command(subcommand)]
        action: CastCmd,
    },
    /// Profiles: settings, lighting and binds as one object
    ///
    /// new, set, apply, export, import, rename, delete, and `route` (which app switches to which
    /// profile).
    Profile {
        #[command(subcommand)]
        action: ProfileCmd,
    },
    /// Switch a device between driver and hardware control
    ///
    /// `driver`: the host (Neuron) controls the device, as Synapse does. `hardware`: the onboard
    /// profile runs on its own.
    Mode {
        /// driver | hardware
        mode: String,
        /// device PID (hex), e.g. 00a8
        #[arg(long)]
        pid: String,
    },
    /// RETIRED spelling of a firmware rebind: it wrote a register (15/02) that never changed what
    /// a key emits. It now authors the bind (`neuron bind add`) and leaves the firmware write to the
    /// button planner (`neuron button apply`). `--key <stock key>` or `--button <hex id>` names the
    /// button, `--to <key>` the key it should emit.
    #[command(hide = true)]
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
    /// Snapshot every device's state to backups/ (read-only)
    ///
    /// The first move of every safe write: a known-good restore and verify reference.
    Backup {
        /// only this PID (hex), e.g. 00a8
        #[arg(long)]
        pid: Option<String>,
    },
    /// Diff a device against a saved backup (read-only)
    ///
    /// Re-snapshots the device and compares it with a backups/*.json file, to confirm only
    /// intended bytes moved.
    Verify {
        /// path to a backups/*.json file
        file: String,
    },
    /// Import Synapse config, or a Synapse export file
    ///
    /// With no FILE, surveys the Razer config on this machine (`--deep` also prints sample
    /// values). With a `*.synapse3` or `*.ChromaEffects` export, previews what would import;
    /// `--apply` writes the profile and rules.
    Import {
        /// a Synapse export (`*.synapse3` / `*.ChromaEffects`) or a Synapse 3 mapping log (`*Mapping*.log`); omit to survey this machine's config
        file: Option<String>,
        /// survey only: also extract and print sample values
        #[arg(long, conflicts_with = "file")]
        deep: bool,
        /// with FILE: write the imported profile and rules (default: preview)
        #[arg(long, requires = "file")]
        apply: bool,
    },
    /// Import a Synapse export file (same as `import FILE`)
    #[command(hide = true)]
    ImportExport {
        /// path to the exported file (a ZIP with a fake extension)
        file: String,
        /// write the imported Profile + rules to disk (else just preview what would import)
        #[arg(long)]
        apply: bool,
    },
    /// Python macros: add, run, check, options
    ///
    /// The bundled CPython starts on demand. New macros are bound to Neuron's capabilities; `#
    /// neuron: raw` enables unrestricted Python for a file.
    Macro {
        #[command(subcommand)]
        action: MacroCmd,
    },
    /// KNOCKBACK rhythm familiar: demo, stats, SVG export
    ///
    /// Plays a scripted session headless, prints the brain's stats and exports the sigil and
    /// storyboard SVGs.
    Twin {
        #[command(subcommand)]
        action: TwinCmd,
    },
    /// Portable clipboards: stash, restore, list
    ///
    /// `neuron pocket a` moves the clipboard into or out of pocket `a`: it stashes if the
    /// clipboard has content, restores if the pocket does, swaps if both, carrying every format.
    /// `--list` shows what each pocket holds, `--sigil out.svg` exports a pocket's content sigil,
    /// `--keep` makes it survive a restart.
    Pocket(PocketArgs),
    /// Read raw getters from a device (debug)
    ///
    /// Interrogates a device's `razer_report` getter space with raw bytes: `neuron probe 0221
    /// --scan` or `neuron probe 0221 06 8e`.
    #[command(hide = true)]
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
    /// Input-pump counters (debug)
    ///
    /// Read-only diagnostics: wake counts by reason, wake-to-first-edge latency histogram,
    /// tick-starvation watchdog.
    /// Counters are in-process: `neuron run` prints its own session's snapshot on exit.
    #[command(hide = true)]
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
    /// Every preset and pattern (with its knobs) a layer can use
    Catalog,
    /// The layer stack of a profile or device: list, add, set, rm, mv, clear, replace
    Stack {
        #[command(subcommand)]
        action: light::StackCmd,
    },
    /// Your own saved looks: save, export, import, delete, list
    Saved {
        #[command(subcommand)]
        action: light::EffectCmd,
    },
    /// Paint a profile's lighting now (same as `profile apply NAME`)
    Apply { profile: String },
    /// The stream frame rate (1-30) a device's saved look uses: read, set, or `0` to clear
    Fps {
        /// device pid (hex)
        #[arg(long)]
        pid: String,
        value: Option<u32>,
    },
    /// Stream a computed effect live.
    ///
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
    /// Preview an effect's bytes (dry run).
    ///
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
    /// Paint the mouse's vitals onto the keyboard.
    ///
    /// CROSS-DEVICE DATA SURFACE — paint the MOUSE's live vitals (battery / charge / active DPI
    /// stage) onto the KEYBOARD's LED matrix. One process speaking BOTH devices: the mouse is the
    /// data source, the keyboard the sink. Loops ~1s, repainting (on-demand, ACK'd) only when the
    /// state changes. Runs until ESC unless `--seconds N` is given.
    Mirror {
        /// stop after N seconds (default: run until ESC)
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Walk the keymap key by key to verify it.
    ///
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
    /// Light every matrix cell in turn.
    ///
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
    /// Light a block of cells to map a wide key.
    ///
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

/// `--help` groups.
///
/// clap 4 groups options under headings but not subcommands, so the top-level command list is
/// built here and every subcommand is hidden from clap's own (flat) list. Hidden commands (the
/// old spellings, the debug harnesses) stay reachable by name. A test keeps this table and the
/// command tree in step.
const HELP_GROUPS: &[(&str, &[&str])] = &[
    ("Devices", &["list", "info", "battery", "adopt", "storage", "mode", "backup", "verify", "badge", "button"]),
    ("Feel", &["feel"]),
    ("Input", &["bind", "control", "action", "trigger", "cast", "gesture", "radial", "macro", "pocket", "run", "watch", "pads", "rumble", "audio", "twin"]),
    ("Lighting", &["lighting"]),
    ("Setup", &["profile", "config", "dump", "apply", "reload", "status", "catalog", "import"]),
];

/// Exit for a command line clap rejected: help and version as clap prints them, anything else as
/// plain lines (`{"error": ...}` under `--json`) with exit code 2.
#[allow(clippy::exit)] // Argument parsing finishes before any runtime resources are acquired.
fn usage_error(e: &clap::Error) -> ! {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if !e.use_stderr() {
        e.exit();
    }
    let rendered = e.render().to_string();
    let mut lines: Vec<String> = rendered
        .lines()
        .map(|l| l.trim().trim_start_matches("error: ").to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("Usage:") && !l.starts_with("For more information"))
        .collect();
    if let Some(first) = lines.first_mut() {
        *first = human_usage_line(first);
    }
    if lines.len() > 1 && lines[0].ends_with(':') {
        let next = lines.remove(1);
        lines[0] = format!("{} {next}", lines[0]);
    }
    // Belt and braces for the top level, where every subcommand is hidden (see `grouped_command`).
    if e.kind() == ErrorKind::InvalidSubcommand && !lines.iter().any(|l| l.starts_with("tip:")) {
        if let Some(ContextValue::String(bad)) = e.get(ContextKind::InvalidSubcommand) {
            let names: Vec<String> = Cli::command().get_subcommands().map(|s| s.get_name().to_string()).collect();
            if let Some(near) = neuron::authoring::nearest(bad, names.iter().map(String::as_str)) {
                lines.push(format!("Did you mean {near}?"));
            }
        }
    }
    if out::json() {
        eprintln!("{}", serde_json::json!({ "error": lines.join(" ") }));
    } else {
        eprintln!("error: {}
Run `neuron help` for usage.", lines.join("
"));
    }
    std::process::exit(2);
}

/// Rewrites clap's `invalid value 'x' for '<ARG>': <Rust parse error>` into plain words.
fn human_usage_line(line: &str) -> String {
    let Some(rest) = line.strip_prefix("invalid value '") else { return line.to_string() };
    let Some((value, rest)) = rest.split_once("' for '") else { return line.to_string() };
    let Some((arg, reason)) = rest.split_once("': ") else { return line.to_string() };
    let arg = arg.trim_matches(|c| matches!(c, '[' | ']' | '<' | '>'));
    let why = if reason.contains("invalid digit") {
        "expected a whole number".to_string()
    } else if reason.contains("empty string") {
        "a number is required".to_string()
    } else if reason.contains("too large") {
        "that number is too large".to_string()
    } else if reason.contains("too small") {
        "that number is too small".to_string()
    } else if reason.contains("invalid float") {
        "expected a number, like 0.5".to_string()
    } else if let Some((_, range)) = reason.split_once(" is not in ") {
        match range.split_once("..=") {
            Some((lo, hi)) => format!("must be between {lo} and {hi}"),
            None => reason.to_string(),
        }
    } else {
        reason.to_string()
    };
    format!("'{value}' is not valid for {arg}: {why}.")
}

fn grouped_command() -> clap::Command {
    let mut cmd = Cli::command();
    let about = |c: &clap::Command, name: &str| c.find_subcommand(name).and_then(|s| s.get_about()).map(ToString::to_string).unwrap_or_default();
    let width = HELP_GROUPS.iter().flat_map(|(_, names)| names.iter()).map(|n| n.len()).max().unwrap_or(0);
    let mut listing = String::new();
    for (heading, names) in HELP_GROUPS {
        let _ = writeln!(listing, "{heading}:");
        for n in *names {
            let _ = writeln!(listing, "  {n:<width$}  {}", about(&cmd, n));
        }
        listing.push('\n');
    }
    listing.push_str("Run `neuron help <command>` for a command's options and examples.");
    let names: Vec<String> = cmd.get_subcommands().map(|s| s.get_name().to_string()).collect();
    for n in names {
        cmd = cmd.mut_subcommand(n, |s| s.hide(true));
    }
    cmd.after_help(listing)
        .bin_name("neuron")
        .override_usage("neuron [OPTIONS] <COMMAND>")
        .help_template("{about}

{usage-heading} {usage}{after-help}

{all-args}")
}

fn main() -> std::process::ExitCode {
    // `--json` is read before clap so even a failure to parse or to start reports in the shape the
    // caller asked for.
    out::set_json(std::env::args().any(|a| a == "--json"));
    // The command tree is large enough that building it in an unoptimized build overflows the 1 MB
    // main-thread stack on Windows, so the whole run happens on a thread with room.
    let worker = std::thread::Builder::new().name("neuron-cli".into()).stack_size(32 << 20).spawn(run);
    let result = match worker {
        Ok(h) => h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("the command panicked"))),
        Err(e) => Err(anyhow::anyhow!("could not start the command thread: {e}")),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", out::error_line(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // Before ANY config read: carry a build-tree config universe forward (see
    // `runroot::adopt_legacy_run_root`). Both binaries do this because either one can be the first
    // to start after an upgrade, and they share one config universe — whoever gets there first
    // migrates, the other no-ops.
    if let Some((from, to)) = neuron::runroot::adopt_legacy_run_root()? {
        eprintln!("carried config forward: {} -> {}", from.display(), to.display());
    }
    let cli = {
        use clap::FromArgMatches as _;
        Cli::from_arg_matches(&grouped_command().try_get_matches().unwrap_or_else(|e| usage_error(&e))).unwrap_or_else(|e| e.exit())
    };
    out::set_json(cli.json);
    out::set_verbose(cli.verbose);
    live::set_skip(cli.no_live);
    let reg = Registry::load()?;
    // Adoption is NOT a startup step (it would give read-only commands a hidden HID-probe + file
    // write): it fires lazily inside device resolution, the moment a command actually reaches for
    // the bus and comes up short. See `adopt_and_reload`.
    match cli.cmd {
        Cmd::List => list(&reg)?,
        Cmd::Discover { emit } => discover_cmd(emit),
        Cmd::Adopt { dry_run } => adopt_cmd(&reg, dry_run)?,
        Cmd::Info { pid } => {
            let (d, pos, total) = open_for_info(&reg, pid.as_deref())?;
            info(&d, pos, total)?;
        }
        Cmd::Battery => {
            // Resolve by CAPABILITY, not enumeration order — "the device with a battery",
            // never "the first device" (which is a keyboard whenever one sorts first). Via the
            // local wrapper so a brand-new battery device is adopted-on-miss (see open_with_command).
            let d = open_with_command(&reg, "battery_level")?;
            let pct = cap::battery_percent(&d)?;
            let charging = cap::charging(&d).unwrap_or(false);
            out::emit(&serde_json::json!({ "percent": pct, "charging": charging }), || {
                println!(
                    "Battery: {pct}%{}",
                    if charging { " (charging)" } else { "" }
                );
            })?;
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
        Cmd::Sniper(a) => setup_cmd::sniper(a)?,
        Cmd::Storage { raw } => storage_status(&reg, raw)?,
        Cmd::Watch { seconds, device: true } => watch_device_cmd(&reg, seconds)?,
        Cmd::Watch { seconds, device: false } => neuron::controls::watch(seconds),
        Cmd::Pads { seconds, rumble } => neuron::controls::pads(seconds, rumble),
        Cmd::Rumble { low, high, left_trigger, right_trigger, ms, tour } => rumble_cmd(
            neuron::haptics::Rumble { low, high, left_trigger, right_trigger },
            ms,
            tour,
        )?,
        Cmd::Gesture { action } => cast_cmd::gesture(action)?,
        Cmd::Audio { action } => audio_cmd(action)?,
        Cmd::Bind { action } => spec::bind(action)?,
        Cmd::Control { action } => control::run(action, &reg)?,
        Cmd::Action { action } => spec::action(action)?,
        Cmd::Trigger { action } => spec::trigger(action)?,
        Cmd::Button { action } => device_cmds::button(action, &reg)?,
        Cmd::Badge { action } => device_cmds::badge(action)?,
        Cmd::Feel { action } => setup_cmd::feel(action, &reg)?,
        Cmd::Idle { secs } => device_cmds::idle(&reg, secs)?,
        Cmd::Config { action } => setup_cmd::config(action)?,
        Cmd::Dump(a) => setup_cmd::dump(a)?,
        Cmd::Apply(a) => setup_cmd::apply(a)?,
        Cmd::Reload => setup_cmd::reload()?,
        Cmd::Status => setup_cmd::status()?,
        Cmd::Catalog => setup_cmd::catalog()?,
        Cmd::Run { seconds, safe } => run_daemon(&reg, seconds, safe),
        Cmd::Lighting { action } => lighting_cmd(&reg, action)?,
        Cmd::Mode { mode, pid } => mode_cmd(&reg, &mode, &pid)?,
        Cmd::Remap {
            key,
            button,
            to,
            reset,
        } => device_cmds::remap(&reg, key.as_deref(), button.as_deref(), to.as_deref(), reset)?,
        Cmd::Backup { pid } => backup_cmd(&reg, pid.as_deref())?,
        Cmd::Verify { file } => verify_cmd(&file)?,
        Cmd::Import { file: Some(f), apply, .. } => import_export_cmd(&f, apply)?,
        Cmd::Import { file: None, deep, .. } => import_cmd(deep),
        Cmd::ImportExport { file, apply } => import_export_cmd(&file, apply)?,
        Cmd::Macro { action } => macro_cmd::run(action)?,
        Cmd::Radial { action } => radial_cmd(action),
        Cmd::Cast { action } => cast_cmd::cast(action)?,
        Cmd::Profile { action } => profile_cmd::run(action, &reg)?,
        Cmd::Twin { action } => twin_cmd(action)?,
        Cmd::Probe {
            pid,
            class,
            id,
            scan,
        } => probe_cmd(&pid, class.as_deref(), id.as_deref(), scan)?,
        Cmd::Pocket(args) => pocket_cmd(args)?,
        Cmd::Prof {
            action: ProfCmd::Pump,
        } => prof_pump_cmd(),
    }
    Ok(())
}

#[derive(clap::Args)]
struct PocketArgs {
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
    /// print the full contents of the named pocket
    #[arg(long, conflicts_with_all = ["delete", "list", "history", "clear_history", "history_item"])]
    inspect: bool,
    /// delete a named pocket
    #[arg(long, conflicts_with_all = ["inspect", "list", "history", "clear_history", "history_item"])]
    delete: bool,
    /// list in-session clipboard history metadata
    #[arg(long, conflicts_with_all = ["inspect", "delete", "clear_history", "history_item", "sigil"])]
    history: bool,
    /// print one full history item by newest-first index
    #[arg(long, conflicts_with_all = ["inspect", "delete", "list", "history", "clear_history", "sigil"])]
    history_item: Option<usize>,
    /// clear in-session clipboard history
    #[arg(long, conflicts_with_all = ["inspect", "delete", "list", "history", "history_item", "sigil"])]
    clear_history: bool,
}

/// Portable clipboards from the CLI: list every pocket's contents, export a pocket's emergent
/// content-sigil, or MOVE the clipboard into/out of a named pocket. The move writes the clipboard
/// (a real mutation), so it arms input for this one-shot — the process exits right after, and the
/// list/sigil paths stay read-only (no arm).
fn pocket_cmd(args: PocketArgs) -> Result<()> {
    let PocketArgs { name, list, keep, sigil, inspect, delete, history, history_item, clear_history } = args;
    let disp = |s: &str| {
        if s.is_empty() {
            "(default)".to_string()
        } else {
            s.to_string()
        }
    };
    let formats = |ids: &[u32]| ids.iter().map(|id| format!("{id:04x}")).collect::<Vec<_>>().join(",");
    if list {
        let live = neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::List, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)?;
        let (entries, live_view) = match live {
            Some(neuron::livesync::PocketReply::Pockets { entries }) => (entries, true),
            Some(_) => bail!("resident app returned an unexpected pocket reply"),
            None => (neuron::pocket::metadata().into_iter().filter(|(_, durable, _, _)| *durable).map(|(slot, durable, ids, bytes)| neuron::livesync::PocketMetadata { slot, durable, kind: if ids.contains(&13) || ids.contains(&1) { "text" } else if ids.contains(&15) { "files" } else if ids.contains(&8) || ids.contains(&17) { "image" } else { "other" }.into(), formats: ids, bytes }).collect(), false),
        };
        if !live_view { println!("offline view · durable pockets only"); }
        if entries.is_empty() { println!("no pockets yet (move something: neuron pocket <name>)"); return Ok(()); }
        for entry in entries {
            let kept = if entry.durable { " · kept" } else { "" };
            println!("  {:<16} {:<6} [{}] · {} bytes{kept}", disp(&entry.slot), entry.kind, formats(&entry.formats), entry.bytes);
        }
        return Ok(());
    }
    if history {
        let Some(neuron::livesync::PocketReply::History { entries }) = neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::History, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)? else {
            bail!("session history is available only while the resident app is running");
        };
        if entries.is_empty() { println!("clipboard history is empty"); }
        for entry in entries {
            println!("  {:<3} {:<6} [{}] · {} bytes", entry.index, entry.kind, formats(&entry.formats), entry.bytes);
        }
        return Ok(());
    }
    if clear_history {
        match neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::ClearHistory, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)? {
            Some(neuron::livesync::PocketReply::Cleared) => println!("clipboard history cleared"),
            None => bail!("session history is available only while the resident app is running"),
            _ => bail!("resident app returned an unexpected pocket reply"),
        }
        return Ok(());
    }
    if let Some(index) = history_item {
        let Some(neuron::livesync::PocketReply::HistoryItem { contents: Some(item) }) = neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::HistoryItem { index }, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)? else {
            bail!("session history item is unavailable; keep the resident app running and check the newest-first index");
        };
        let item = item.decode().map_err(anyhow::Error::msg)?;
        println!("history #{index}: {} formats · {} bytes", item.formats.len(), item.formats.iter().map(|f| f.bytes.len()).sum::<usize>());
        if let Some(text) = item.text() { print!("{text}"); if !text.ends_with('\n') { println!(); } }
        return Ok(());
    }
    let slot = name.unwrap_or_default();
    if inspect {
        neuron::pocket::validate_slot_name(&slot).map_err(anyhow::Error::msg)?;
        let live = neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::Inspect { slot: slot.clone() }, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)?;
        let item = match live {
            Some(neuron::livesync::PocketReply::Pocket { contents, .. }) => contents.map(neuron::livesync::WirePocket::decode).transpose().map_err(anyhow::Error::msg)?,
            Some(_) => bail!("resident app returned an unexpected pocket reply"),
            None => neuron::pocket::inspect(&slot),
        };
        let Some(item) = item else { bail!("pocket {} does not exist", disp(&slot)); };
        println!("pocket {}: {} formats · {} bytes", disp(&slot), item.formats.len(), item.formats.iter().map(|f| f.bytes.len()).sum::<usize>());
        if let Some(text) = item.text() { print!("{text}"); if !text.ends_with('\n') { println!(); } }
        return Ok(());
    }
    if delete {
        let deleted = match neuron::livesync::request_pockets(&neuron::livesync::PocketRequest::Delete { slot: slot.clone() }, std::time::Duration::from_secs(5)).map_err(anyhow::Error::msg)? {
            Some(neuron::livesync::PocketReply::Deleted { removed }) => removed,
            Some(_) => bail!("resident app returned an unexpected pocket reply"),
            None => neuron::pocket::delete(&slot).map_err(anyhow::Error::msg)?,
        };
        if deleted { println!("deleted pocket {}", disp(&slot)); }
        else { println!("pocket {} does not exist", disp(&slot)); }
        return Ok(());
    }
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
    let status = neuron::pocket::activate(&slot, true);
    // activate() persists on a worker thread; this process exits NOW — flush synchronously or
    // the worker dies mid-write and the stash evaporates (live-verified before this call existed).
    neuron::pocket::flush_durable_sync()?;
    println!("{status}");
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
    out::emit(&serde_json::json!({ "captured": name, "summary": p.summary(), "profile": p }), || {
        println!("captured '{name}' from live device state: {}", p.summary());
    })
}

/// Apply a profile — write each set field to whichever device owns that capability. The
/// orchestration spine: settings via the typed setters, lighting via the unified backend.
fn profile_apply(reg: &Registry, name: &str) -> Result<()> {
    let p = Profile::load(name)?;
    say!("applying '{name}': {}", p.summary());

    // Delegate to the ONE canonical apply orchestration in neuron-core (`Profile::apply`). It applies
    // the FULL DPI stage list (not just the single active DPI), polling/brightness, the per-LED frame
    // and named effect across all lit devices, plus the gated idle/in-game writes (reported honestly
    // as `[gated]`, never faked), and returns the host-side gaming-mode policy. The GUI renders apply
    // from this exact same report — no duplicated apply logic.
    let report = p.apply(reg);
    for line in &report.applied {
        say!("  {line}");
    }
    for line in &report.gated {
        say!("  {line} [gated — derived write off by default; enable via env flag]");
    }
    for line in &report.skipped {
        say!("  skipped: {line}");
    }
    // Push the applied profile's gaming-mode policy to the ONE shared carrier in `neuron::hook`
    // (the same cell the GUI drives). If the run-daemon's listener thread is live it picks this up on
    // its next `reconcile` and (de)installs the WH_KEYBOARD_LL suppression hook to match — so apply
    // works from the daemon's AppFocus->ProfileSwitch path AND the one-shot CLI `profile apply`.
    neuron::hook::set_policy(report.gaming_mode);
    if report.gaming_mode.any() {
        say!(
            "  gaming-mode -> policy active (host-side Alt+Tab/Win/Alt+F4 suppression {})",
            if neuron::hook::is_installed() {
                "hook live on the daemon listener thread"
            } else {
                "hook arms when the `run` daemon is listening"
            }
        );
    }
    if out::json() {
        out::print_json(&serde_json::json!({
            "profile": name, "summary": p.summary(),
            "applied": report.applied, "gated": report.gated, "skipped": report.skipped,
            "gaming_mode": report.gaming_mode.any(),
        }));
    }
    Ok(())
}

fn cast_run(trigger_override: Option<neuron::controls::ControlRef>) {
    let cfg = CastConfig::load();
    let vault = Vault::load();
    let trigger = trigger_override.unwrap_or(cfg.trigger);
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
        Some(LightingCmd::Catalog) => light::catalog(),
        Some(LightingCmd::Stack { action }) => light::stack(action),
        Some(LightingCmd::Saved { action }) => light::effect(action),
        Some(LightingCmd::Apply { profile }) => profile_apply(reg, &profile),
        Some(LightingCmd::Fps { pid, value }) => light::fps(&pid, value),
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
                    .unwrap_or("<new>")
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
    let logs = synapse_mapping_logs(&roots);
    if !logs.is_empty() {
        println!("\nSynapse 3 button layouts, as its service last logged them (preview with `neuron import FILE`):");
        for l in &logs {
            println!("  {}", l.display());
        }
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
        println!("\n(`neuron import --deep` prints sample values; `neuron import FILE --apply` writes an export or a mapping log.)");
    }
}

/// Synapse 3's per-device mapping logs (`Synapse3\Log\*Mapping*.log`) under the vendor roots: the
/// only readable record of a layout whose profile lives in the encrypted account cache.
fn synapse_mapping_logs(roots: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = roots
        .iter()
        .filter_map(|r| std::fs::read_dir(r.join("Synapse3").join("Log")).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.contains("Mapping") && p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
        })
        .collect();
    out.sort();
    out
}

/// Print every settings report a Razer mouse pushes by itself, for `seconds`: the `05` family
/// (`05 02` DPI, `05 0e` side plate by strap-code) and the `04` deferred buttons. The same
/// collections the app's event reader listens on; read-only, and safe beside a running app.
fn watch_device_cmd(reg: &Registry, seconds: u64) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let infos = transport::enumerate()?;
    let pipes: Vec<_> = infos
        .into_iter()
        .filter(|i| i.vid == neuron::synth::RAZER_VID)
        .filter(|i| (i.usage_page == 0x0001 && i.usage == 0x0000) || i.usage_page >= 0xFF00)
        .collect();
    if pipes.is_empty() {
        bail!("no Razer event collection to listen on");
    }
    println!("listening {seconds}s on {} collection(s); swap a plate or press DPI (Ctrl+C stops)", pipes.len());
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for info in pipes {
        let def = reg.devices.iter().find(|d| d.vendor_id == info.vid && d.product_ids().any(|p| p == info.pid)).cloned();
        let stop = stop.clone();
        threads.push(std::thread::spawn(move || {
            let Ok(reader) = transport::open_reader(&info.path) else { return };
            let mut buf = [0u8; 91];
            while !stop.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(Some(n)) if n >= 2 => {
                        let b = &buf[..n];
                        // Windows hands the report without its id byte on these collections.
                        let meaning = match (b[0], b[1]) {
                            (0x05, 0x0e) => {
                                let id = b.get(2).copied().unwrap_or(0);
                                let label = if id == 0 {
                                    "detached".to_string()
                                } else {
                                    def.as_ref().and_then(|d| d.side_plate_label(id)).map_or_else(|| "not in the device's [side_plates]".to_string(), str::to_string)
                                };
                                format!("side plate: strap-code {id} -> {label}")
                            }
                            (0x05, 0x02) if n >= 4 => format!("dpi {}", u16::from_be_bytes([b[2], b[3]])),
                            (0x04, 0x00) => "deferred buttons released".to_string(),
                            (0x04, c) => format!("deferred button {c:#04x}"),
                            _ => String::new(),
                        };
                        let hex: Vec<String> = b.iter().take(8).map(|x| format!("{x:02x}")).collect();
                        println!("  {:04x}  {:<24} {meaning}", info.pid, hex.join(" "));
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    Ok(())
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
        neuron::profile::Profile::write_rules(&imported.profile.name, toml::to_string_pretty(&doc)?.as_bytes(), !imported.profile.is_empty())
            .map_err(anyhow::Error::msg)
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

fn parse_hex16(s: &str) -> Result<u16> {
    u16::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|_| anyhow::anyhow!("'{s}' is not hex. Use digits and a-f, like 00a8."))
}

/// A parsed `--mute on|off|toggle` request — the pure decode shared by `audio mic` and `audio out`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(windows, test))]
enum MuteAction {
    On,
    Off,
    Toggle,
}

/// Parse a `--mute` value string into a [`MuteAction`]. Accepts the friendly synonyms the CLI has
/// always taken (on/true/1/mute, off/false/0/unmute, toggle). Pure + case-insensitive, so the whole
/// accepted-vocabulary is unit-testable without touching Core Audio.
#[cfg(any(windows, test))]
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
        return out::emit(&serde_json::json!({ "stages": cur, "active_index": act, "active": act + 1 }), || {
            if cur.is_empty() {
                println!("DPI stages: (none reported)");
            } else {
                let list: Vec<String> = cur
                    .iter()
                    .enumerate()
                    .map(|(i, v)| if i as u8 == act { format!("[{v}]") } else { v.to_string() })
                    .collect();
                println!("DPI stages: {}  (active marked, {} stage(s))", list.join(" "), cur.len());
            }
        });
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
    say!(
        "setting DPI stages [{}] active {} (live + onboard)...",
        list.join("/"),
        active,
    );
    // VISITOR discipline: `set_dpi_stages` flips to driver mode INTERNALLY (no prior returned to us),
    // so read the mode BEFORE and restore it after by the same rule — a one-shot CLI never leaves the
    // driver lease held (the `dpi_trap` self-poisoning loop). `unwrap_or(0)` = treat an unanswered
    // getter as non-driver, matching `ensure_custody`'s own "flip when unsure".
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
    say!("  done — write verified against the device's stage-table read-back.");
    if out::json() {
        out::print_json(&serde_json::json!({ "stages": stages, "active": active, "verified": true, "planes": ["live", "onboard"] }));
    }
    Ok(())
}

/// Sensor lift-off distance: read the current, or write a symmetric level / asymmetric split.
/// Every write read-back-verifies (bails on mismatch, never a false success) — the point here is
/// to HARDWARE-CONFIRM the asymmetric path on the Naga via the shared `0x0B/0x85` getter.
fn lod_cmd(reg: &Registry, lift: Option<u8>, landing: Option<u8>, sym: Option<u8>) -> Result<()> {
    // The DPI-capable mouse (the Naga); its sensor class rides the same control interface.
    let d = open_with_command(reg, "dpi")?;
    let show = |d: &Device| match writes::lift_off_async(d) {
        Some((lf, la)) => say!("  LOD now: ASYMMETRIC — lift {lf} / landing {la}"),
        None => say!(
            "  LOD now: symmetric level {}",
            writes::lift_off_distance(d).map_or_else(|_| "?".into(), |v| v.to_string())
        ),
    };
    let state = |d: &Device| match writes::lift_off_async(d) {
        Some((lf, la)) => serde_json::json!({ "mode": "asymmetric", "lift": lf, "landing": la }),
        None => serde_json::json!({ "mode": "symmetric", "level": writes::lift_off_distance(d).ok() }),
    };
    say!("current lift-off state:");
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
            let prior = ensure_custody(&d);
            say!("setting SYMMETRIC lift-off level {level}...");
            let res = writes::set_lift_off_distance(&d, level);
            restore_custody_if_visitor(&d, prior);
            res?;
            say!("  ACCEPTED + read-back VERIFIED (0x0B/0x85 echoed mode=symmetric + level).");
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
            let prior = ensure_custody(&d);
            say!("setting ASYMMETRIC lift-off: lift {lf} / landing {la}...");
            // The write itself re-reads 0x0B/0x85 and bails unless the device echoes
            // mode=async + this exact lift/landing pair — so reaching the line below IS the
            // hardware round-trip.
            let res = writes::set_lift_off_asymmetric(&d, lf, la);
            restore_custody_if_visitor(&d, prior);
            res?;
            say!("  ACCEPTED + read-back VERIFIED on the shared 0x0B/0x85 getter — hardware round-trip.");
            show(&d);
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            bail!("an asymmetric split needs BOTH --lift and --landing (e.g. `neuron lod --lift 12 --landing 3`)");
        }
        (None, None, None) => {
            say!("(pass --lift N --landing M to set a split, or --sym 0|1|2 to set/restore symmetric)");
        }
    }
    if out::json() {
        out::print_json(&serde_json::json!({ "lod": state(&d), "verified": sym.is_some() || lift.is_some() }));
    }
    Ok(())
}

/// Select the active `HyperScroll` wheel stage via the wire-confirmed `set_scroll_stage` write
/// (class 0x15/0x00). With no value, reads the active stage and enabled count (0x15/0x80, 0x15/0x81).
fn scroll_cmd(reg: &Registry, stage: Option<u8>, volatile: bool) -> Result<()> {
    let d = open_with_command(reg, "set_scroll_stage")?;
    match stage {
        None => {
            let store = cap::Store::Volatile;
            let stage = writes::scroll_stage(&d, store)?;
            let count = writes::scroll_stage_count(&d, store)?;
            return out::emit(&serde_json::json!({ "stage": stage, "count": count }), || {
                println!("scroll stage {stage} of {count} enabled");
                println!("  (pass a 1-based stage to select it, e.g. `neuron scroll 2`.)");
            });
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
            say!(
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
            say!("  done — scroll stage {s} active.");
            if out::json() {
                out::print_json(&serde_json::json!({ "stage": s, "persisted": !volatile }));
            }
        }
    }
    Ok(())
}

/// Drive every pad's motors: one setting for `ms`, or a tour of each motor at three strengths.
fn rumble_cmd(r: neuron::haptics::Rumble, ms: u64, tour: bool) -> Result<()> {
    use neuron::haptics::Rumble;
    neuron::pad::start_platform_sources();
    // Pads publish their sticks from the first report; give sources a moment to see them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut pads = Vec::new();
    while pads.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        pads = neuron::haptics::devices();
    }
    if pads.is_empty() {
        bail!("no pad with motors found");
    }
    let steps: Vec<(Rumble, u64)> = if tour {
        let motor = |i: usize, v: f32| {
            let mut m = [0.0f32; 4];
            m[i] = v;
            Rumble { low: m[0], high: m[1], left_trigger: m[2], right_trigger: m[3] }
        };
        let mut s = Vec::new();
        for (i, name) in ["low", "high", "left trigger", "right trigger"].iter().enumerate() {
            println!("  {name}: 25% → 50% → 100%");
            for v in [0.25, 0.5, 1.0] {
                s.push((motor(i, v), 450));
                s.push((Rumble::OFF, 150));
            }
            s.push((Rumble::OFF, 500));
        }
        s
    } else {
        vec![(r, ms)]
    };
    let total: u64 = steps.iter().map(|(_, ms)| ms).sum();
    for p in &pads {
        println!("rumble {p}");
        neuron::haptics::play(p, steps.clone());
    }
    std::thread::sleep(std::time::Duration::from_millis(total + 200));
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
                // An explicit getter prints whatever answered: an all-zero reply is a real value
                // (device mode 0x00 = normal), only the sweep below drops empties as noise.
                if let Some(a) = discover::exec(&*t, tid, c, i, 0x20, &[]) {
                    dump(info.usage_page, info.usage, c, i, &a);
                    any = true;
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
                println!("  {c:02X}/{i:02X} gave no valid reply (unsupported, unavailable, or busy)");
            }
            None => println!("  (no getters responded)"),
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

#[cfg(windows)]
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
        neuron::action::arm_input(true);
        neuron::writes::set_writes_paused(false);
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

#[cfg(not(windows))]
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
#[cfg(windows)]
fn run_intent(devices: &mut DeviceSession<'_>, intent: &neuron::action::Intent) -> String {
    run_intent_recording(devices, intent, true)
}

#[cfg(windows)]
fn run_intent_recording(devices: &mut DeviceSession<'_>, intent: &neuron::action::Intent, record_undo: bool) -> String {
    let mut cursor = CliProfileCursor;
    let before = cursor.active_profile();
    neuron::intent::run_shared_intent_observe(
        devices,
        &mut cursor,
        intent,
        neuron::dpi_origin::Cause::UserApplied,
        |report| {
            if record_undo && matches!(intent, neuron::action::Intent::ProfileSwitch(_) | neuron::action::Intent::ProfileCycle(_)) {
                let applied = daemon_active_profile();
                if let Some(entry) = neuron::session_undo::verified_profile_entry(before.clone(), applied, &report.skipped, &report.gated) {
                    neuron::session_undo::push(entry);
                }
            }
        },
    )
        .unwrap_or_else(|| "app intent needs the resident app - run neuron-app".into())
}

#[cfg(windows)]
struct CliProfileCursor;

#[cfg(windows)]
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
#[cfg(windows)]
fn daemon_active_profile() -> String {
    neuron::profile::active()
}
#[cfg(windows)]
fn set_daemon_active_profile(name: &str) {
    neuron::profile::set_active(name);
}

/// Fire one [`Trigger`] through the unified [`Engine`] and carry out the result: run every matching
/// rule's host action, route any daemon [`Intent`] (DPI / scroll / profile) to real device/profile
/// state, and drive a held-down [`Action::Turbo`] autofire while the trigger stays down. This is
/// the single dispatch entry the run-daemon uses for buttons, the mic tap, gestures, radial flicks
/// and app focus alike — the "one dispatcher" made live.
#[cfg(windows)]
struct CliIntentRunner<'a, 'reg> {
    devices: &'a mut DeviceSession<'reg>,
}

#[cfg(windows)]
impl IntentRunner for CliIntentRunner<'_, '_> {
    fn run_intent(&mut self, intent: &neuron::action::Intent) -> String {
        if matches!(intent, neuron::action::Intent::Undo) {
            undo_latest(self.devices)
        } else {
            run_intent(self.devices, intent)
        }
    }

}

#[cfg(windows)]
fn undo_latest(devices: &mut DeviceSession<'_>) -> String {
    use neuron::session_undo::{AudioValue, Entry};
    if !neuron::safety::input_armed() { return "undo is disarmed".into(); }
    let Some(record) = neuron::session_undo::peek() else { return "undo: nothing to restore".into(); };
    let result = match &record.entry {
        Entry::Audio { id, flow: _, applied, .. } => {
            let Some(ctl) = neuron::audio::VolumeCtl::open(id) else { return format!("undo: audio endpoint '{id}' unavailable"); };
            let read = || match applied {
                AudioValue::Volume(_) => ctl.try_get_volume().map(AudioValue::Volume).map(neuron::session_undo::State::Audio),
                AudioValue::Mute(_) => ctl.try_get_mute().map(AudioValue::Mute).map(neuron::session_undo::State::Audio),
            }.ok_or_else(|| format!("audio endpoint '{id}' read failed"));
            neuron::session_undo::restore_transaction(&record.entry, read, |state| {
                if !neuron::safety::input_armed() { return Err("undo is disarmed".into()); }
                let neuron::session_undo::State::Audio(value) = state else { return Err("undo entry is not audio".into()); };
                let ok = match value { AudioValue::Volume(v) => ctl.set_volume(*v), AudioValue::Mute(v) => ctl.set_mute(*v) };
                if ok { Ok(()) } else { Err(format!("audio endpoint '{id}' restore failed")) }
            }).map_err(|e| format!("undo: audio endpoint '{id}': {e}"))
        }
        Entry::Profile { before, applied } => {
            let mut cursor = CliProfileCursor;
            if cursor.active_profile() != *applied { return format!("undo: active profile changed since '{applied}'"); }
            let intent = neuron::action::Intent::ProfileSwitch(before.clone());
            let status = neuron::intent::run_shared_intent(devices, &mut cursor, &intent, neuron::dpi_origin::Cause::UserApplied)
                .unwrap_or_else(|| "undo: profile restore unavailable".into());
            let unresolved = neuron::profile::active_missing();
            if neuron::session_undo::profile_restore_complete(&cursor.active_profile(), before, &unresolved) { Ok(()) }
            else if !unresolved.is_empty() { Err(format!("undo: profile restore incomplete ({status}); unresolved: {}", unresolved.join(", "))) }
            else { Err(format!("undo: profile restore failed ({status}); active profile is '{}'", cursor.active_profile())) }
        }
        Entry::Lighting { .. } => Err("undo: lighting restore unavailable in the daemon".into()),
    };
    match result {
        Ok(()) => { neuron::session_undo::complete(record.id); "undo restored the previous state".into() }
        Err(message) => message,
    }
}

#[cfg(windows)]
fn fire_trigger(
    devices: &mut DeviceSession<'_>,
    exec: &mut DispatchExecutor,
    rt: &mut neuron::controls::Runtime,
    trigger: &neuron::engine::Trigger,
) -> Option<DispatchOutcome> {
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
#[cfg(windows)]
fn run_listen(reg: &Registry, seconds: Option<u64>, rt: neuron::controls::Runtime) {
    use neuron::controls::{HoldEdges, InputEdge, MIC_TAP};
    use neuron::engine::Trigger;
    use std::cell::RefCell;
    struct SessionUndoReset;
    impl Drop for SessionUndoReset {
        fn drop(&mut self) { neuron::session_undo::clear(); }
    }
    neuron::session_undo::clear();
    let _undo_reset = SessionUndoReset;

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
    // Pads that only a platform API delivers (GameInput) feed the same pump.
    neuron::pad::start_platform_sources();

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
                    let line = run_intent_recording(
                        &mut devices.borrow_mut(),
                        &neuron::action::Intent::ProfileSwitch(name),
                        false,
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
    // measurement harness read-out: this session's pump counters (see `neuron prof pump`).
    prof_pump_cmd();
}

#[cfg(windows)]
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

#[cfg(not(windows))]
fn audio_cmd(_action: AudioCmd) -> Result<()> {
    anyhow::bail!(
        "audio endpoints are Windows-only for now: neuron reads them through Core Audio and \
         has no PipeWire/ALSA backend yet, so it can see none of yours."
    );
}

/// Poll Razer audio endpoints for volume/mute changes. The `BlackShark` knob/mute and the
/// Seiren tap surface as Core Audio changes (UAC feature-unit), not Raw Input HID — this is
/// the channel a Core-Audio-based remap listens on.
#[cfg(windows)]
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

#[cfg(windows)]
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
#[cfg(windows)]
fn resolve_mic(device: Option<&str>) -> Result<neuron::audio::Endpoint> {
    audio::resolve_capture(device).ok_or_else(|| match device {
        Some(n) => anyhow::anyhow!("no capture device matching '{n}'"),
        None => anyhow::anyhow!("no capture (microphone) endpoints found"),
    })
}

#[cfg(windows)]
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
#[cfg(windows)]
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
/// [`neuron::writes::ensure_custody`] (one driver-mode sequence, no CLI-local copy of the magic
/// bytes that could drift from core). RETURNS the PRIOR mode byte so a one-shot command can hand
/// custody back on the way out (see [`restore_custody_if_visitor`]) — a CLI is a VISITOR.
fn ensure_custody(d: &Device) -> u8 {
    neuron::writes::ensure_custody(d)
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
/// getter reads as non-driver, matching `ensure_custody`'s own "flip when unsure" assumption, so we
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
        let prior = ensure_custody(&d);
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
    let verified = value.map(|v| x == v && y == v);
    out::emit(&serde_json::json!({ "x": x, "y": y, "requested": value, "verified": verified }), || match value {
        Some(_) => println!(
            "DPI -> {x} x {y}  [{}]",
            if verified == Some(true) { "verified" } else { "MISMATCH" }
        ),
        None => println!("DPI: {x} x {y}"),
    })
}

fn polling_cmd(reg: &Registry, hz: Option<u32>) -> Result<()> {
    let d = open_with_command(reg, "polling_rate")?;
    if let Some(h) = hz {
        // VISITOR discipline: restore the found custody state on exit (success or error).
        let prior = ensure_custody(&d);
        let res = cap::set_polling_hz(&d, h);
        restore_custody_if_visitor(&d, prior);
        let target = res?;
        let got = cap::polling_rate_hz(&d)?;
        out::emit(&serde_json::json!({ "hz": got, "requested": target, "verified": got == target }), || {
            println!("polling -> {got} Hz  [{}]", if got == target { "verified" } else { "MISMATCH" });
        })
    } else {
        let hz = cap::polling_rate_hz(&d)?;
        out::emit(&serde_json::json!({ "hz": hz }), || println!("polling: {hz} Hz"))
    }
}

fn brightness_cmd(reg: &Registry, pct: Option<u8>) -> Result<()> {
    // brightness is DUAL-DIALECT (matrix top-level command vs legacy lighting-block spec), so resolve
    // it by CAPABILITY — the command-name path skipped legacy boards (the BlackWidow) that can only
    // write brightness through the lighting block.
    let capability = if pct.is_some() {
        neuron::registry::Capability::SetBrightness
    } else {
        neuron::registry::Capability::Brightness
    };
    let d = Device::open_with_capability(reg, capability)
        // MISS: adopt brand-new hardware once, retry against the fresh registry (zero-cost when
        // the device already resolves).
        .or_else(|e| match adopt_and_reload() {
            Some(reg2) => Device::open_with_capability(&reg2, capability),
            None => Err(e),
        })?;
    if let Some(p) = pct {
        // lighting writes need driver mode; ensure it (reversible) via the canonical core helper.
        // VISITOR discipline: capture the prior mode and restore it on exit — don't leave the driver
        // lease held from a one-shot command (the `dpi_trap` self-poisoning class of bug).
        let prior = neuron::writes::ensure_custody(&d);
        let res = cap::set_brightness(&d, p, cap::Store::Persist);
        restore_custody_if_visitor(&d, prior);
        res?;
    }
    let got = cap::brightness_percent(&d)?;
    let verified = pct.map(|p| got.abs_diff(p) <= 1);
    out::emit(&serde_json::json!({ "percent": got, "requested": pct, "verified": verified }), || match pct {
        Some(_) => println!("brightness -> {got}%  [{}]", if verified == Some(true) { "verified" } else { "MISMATCH" }),
        None => println!("brightness: {got}%"),
    })
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
    let on = cap::game_mode(&d)?;
    out::emit(&serde_json::json!({ "game_mode": on, "requested": want, "verified": want.map(|w| w == on) }), || {
        if on {
            println!("game mode: ON — the keyboard is eating the Win key in firmware");
        } else {
            println!("game mode: off");
        }
    })
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

fn gesture_record(name: &str, trigger: neuron::controls::ControlRef) -> Result<()> {
    let mut vault = Vault::load();
    println!("Recording '{name}'. HOLD {}, draw, release (ESC aborts)...", trigger.label());
    let stroke = glyph::capture_held(trigger, 8192);
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

fn gesture_match(trigger: neuron::controls::ControlRef) -> Result<()> {
    let vault = Vault::load();
    if vault.templates.is_empty() {
        println!("vault is empty — record gestures first: neuron gesture record <name>");
        return Ok(());
    }
    println!("HOLD {}, draw, release...", trigger.label());
    let stroke = glyph::capture_held(trigger, 8192);
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

/// Connected devices no registry def covers yet: `(pid, name)`. Reads report these and leave the
/// learning to `neuron adopt`, so a read verb never writes `devices/auto/`.
fn unknown_devices(reg: &Registry) -> Vec<(u16, String)> {
    let pids = neuron::synth::unknown_present(reg);
    if pids.is_empty() {
        return Vec::new();
    }
    let infos = transport::enumerate().unwrap_or_default();
    pids.into_iter()
        .map(|pid| {
            let name = infos.iter().find(|i| i.pid == pid && !i.product.is_empty()).map_or_else(|| format!("Razer device {pid:04x}"), |i| i.product.clone());
            (pid, name)
        })
        .collect()
}

fn list(reg: &Registry) -> Result<()> {
    let infos = transport::enumerate()?;
    let mut rows = Vec::new();
    for i in &infos {
        // find_for_pipe: one line per DRIVEN control pipe — on a two-family pid each pipe lists
        // under the family that frames it, rather than both collapsing to find_by_pid's first def.
        if let Some(def) = reg.find_for_pipe(i) {
            let mode = def.mode_for(i.pid).map_or("?", |m| m.name.as_str());
            rows.push(serde_json::json!({ "name": def.name, "codename": def.codename, "pid": format!("{:04x}", i.pid), "mode": mode }));
        }
    }
    let unknown = unknown_devices(reg);
    let unknown_json: Vec<_> = unknown.iter().map(|(pid, name)| serde_json::json!({ "name": name, "pid": format!("{pid:04x}") })).collect();
    out::emit(&serde_json::json!({ "devices": rows, "unknown": unknown_json }), || {
        if rows.is_empty() {
            println!("No recognized Razer devices found.");
        }
        for (pid, name) in &unknown {
            println!("new device {name} (pid {pid:04x}): run `neuron adopt` to learn it");
        }
        for r in &rows {
            println!(
                "{}  [{}]  pid={}  mode={}",
                r["name"].as_str().unwrap_or(""),
                r["codename"].as_str().unwrap_or(""),
                r["pid"].as_str().unwrap_or(""),
                r["mode"].as_str().unwrap_or("")
            );
        }
    })
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

/// The device `info` describes: the one `--pid` names, else the first recognized one. Returns the
/// device with `(its position, how many are connected)`.
fn open_for_info(reg: &Registry, pid: Option<&str>) -> Result<(Device, usize, usize)> {
    let want = pid.map(parse_pid).transpose()?;
    let infos = transport::enumerate()?;
    // one entry per driven control pipe, deduplicated by (pid, family) like `list`
    let mut found: Vec<(&neuron::transport::HidDeviceInfo, &DeviceDef)> = Vec::new();
    for i in &infos {
        if let Some(def) = reg.find_for_pipe(i) {
            if !found.iter().any(|(j, d)| j.pid == i.pid && d.dialect == def.dialect) {
                found.push((i, def));
            }
        }
    }
    if found.is_empty() {
        let unknown = unknown_devices(reg);
        if let Some((pid, name)) = unknown.first() {
            bail!("{name} (pid {pid:04x}) is not known yet: run `neuron adopt` to learn it");
        }
        bail!("no recognized Razer device connected");
    }
    let total = found.len();
    let idx = match want {
        Some(w) => found.iter().position(|(i, _)| i.pid == w).ok_or_else(|| anyhow::anyhow!("no connected device has pid {w:04x} (see `neuron list`)"))?,
        None => 0,
    };
    let (i, def) = found[idx];
    Ok((Device::open(def.clone(), i.pid)?, idx + 1, total))
}

/// A device pid typed as hex, with or without `0x`.
fn parse_pid(s: &str) -> Result<u16> {
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).map_err(|_| anyhow::anyhow!("'{s}' is not a device pid. Use hex, like 00a8 (see `neuron list`)."))
}

fn info(d: &Device, pos: usize, total: usize) -> Result<()> {
    let mut v = serde_json::json!({ "name": d.def.name, "codename": d.def.codename, "pid": format!("{:04x}", d.pid) });
    let mut lines = vec![format!("{} [{}]", d.def.name, d.def.codename), format!("  pid:      {:04x}", d.pid)];
    if total > 1 {
        v["device"] = serde_json::json!({ "index": pos, "of": total });
        let _ = write!(lines[0], "  ({pos} of {total} devices; --pid to pick)");
    }
    if let Ok(fw) = cap::firmware(d) {
        v["firmware"] = fw.clone().into();
        lines.push(format!("  firmware: v{fw}"));
    }
    if let Ok(m) = cap::device_mode(d) {
        v["mode"] = m.into();
        lines.push(format!("  mode:     0x{m:02x}"));
    }
    if let Ok((x, y)) = cap::dpi(d) {
        v["dpi"] = serde_json::json!({ "x": x, "y": y });
        lines.push(format!("  dpi:      {x} x {y}"));
    }
    if let Ok(hz) = cap::polling_rate_hz(d) {
        v["polling_hz"] = hz.into();
        lines.push(format!("  polling:  {hz} Hz"));
    }
    if let Ok(br) = cap::brightness_percent(d) {
        v["brightness"] = br.into();
        lines.push(format!("  lighting: {br}% brightness"));
    }
    if let Ok(b) = cap::battery_percent(d) {
        let c = cap::charging(d).unwrap_or(false);
        v["battery"] = serde_json::json!({ "percent": b, "charging": c });
        lines.push(format!("  battery:  {b}%{}", if c { " (charging)" } else { "" }));
    }
    if let Ok(s) = cap::storage(d) {
        v["onboard"] = serde_json::json!({ "percent_free": s.pct_remaining(), "free_kb": s.free_bytes() / 1024, "max_kb": s.max_bytes / 1024, "macro_slots": s.max_macros });
        lines.push(format!(
            "  onboard:  {}% free ({} KB of {} KB, {} macro slots)",
            s.pct_remaining(),
            s.free_bytes() / 1024,
            s.max_bytes / 1024,
            s.max_macros
        ));
    }
    out::emit(&v, || {
        for l in &lines {
            println!("{l}");
        }
    })
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

    #[test]
    fn help_is_grouped_one_line_each_and_hides_the_harnesses() {
        let cmd = Cli::command();
        let grouped: std::collections::BTreeSet<&str> = HELP_GROUPS.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        for s in cmd.get_subcommands() {
            let name = s.get_name();
            if s.is_hide_set() {
                assert!(!grouped.contains(name), "{name} is hidden but listed in a help group");
                continue;
            }
            assert!(grouped.contains(name), "{name} is visible but in no help group");
            let about = s.get_about().map(ToString::to_string).unwrap_or_default();
            assert!(!about.is_empty() && !about.contains('\n') && about.chars().count() <= 70, "{name}: {about:?}");
        }
        for n in &grouped {
            assert!(cmd.find_subcommand(n).is_some(), "group lists unknown command {n}");
        }
        for n in ["prof", "probe", "discover", "dpi", "polling", "dpi-stages", "scroll", "lod", "brightness", "game-mode", "sniper", "idle", "import-export", "remap"] {
            assert!(cmd.find_subcommand(n).is_some_and(clap::Command::is_hide_set), "{n} should be hidden");
        }
    }

    #[test]
    fn rust_parse_errors_become_plain_words() {
        assert_eq!(human_usage_line("invalid value 'abc' for '[VALUE]': invalid digit found in string"), "'abc' is not valid for VALUE: expected a whole number.");
        assert_eq!(human_usage_line("invalid value '99999' for '[VALUE]': 99999 is not in 0..=65535"), "'99999' is not valid for VALUE: must be between 0 and 65535.");
        assert_eq!(human_usage_line("unrecognized subcommand 'x'"), "unrecognized subcommand 'x'");
    }

    #[test]
    fn sniper_takes_trigger_and_still_accepts_bind() {
        for flag in ["--trigger", "--bind"] {
            let c = Cli::try_parse_from(["neuron", "sniper", flag, "mouse:5", "--dpi", "400"]).unwrap();
            assert!(matches!(c.cmd, Cmd::Sniper(_)));
        }
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
