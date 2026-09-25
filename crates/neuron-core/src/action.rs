// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The unified action model — what any input source (control event, gesture, radial flick)
//! resolves to. Deliberately serde-tagged and self-contained so it is the contract between
//! the headless engine and ANY client — the CLI today and the in-process Slint GUI, both of which
//! `use neuron::*` directly (one process, no internal IPC): config is data, execution is here.

use crate::macros::context::Context;
use serde::{Deserialize, Serialize};

/// One thing Neuron can do in response to an input. `type` selects the variant in TOML/JSON,
/// e.g. `{ type = "key", key = "f" }` or `{ type = "run", cmd = "obs --start" }`.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Do nothing (an unbound wedge / placeholder).
    #[default]
    Noop,
    /// Run a shell command line.
    Run { cmd: String },
    /// Synthesize a keypress (down+up). `key` is a name like "f", "1", "enter", "f5", "space".
    /// This is the comms-wheel killer: flick a direction -> press a game keybind.
    Key { key: String },
    /// Set the mic's mute: mode = "toggle" | "on" | "off".
    MicMute {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default = "toggle")]
        mode: String,
    },
    /// Nudge the mic gain by +/- percentage points.
    MicGain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        delta_pct: f32,
    },
    /// Set the mic gain to an absolute percentage.
    MicGainSet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        pct: f32,
    },
    /// Set an OUTPUT endpoint's mute (headset / sound card / speakers): mode = "toggle"|"on"|"off".
    /// `device` is an optional name substring; default = the system's active output. Generic over
    /// any render endpoint the OS exposes — not tied to a specific Razer device.
    OutputMute {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default = "toggle")]
        mode: String,
    },
    /// Nudge an OUTPUT endpoint's volume by +/- percentage points (headphones / sound card).
    OutputGain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        delta_pct: f32,
    },
    /// A timed list of steps — the foundation of a macro. Each step runs an action, optionally
    /// holding it for `hold_ms` and pausing `delay_ms` before the next. This is how
    /// `bindings.rs` + `cast.rs` + the run-daemon collapse into one spine: a macro is just an
    /// `Action`. Boxed inner actions so the enum stays a fixed, small size.
    ///
    /// A struct variant (not a newtype) because `Action` is internally tagged
    /// (`#[serde(tag = "type")]`) and serde can't serialize a tagged newtype wrapping a
    /// sequence — `{ type = "sequence", steps = [...] }`.
    Sequence { steps: Vec<Step> },
    /// Run a stored, named script — the power tier. The body lives out-of-band (compiled native
    /// Rust artifact, or a shell/file path), referenced by id so the serialized config stays
    /// small and stable. The macro engine (`macros/`) resolves + executes the `ScriptRef`.
    /// Flattened struct variant for the same internal-tagging reason as `Sequence`.
    Script {
        #[serde(flatten)]
        script: ScriptRef,
    },
    /// Synthesize a mouse button event (down+up) via `SendInput`. The comms-wheel / remap surface
    /// needs real mouse buttons as *outputs* (e.g. a thumb wedge that middle-clicks, or a gesture
    /// that fires Back). `kind` selects which button; scroll wheel "clicks" emit a notch of
    /// vertical/horizontal wheel motion.
    MouseButton { button: MouseButtonKind },
    /// Synthesize a media / volume transport key (Play-Pause, Next, Vol±, Mute, …) via the media
    /// virtual-keys. The clean home for what the importer used to encode as `Key { key:
    /// "media-play-pause" }` — a first-class, typed variant.
    Media { key: MediaKind },
    /// **Daemon intent** — cycle the mouse DPI stage list up or down, FROM THE CURRENT DPI. The
    /// `Action` has no device handle, so `run()` cannot itself talk to the mouse; instead this
    /// variant carries a typed [`Intent`] (see [`Action::intent`]) the run-daemon reads and
    /// dispatches against the live device (via `capability::set_dpi` over the current stage list).
    /// The cycle is a *delta on the current value*, not a flat index — see the cycle contract on
    /// [`Intent`]. `run()` is a no-op that reports the intent for logging/dry-run.
    DpiCycle { dir: Direction },
    /// **Daemon intent** — set the mouse DPI to an absolute value. Daemon-handled like
    /// [`Action::DpiCycle`] (no device handle at the `Action` layer).
    DpiSet { dpi: u16 },
    /// **Daemon intent** — request a `HyperScroll` scroll-wheel stage cycle. The confirmed device
    /// command is currently set-only (`writes::set_scroll_stage`); there is no active-stage getter,
    /// so resident runtimes must keep an explicit cursor before this can be a true cycle.
    ScrollStageCycle { dir: Direction },
    /// **Daemon intent** — switch to a named Neuron profile. Daemon-handled: the run-daemon loads
    /// the [`crate::profile::Profile`] and applies it. `run()` reports the intent.
    ProfileSwitch { name: String },
    /// **Daemon intent** — cycle to the next/previous saved profile (by sorted name order), FROM
    /// THE CURRENTLY-ACTIVE profile (a delta on the current selection, not a flat
    /// `Up->[1]`/`Down->[last]` index — see the cycle contract on [`Intent`]). Daemon-handled.
    ProfileCycle { dir: Direction },
    /// Turbo / autofire: repeat the inner `action` at `cps` (clicks-per-second) **while the
    /// trigger is held**. The repetition is the *daemon's* job — it owns the held-state edges and
    /// the timer — so this is a daemon-cooperative variant: the daemon reads `cps` (via
    /// [`Action::turbo`]) and re-fires the inner action on an interval until the trigger releases.
    /// A one-shot `run()` fires the inner action exactly ONCE (a sensible no-daemon fallback / the
    /// "single press" of an autofire). Boxed inner action so the enum stays a small fixed size.
    ///
    /// Chosen over a `turbo: Option<u16>` field on a binding because the spine is Action-centric
    /// (a `Trigger -> Action` `Rule`); modelling autofire as an `Action` keeps turbo composable
    /// (any action can be turbo'd, including a `Sequence`) and keeps `Rule`/`Trigger` unchanged.
    Turbo { action: Box<Action>, cps: u16 },
    /// **App intent** — prime the TELEPORT instrument (the monitor mini-map opens on the next
    /// hold of the cast trigger). Handled by the resident app's weave service; the CLI daemon
    /// reports it honestly instead (no overlay surface there).
    Teleport,
    /// **App intent** — open a WHITEBOARD session. Same routing contract as [`Action::Teleport`].
    Whiteboard,
    /// **App intent** — enter KNOCKBACK, the rhythm familiar: a lightweight, non-invasive
    /// AFK duet you drum on the mouse while you wait (queue, respawn timer, idle). Same
    /// routing contract as [`Action::Teleport`] — primes the session for the next hold; the
    /// CLI daemon reports it honestly (the overlay surface is the resident app's).
    Knockback,
    /// **App intent** — GLANCE: toggle a live peek portal (DWM thumbnail near the cursor) at the
    /// first window whose title or exe contains `target`. Focus never moves — watch a build, a
    /// chat, a render from inside anything. Same routing contract as [`Action::Teleport`].
    Glance { target: String },
    /// **App intent** — SUMMON the window matching `window` (title/exe substring, like Glance):
    /// `mode` decides whether it comes to your hand, surfaces where it is, or toggles.
    Summon { window: String, mode: SummonMode },
    /// **App intent** — BANISH (minimise) a window out of your way; `pick` chooses which.
    Banish { pick: WindowPick },
    /// **App intent** — PIN a window always-on-top (toggle); `pick` chooses which.
    Pin { pick: WindowPick },
    /// **App intent** — KILL: force-TERMINATE the process owning the picked window (not a polite
    /// quit — `TerminateProcess`, for a hung app). `pick` chooses which window; gated by the arm
    /// switch (safe-mode suppresses it) since it's destructive.
    Kill { pick: WindowPick },
    /// **App intent** — ECHO: replay the LAST action Neuron fired, whatever it was (a keybind, a
    /// macro, a window verb — its full value, re-run). The repeat key for "do that again".
    Echo,
    /// **App intent** — TETHER, the warpstone: ONE intuitive button. In the default `Mark` mode,
    /// press with nothing tethered → drop the stone at the current window+spot; press while away →
    /// warp back to it; press while already on it → release it. `slot` names the stone (blank = the
    /// default; bind more for more stones). In `Wormhole` mode the slot holds TWO anchors and a
    /// press SWAPS A⇄B (a portal between two fixed places, not arbitrary→one); see [`TetherMode`].
    Tether {
        slot: String,
        #[serde(default)]
        mode: TetherMode,
    },
    /// GHOST-PASTE — type the current clipboard as real keystrokes (the Ctrl+V that works in
    /// game chats, RDP, VMs, license fields). `speed` is the ghost-writing pace; default is
    /// borderline-instant. Runs on its own thread so it never blocks the dispatch tick.
    GhostPaste { speed: GhostSpeed },
    /// MOMENTARY MIC — push-to-talk / push-to-mute at the endpoint, for a HELD trigger: while
    /// held the mic flips (mode decides which way), on release it returns. Universal, zero
    /// per-app setup. (Bind to a button-hold; a one-shot trigger has nothing to release.)
    MomentaryMic {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default)]
        mode: MomentaryMode,
    },
    /// SNIPER — hold-to-precision DPI, for a HELD trigger: while held the mouse drops to `dpi`
    /// (a VOLATILE write — never flashed onboard), on release it snaps back to whatever it was.
    /// Edge-driven like [`Action::MomentaryMic`]: the resident dispatch loop owns the drop on DOWN
    /// and the restore on UP (a one-shot trigger has nothing to release). Bind to a button/key hold.
    Sniper {
        #[serde(default)]
        dpi: u16,
    },
    /// OUTPUT FLIP — switch the default audio output device. `devices` (name substrings) is the
    /// set you cycle through (remembered by name, so a disconnected one is simply skipped);
    /// empty = cycle every connected render endpoint. Headset ⇄ speakers in one press.
    OutputFlip {
        #[serde(default)]
        devices: Vec<String>,
    },
    /// **App intent** — DIAL: prime an analog knob for `target`. The next hold of the cast
    /// trigger becomes a slide (the eigenmotion stroke turns it: up/right = more, fast = coarse,
    /// slow = fine). Handled by the resident app's weave service.
    Dial { target: DialTarget },
    /// **App intent** — CONTROL CENTER: prime the system-state glance (the next hold opens it).
    /// A glanceable read of "wtf is my internet / am i on ethernet / what's my output" plus a
    /// quick bluetooth toggle seam. Same routing contract as [`Action::Teleport`].
    Control,
    /// POCKET — a portable clipboard. Activating MOVES content between the OS clipboard and a named
    /// register: clipboard full → stash it; clipboard empty → restore it; both full → swap. Carries
    /// EVERY clipboard format (text/images/files/…), not just text. `slot` is the identity (two
    /// binds to the same name share one pocket; empty = the default pocket); `persist` makes it
    /// durable on disk. A pure host action — see [`crate::pocket`].
    Pocket {
        #[serde(default)]
        slot: String,
        #[serde(default, skip_serializing_if = "is_false")]
        persist: bool,
    },
    /// CURTAIN — a panic privacy screen. **Not** a power action: it never touches DPMS / the monitor
    /// power state. It throws one opaque-black, topmost window across the whole virtual desktop so the
    /// screen content is hidden INSTANTLY and RELIABLY, then the first key/click reveals everything
    /// with zero latency (nothing was powered down — no link re-train, no window reshuffle, no
    /// "did my PC die?" recovery). Fire-and-forget from a keybind OR a spellweaving flick: the worker
    /// fades the black in, disarms until the trigger is released, then dismisses on the next fresh
    /// key/click. A pure host overlay — no device handle, no daemon, no system call. See
    /// [`crate::curtain`].
    Curtain,
    /// LOCK the workstation (Win+L) — `LockWorkStation`. Instant, intentional (no settle); arm-gated so
    /// tests can't lock the dev's session. Pure host action.
    Lock,
    /// SLEEP / suspend the machine — `SetSuspendState` (S3 standby). Arm-gated so tests never suspend
    /// the dev's box. Pure host action.
    Sleep,
    /// OBS — drive OBS Studio as a first-class bindable action (stream / record / record-pause /
    /// replay-buffer / scene switch / input mute). Routed through the SAME [`crate::obs_hook`]
    /// seam a macro's `obs_*` verb takes, so bound keys, radial wedges, gestures, sequences, and
    /// macros are one OBS surface — and it reports "OBS not connected" honestly when the
    /// CONNECTIONS host (or its obs gate) is off, never a silent no-op. Fire-and-forget into the
    /// connection's command queue: never blocks the dispatch tick. Arm-gated: accidentally going
    /// LIVE during a verify pass is the one broadcast mistake that can't be taken back.
    Obs {
        #[serde(default)]
        op: ObsOp,
        /// The op's argument where one applies: the scene name for `scene`; the OBS input name
        /// for `mute` (blank = "Mic/Aux"); "start"/"stop" to make stream / record / replay
        /// one-directional (blank = toggle, or save for replay).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        arg: String,
    },
}

/// What an [`Action::Obs`] does. Each maps to an `obs_*` act verb (see `obs_control`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ObsOp {
    /// start / stop / toggle the stream (`arg` picks; blank = toggle)
    #[default]
    Stream,
    /// start / stop / toggle recording
    Record,
    /// pause ⇄ resume the running recording (one toggle press)
    RecordPause,
    /// the replay buffer: blank = SAVE the clip ("clip that!"); `arg` "start"/"stop" runs the buffer
    Replay,
    /// switch the program scene to `arg`
    Scene,
    /// toggle mute on the OBS input named in `arg` (blank = "Mic/Aux")
    Mute,
}

impl ObsOp {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ObsOp::Stream => "stream",
            ObsOp::Record => "record",
            ObsOp::RecordPause => "record pause",
            ObsOp::Replay => "replay",
            ObsOp::Scene => "scene",
            ObsOp::Mute => "mute",
        }
    }
}

/// What an [`Action::Dial`] turns. Each maps to a live get/set the slide drives continuously.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DialTarget {
    /// the default output's master volume
    #[default]
    OutputVolume,
    /// the mic's capture level
    MicVolume,
}

impl DialTarget {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DialTarget::OutputVolume => "volume",
            DialTarget::MicVolume => "mic",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> DialTarget {
        match s.trim().to_lowercase().as_str() {
            "mic" | "mic-volume" | "mic-vol" => DialTarget::MicVolume,
            _ => DialTarget::OutputVolume,
        }
    }
}

/// The ghost-writing pace for [`Action::GhostPaste`] — fast enough to feel like a paste, organic
/// enough (jittered per-key) not to read as a robot. INSTANT is opt-in (the most synthetic).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GhostSpeed {
    /// as fast as the keys can fire — opt-in (the most detectable)
    Instant,
    /// near-instant with a whisper of jitter — the default, reads like a quick paste
    #[default]
    Borderline,
    /// a fast human (~180 wpm)
    Fast,
    /// an unhurried human (~70 wpm) — the calmest, least flaggable
    Normal,
}

impl GhostSpeed {
    /// `(base ms per char, jitter scale ms)` — shared with the Python host's `type_ghost`. The
    /// jitter keeps the cadence from being a metronome, and it is a one-sided slow tail (see
    /// [`ghost_type`]), not a symmetric ±.
    #[must_use]
    pub fn timing(self) -> (u64, u64) {
        match self {
            GhostSpeed::Instant => (0, 0),
            GhostSpeed::Borderline => (9, 5),
            GhostSpeed::Fast => (55, 25),
            GhostSpeed::Normal => (165, 60),
        }
    }
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            GhostSpeed::Instant => "instant",
            GhostSpeed::Borderline => "borderline",
            GhostSpeed::Fast => "fast",
            GhostSpeed::Normal => "normal",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> GhostSpeed {
        match s.trim().to_lowercase().as_str() {
            "instant" => GhostSpeed::Instant,
            "fast" => GhostSpeed::Fast,
            "normal" | "slow" => GhostSpeed::Normal,
            _ => GhostSpeed::Borderline,
        }
    }
}

/// Which way [`Action::MomentaryMic`] flips while the trigger is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MomentaryMode {
    /// adaptive: held = the OPPOSITE of however the mic rests (push-to-talk if you keep it
    /// muted, push-to-mute if you keep it live) — captured at the press, restored on release
    #[default]
    Flip,
    /// push-to-talk: held = live, release = muted (for the always-muted)
    Talk,
    /// push-to-mute / cough button: held = muted, release = live (for the always-live)
    Mute,
}

impl MomentaryMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MomentaryMode::Flip => "flip",
            MomentaryMode::Talk => "talk",
            MomentaryMode::Mute => "mute",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> MomentaryMode {
        match s.trim().to_lowercase().as_str() {
            "talk" | "ptt" | "push-to-talk" => MomentaryMode::Talk,
            "mute" | "ptm" | "cough" => MomentaryMode::Mute,
            _ => MomentaryMode::Flip,
        }
    }
    /// Given the mic's rest mute-state, the `(while-held, on-release)` mute-states.
    #[must_use]
    pub fn states(self, rest_muted: bool) -> (bool, bool) {
        match self {
            MomentaryMode::Flip => (!rest_muted, rest_muted),
            MomentaryMode::Talk => (false, true),
            MomentaryMode::Mute => (true, false),
        }
    }
}

/// Which window a window-targeting action acts on. The ENUM the quick-actions pick through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WindowPick {
    /// the active window (the one you're looking at)
    #[default]
    Focused,
    /// the window under the cursor — point at it, even unfocused
    Hover,
    /// every window OCCLUDED behind another (the declutter sweep) — banish only
    Behind,
}

impl WindowPick {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            WindowPick::Focused => "focused",
            WindowPick::Hover => "hover",
            WindowPick::Behind => "behind",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> WindowPick {
        match s.trim().to_lowercase().as_str() {
            "hover" | "under" | "cursor" => WindowPick::Hover,
            "behind" | "buried" | "hidden" => WindowPick::Behind,
            _ => WindowPick::Focused,
        }
    }
}

/// How [`Action::Summon`] delivers the matched window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SummonMode {
    /// raise it where it lives (switches virtual desktop natively if it's elsewhere)
    #[default]
    Focus,
    /// bring it TO the cursor — moved + focused, cross-desktop pulled in
    Here,
    /// raise it if hidden, minimise it if it's already foreground (the quake-terminal drop)
    Toggle,
}

impl SummonMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SummonMode::Focus => "focus",
            SummonMode::Here => "here",
            SummonMode::Toggle => "toggle",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> SummonMode {
        match s.trim().to_lowercase().as_str() {
            "here" => SummonMode::Here,
            "toggle" => SummonMode::Toggle,
            _ => SummonMode::Focus,
        }
    }
}

/// How a [`Action::Tether`] behaves. `Mark` is the original warpstone (arbitrary place → the one
/// anchor, with mark/recall/release off a single press). `Wormhole` is a PORTAL between two fixed
/// anchors: the slot holds A and B, and a press swaps you between them (anchor A ⇄ anchor B),
/// independent of which virtual desktop or window you're on. Standing at neither endpoint, a press
/// goes to A (and a still-empty endpoint is captured here first — the set/rebind), so a half-built
/// wormhole completes itself by use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TetherMode {
    /// the warpstone: one press = mark / recall / release, against any place you're in.
    #[default]
    Mark,
    /// the portal: two fixed anchors, a press swaps A ⇄ B (focus + cursor).
    Wormhole,
}

impl TetherMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            TetherMode::Mark => "mark",
            TetherMode::Wormhole => "wormhole",
        }
    }
    #[must_use]
    pub fn parse(s: &str) -> TetherMode {
        match s.trim().to_lowercase().as_str() {
            "wormhole" | "portal" | "swap" => TetherMode::Wormhole,
            _ => TetherMode::Mark,
        }
    }
}

/// Which mouse button (or wheel notch) a [`Action::MouseButton`] synthesizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
    /// "Back" / X1 (button 4).
    Back,
    /// "Forward" / X2 (button 5).
    Forward,
    /// A horizontal wheel notch to the left (tilt-scroll).
    ScrollLeft,
    /// A horizontal wheel notch to the right (tilt-scroll).
    ScrollRight,
}

impl MouseButtonKind {
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MouseButtonKind::Left => "left-click",
            MouseButtonKind::Right => "right-click",
            MouseButtonKind::Middle => "middle-click",
            MouseButtonKind::Back => "back",
            MouseButtonKind::Forward => "forward",
            MouseButtonKind::ScrollLeft => "scroll-left",
            MouseButtonKind::ScrollRight => "scroll-right",
        }
    }
}

/// Which media / volume transport key a [`Action::Media`] synthesizes. These map to the Windows
/// `VK_MEDIA_*` / `VK_VOLUME_*` virtual-keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaKind {
    PlayPause,
    Stop,
    Next,
    Prev,
    VolumeUp,
    VolumeDown,
    VolumeMute,
}

impl MediaKind {
    /// The Windows virtual-key for this transport control.
    #[must_use]
    pub fn vk(self) -> u16 {
        match self {
            MediaKind::PlayPause => 0xB3,  // VK_MEDIA_PLAY_PAUSE
            MediaKind::Stop => 0xB2,       // VK_MEDIA_STOP
            MediaKind::Next => 0xB0,       // VK_MEDIA_NEXT_TRACK
            MediaKind::Prev => 0xB1,       // VK_MEDIA_PREV_TRACK
            MediaKind::VolumeUp => 0xAF,   // VK_VOLUME_UP
            MediaKind::VolumeDown => 0xAE, // VK_VOLUME_DOWN
            MediaKind::VolumeMute => 0xAD, // VK_VOLUME_MUTE
        }
    }
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::PlayPause => "play/pause",
            MediaKind::Stop => "stop",
            MediaKind::Next => "next-track",
            MediaKind::Prev => "prev-track",
            MediaKind::VolumeUp => "vol+",
            MediaKind::VolumeDown => "vol-",
            MediaKind::VolumeMute => "mute",
        }
    }
    /// Parse the friendly key-name strings the migration importer emits (`"media-play-pause"`,
    /// `"volume-up"`, …) into a typed `MediaKind`, so a `Key { key: "media-*" }` an older import
    /// produced can be recognized as a media transport key. `None` for a non-media name.
    #[must_use]
    pub fn from_key_name(name: &str) -> Option<MediaKind> {
        Some(match name {
            "media-play-pause" | "media-play" | "play-pause" => MediaKind::PlayPause,
            "media-stop" => MediaKind::Stop,
            "media-next" => MediaKind::Next,
            "media-prev" | "media-previous" => MediaKind::Prev,
            "volume-up" => MediaKind::VolumeUp,
            "volume-down" => MediaKind::VolumeDown,
            "volume-mute" => MediaKind::VolumeMute,
            _ => return None,
        })
    }
}

/// A cycle direction for the stage/profile-cycling actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Up,
    Down,
}

impl Direction {
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }
    /// +1 for `Up`, -1 for `Down` — the step the daemon applies to a stage/profile index.
    #[must_use]
    pub fn step(self) -> i32 {
        match self {
            Direction::Up => 1,
            Direction::Down => -1,
        }
    }
}

/// A typed instruction the run-daemon must carry out against live device/profile state that the
/// stateless [`Action`] layer cannot reach on its own (it has no device handle, no profile store,
/// no current-stage cursor). Actions that need the daemon return one of these from
/// [`Action::intent`]; the daemon matches on it and performs the real work (DPI/scroll writes via
/// `capability`/`writes`, profile load+apply via `profile`). Pure-host actions (`Key`, `Run`,
/// `MouseButton`, `Media`, `Mic*`, `Output*`, `Sequence`, `Script`) return `None` — they fully
/// execute in `run_ctx` and need no daemon cooperation.
///
/// ## Cycle contract — "from CURRENT", never a flat index (read this before implementing a daemon)
/// The two fully-applied *cycling* intents ([`DpiCycle`](Intent::DpiCycle) and
/// [`ProfileCycle`](Intent::ProfileCycle)) carry only a [`Direction`] — deliberately. The cycle is
/// defined **relative to the live current value**, which only the daemon can read or own (the
/// `Action` layer is stateless: no device handle, no profile-selection cursor). The daemon MUST:
/// 1. read/own the CURRENT value (current DPI from `capability::dpi`; the currently-active profile
///    name),
/// 2. find its position in the ordered list (DPI stage list / sorted profile names),
/// 3. step by [`Direction::step`] (`+1`/`-1`) — wrapping with `rem_euclid` over the list length,
/// 4. apply the value at the resulting position.
///
/// A daemon that instead maps `Direction::Up -> index 1` / `Direction::Down -> last` (a *flat*
/// per-direction index that ignores the current selection) is WRONG: it would jump to the same two
/// profiles forever instead of advancing next/previous from wherever the user is. The direction is
/// a *delta on the current cursor*, not an absolute slot. (`DpiSet`/`ProfileSwitch` are absolute and
/// carry their full target, so they need no current read.)
///
/// [`ScrollStageCycle`](Intent::ScrollStageCycle) is intentionally still an intent, but it is not
/// fully applied by the resident runtimes yet: the confirmed `HyperScroll` command is set-only and
/// the device exposes no active-stage getter, so a correct cycle needs a resident cursor/list first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Cycle the DPI stage list up/down, **from the current DPI** (see the cycle contract on
    /// [`Intent`]): read current DPI, find its stage, step by [`Direction::step`], apply that stage.
    DpiCycle(Direction),
    /// Set DPI to an absolute value (no current read needed — the value is carried).
    DpiSet(u16),
    /// Request `HyperScroll` stage cycling. The confirmed write is set-only today, so live runtimes
    /// report this as pending until they own a trustworthy active-stage cursor/list.
    ScrollStageCycle(Direction),
    /// Switch to the named profile (absolute — the target name is carried).
    ProfileSwitch(String),
    /// Cycle to the next/previous profile **from the currently-active one** (per the [`Intent`]
    /// cycle contract): not a flat `Up->[1]`/`Down->[last]` index — a delta on the current cursor.
    ProfileCycle(Direction),
    /// Prime the teleport instrument (APP-level — no device write; the resident app's weave
    /// service presents it; a daemon without that surface reports it).
    Teleport,
    /// Open a whiteboard session (APP-level, same contract as [`Intent::Teleport`]).
    Whiteboard,
    /// Enter the KNOCKBACK rhythm-familiar session (APP-level, same contract as
    /// [`Intent::Teleport`]).
    Knockback,
    /// Toggle a glance portal at the named window (APP-level, same contract as
    /// [`Intent::Teleport`]; the String is the title/exe substring).
    Glance(String),
    /// Summon a window (APP-level): the matcher + the delivery mode.
    Summon(String, SummonMode),
    /// Banish (minimise) a window (APP-level): which one.
    Banish(WindowPick),
    /// Pin a window on top (APP-level, toggle): which one.
    Pin(WindowPick),
    /// Kill (force-terminate) the picked window's process (APP-level, destructive).
    Kill(WindowPick),
    /// Echo — replay the last fired action (APP-level; the resident remembers it).
    Echo,
    /// Tether (APP-level): the slot name + the mode — `Mark` is the one intuitive
    /// mark/recall/release warpstone, `Wormhole` swaps two fixed anchors A ⇄ B.
    Tether(String, TetherMode),
    /// Dial — prime the analog knob (APP-level): what it turns.
    Dial(DialTarget),
    /// Control center — prime the system-state glance (APP-level, same contract as
    /// [`Intent::Teleport`]).
    Control,
}

/// One step of a macro `Sequence`. The inner `action` is boxed because `Action` contains
/// `Step` transitively (`Sequence(Vec<Step>)`) — boxing breaks the otherwise-infinite size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// What this step does.
    pub action: Box<Action>,
    /// Pause this many milliseconds AFTER the step, before the next one runs. 0 = no pause.
    #[serde(default)]
    pub delay_ms: u32,
    /// Hold the action for this many milliseconds (e.g. a held key) before releasing. 0 = a
    /// plain tap / fire-and-forget. (Hold semantics are realized by the executor.)
    #[serde(default)]
    pub hold_ms: u32,
}

/// A reference to a stored script body. For a [`ScriptKind::Python`] macro the `id` is the
/// registered macro name (the `macros/scripts/<id>.py` stem the Macro Host keeps warm); for Shell it
/// is the inline command line; for File it is the path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptRef {
    /// Stable identifier — the warm-macro name (Python), the command (Shell), or the path (File).
    pub id: String,
    /// How to resolve and execute the body.
    pub kind: ScriptKind,
}

/// The execution flavor of a `ScriptRef`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScriptKind {
    /// A warm Python macro run by the [`crate::macros::MacroHost`] — full unsandboxed power, real-
    /// time (registered once, called by id on each trigger). The modern power tier.
    Python,
    /// A quick shell-out — an inline command line dispatched to the OS interpreter (pwsh/bash).
    Shell,
    /// A path to an external script/executable (.ps1/.py/.exe), run by the OS.
    File,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn toggle() -> String {
    "toggle".into()
}

impl Action {
    /// A short human description (for `show`, logs, and a future GUI label).
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Action::Noop => "—".into(),
            Action::Run { cmd } => format!("run `{cmd}`"),
            Action::Key { key } => format!("press [{key}]"),
            Action::MicMute { mode, device } => {
                format!("mic mute [{mode}]{}", dev(device))
            }
            Action::MicGain { delta_pct, device } => {
                format!("mic gain {delta_pct:+}%{}", dev(device))
            }
            Action::MicGainSet { pct, device } => {
                format!("mic gain = {pct}%{}", dev(device))
            }
            Action::OutputMute { mode, device } => {
                format!("output mute [{mode}]{}", dev(device))
            }
            Action::OutputGain { delta_pct, device } => {
                format!("output vol {delta_pct:+}%{}", dev(device))
            }
            Action::Sequence { steps } => format!(
                "macro ({} step{})",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
            Action::Script { script } => {
                format!("script `{}` [{}]", script.id, script.kind.label())
            }
            Action::MouseButton { button } => format!("mouse {}", button.label()),
            Action::Media { key } => format!("media {}", key.label()),
            Action::DpiCycle { dir } => format!("DPI cycle {}", dir.label()),
            Action::DpiSet { dpi } => format!("DPI -> {dpi}"),
            Action::ScrollStageCycle { dir } => format!("scroll-stage cycle {}", dir.label()),
            Action::ProfileSwitch { name } => format!("profile -> {name}"),
            Action::ProfileCycle { dir } => format!("profile cycle {}", dir.label()),
            Action::Turbo { action, cps } => format!("turbo {cps}cps: {}", action.describe()),
            Action::Teleport => "teleport".into(),
            Action::Whiteboard => "whiteboard".into(),
            Action::Knockback => "knockback".into(),
            Action::Glance { target } => format!("glance [{target}]"),
            Action::Summon { window, mode } => format!("summon [{window}] {}", mode.label()),
            Action::Banish { pick } => format!("banish ({})", pick.label()),
            Action::Pin { pick } => format!("pin ({})", pick.label()),
            Action::Kill { pick } => format!("kill ({})", pick.label()),
            Action::Echo => "echo (replay last)".into(),
            Action::Tether { slot, mode } => {
                let m = if *mode == TetherMode::Mark {
                    String::new()
                } else {
                    format!(" {}", mode.label())
                };
                if slot.is_empty() {
                    format!("tether{m}")
                } else {
                    format!("tether [{slot}]{m}")
                }
            }
            Action::GhostPaste { speed } => format!("ghost-paste ({})", speed.label()),
            Action::MomentaryMic { mode, .. } => format!("momentary mic ({})", mode.label()),
            Action::Sniper { dpi } => format!("sniper (hold \u{2192} {dpi} DPI)"),
            Action::OutputFlip { devices } => {
                if devices.is_empty() {
                    "output flip".into()
                } else {
                    format!("output flip [{}]", devices.join(" \u{00b7} "))
                }
            }
            Action::Dial { target } => format!("dial {}", target.label()),
            Action::Control => "control center".into(),
            Action::Pocket { slot, persist } => {
                let keep = if *persist { " (keep)" } else { "" };
                if slot.is_empty() {
                    format!("pocket{keep}")
                } else {
                    format!("pocket [{slot}]{keep}")
                }
            }
            Action::Curtain => "curtain".into(),
            Action::Lock => "lock".into(),
            Action::Sleep => "sleep".into(),
            Action::Obs { op, arg } => {
                let a = arg.trim();
                match (op, a.is_empty()) {
                    (ObsOp::Scene, _) => format!("obs scene \u{2192} {a}"),
                    (ObsOp::Mute, true) => "obs mute [Mic/Aux]".into(),
                    (ObsOp::Mute, false) => format!("obs mute [{a}]"),
                    (_, true) => format!("obs {}", op.label()),
                    (_, false) => format!("obs {} {a}", op.label()),
                }
            }
        }
    }

    /// The [`Intent`] this action delegates to the run-daemon, if any. Daemon-handled variants
    /// (DPI / scroll-stage / profile) return `Some` — the daemon matches on it and performs the
    /// real device/profile work the stateless `Action` layer can't. Every host-executable action
    /// returns `None` (it fully runs in [`run_ctx`](Action::run_ctx)). This is the documented
    /// contract between an `Action` and the daemon: *if `intent()` is `Some`, run THAT against live
    /// state; otherwise just call `run_ctx`.*
    #[must_use]
    pub fn intent(&self) -> Option<Intent> {
        match self {
            Action::DpiCycle { dir } => Some(Intent::DpiCycle(*dir)),
            Action::DpiSet { dpi } => Some(Intent::DpiSet(*dpi)),
            Action::ScrollStageCycle { dir } => Some(Intent::ScrollStageCycle(*dir)),
            Action::ProfileSwitch { name } => Some(Intent::ProfileSwitch(name.clone())),
            Action::ProfileCycle { dir } => Some(Intent::ProfileCycle(*dir)),
            Action::Teleport => Some(Intent::Teleport),
            Action::Whiteboard => Some(Intent::Whiteboard),
            Action::Knockback => Some(Intent::Knockback),
            Action::Glance { target } => Some(Intent::Glance(target.clone())),
            Action::Summon { window, mode } => Some(Intent::Summon(window.clone(), *mode)),
            Action::Banish { pick } => Some(Intent::Banish(*pick)),
            Action::Pin { pick } => Some(Intent::Pin(*pick)),
            Action::Kill { pick } => Some(Intent::Kill(*pick)),
            Action::Echo => Some(Intent::Echo),
            Action::Tether { slot, mode } => Some(Intent::Tether(slot.clone(), *mode)),
            Action::Dial { target } => Some(Intent::Dial(*target)),
            Action::Control => Some(Intent::Control),
            _ => None,
        }
    }

    /// If this is a [`Action::Turbo`], the autofire rate (clicks-per-second) and the inner action
    /// to repeat — what the daemon needs to drive the held-down autofire loop. `None` for every
    /// non-turbo action. The daemon: on the trigger's *down* edge, fire `action` every
    /// `1000/cps` ms until the *up* edge.
    #[must_use]
    pub fn turbo(&self) -> Option<(u16, &Action)> {
        match self {
            Action::Turbo { action, cps } => Some((*cps, action)),
            _ => None,
        }
    }

    /// Whether running this action actually READS the captured [`Context`] (foreground app / cwd /
    /// clipboard / selection). Only the macro tiers do: a [`Action::Script`], a [`Action::Sequence`]
    /// containing one (checked transitively), or a [`Action::Turbo`] wrapping one. Every plain host
    /// action (`Key`/`MouseButton`/`Media`/`Mic*`/`Output*`/`Run`/intents/`Noop`) ignores `ctx`, so
    /// `run_ctx(&Context::default())` is identical to `run()` for them.
    ///
    /// The live dispatcher uses this to skip the expensive [`Context::capture`] (a clipboard open +
    /// foreground-window + process-image probe — a full OS round-trip that contends process-wide) on
    /// the common case where nothing matched needs it. Capture is paid only when a macro will read it.
    #[must_use]
    pub fn needs_context(&self) -> bool {
        match self {
            Action::Script { .. } => true,
            Action::Sequence { steps } => steps.iter().any(|s| s.action.needs_context()),
            Action::Turbo { action, .. } => action.needs_context(),
            _ => false,
        }
    }

    /// Execute the action with no captured context (the simple dispatch path). Side-effecting;
    /// returns a short result line for logging. Equivalent to [`Action::run_ctx`] against an
    /// empty [`Context`] — kept for callers (the CLI, `bindings.rs`, `cast.rs`) that don't
    /// snapshot the world before firing.
    #[must_use]
    pub fn run(&self) -> String {
        self.run_ctx(&Context::default())
    }

    /// Execute the action against a captured [`Context`] — the spine's dispatch path. The context
    /// is the world the action reacts to (foreground app / cwd / clipboard / selection / the
    /// window to restore). Only the script + sequence tiers consume it today; the simple actions
    /// ignore it, so `run()` and `run_ctx(&Context::default())` are identical for them.
    ///
    /// Threading `ctx` here (rather than re-`capture()`ing inside each action) is what makes a
    /// macro see a *consistent* snapshot: every step of a `Sequence`, and a `Script`, reason about
    /// the same foreground/clipboard the trigger fired against — and can `restore_foreground()`
    /// the original window after a sub-second in-game macro.
    #[must_use]
    pub fn run_ctx(&self, ctx: &Context) -> String {
        match self {
            Action::Noop => "noop".into(),
            Action::Run { cmd } => run_cmd(cmd),
            Action::Key { key } => press_key(key),
            Action::MicMute { device, mode } => mic_mute(device.as_deref(), mode),
            Action::MicGain { device, delta_pct } => mic_gain(device.as_deref(), *delta_pct),
            Action::MicGainSet { device, pct } => mic_set(device.as_deref(), *pct),
            Action::OutputMute { device, mode } => out_mute(device.as_deref(), mode),
            Action::OutputGain { device, delta_pct } => out_gain(device.as_deref(), *delta_pct),
            Action::Sequence { steps } => run_sequence(steps, ctx),
            // The macro engine's context-aware entry point: the trigger-time `ctx` the engine
            // captured threads all the way INTO the native macro body, so `ctx.app()` /
            // `ctx.cwd()` / `ctx.prev_window()` inside the macro are the world as it was when the
            // trigger fired (not a fresh capture taken mid-dispatch). A `Script` nested in a
            // `Sequence` therefore sees the same consistent snapshot as the rest of the macro.
            Action::Script { script } => crate::macros::run_script_ctx(script, ctx),
            Action::MouseButton { button } => press_mouse(*button),
            Action::Media { key } => press_media(*key),
            // Daemon-handled variants: the stateless Action layer has no device handle / profile
            // store, so run() only REPORTS the intent (a clean dry-run line). The run-daemon reads
            // `intent()` and performs the real work against live state. This keeps run()/run_ctx()
            // honest — they never silently no-op a write they can't do.
            Action::DpiCycle { dir } => format!("intent: DPI cycle {} (daemon)", dir.label()),
            Action::DpiSet { dpi } => format!("intent: DPI -> {dpi} (daemon)"),
            Action::ScrollStageCycle { dir } => {
                format!("intent: scroll-stage cycle {} (daemon)", dir.label())
            }
            Action::ProfileSwitch { name } => format!("intent: profile -> {name} (daemon)"),
            Action::ProfileCycle { dir } => {
                format!("intent: profile cycle {} (daemon)", dir.label())
            }
            // A one-shot turbo press: fire the inner action ONCE (the daemon drives the held
            // autofire loop via `turbo()`; a context-free run() is the single-press fallback).
            Action::Turbo { action, .. } => action.run_ctx(ctx),
            Action::Teleport => "intent: teleport (app)".into(),
            Action::Whiteboard => "intent: whiteboard (app)".into(),
            Action::Knockback => "intent: knockback (app)".into(),
            Action::Glance { target } => format!("intent: glance [{target}] (app)"),
            Action::Summon { window, mode } => {
                format!("intent: summon [{window}] {} (app)", mode.label())
            }
            Action::Banish { pick } => format!("intent: banish ({}) (app)", pick.label()),
            Action::Pin { pick } => format!("intent: pin ({}) (app)", pick.label()),
            Action::Kill { pick } => format!("intent: kill ({}) (app)", pick.label()),
            Action::Echo => "intent: echo (app)".into(),
            Action::Tether { slot, mode } => {
                format!("intent: tether [{slot}] {} (app)", mode.label())
            }
            // GHOST-PASTE: type the clipboard on a worker thread (never block this tick); the
            // typing is arm-gated, like every other synthesis.
            Action::GhostPaste { speed } => ghost_paste(*speed),
            // MOMENTARY MIC is edge-driven (the dispatch loop owns the press/release of a HELD
            // trigger). A one-shot fire has no release to honour, so run_ctx only reports.
            Action::MomentaryMic { mode, .. } => {
                format!("momentary mic ({}) \u{2014} hold to use", mode.label())
            }
            // SNIPER is edge-driven (the dispatch loop owns the DPI drop on DOWN, the restore on
            // UP of a HELD trigger). A one-shot fire has no release to honour, so run_ctx only reports.
            Action::Sniper { dpi } => format!("sniper ({dpi} DPI) \u{2014} hold to use"),
            // OUTPUT FLIP: set the default render endpoint to the next in the set (host-side,
            // reversible — an OS setting like the mute toggles, not a device write).
            Action::OutputFlip { devices } => out_flip(devices),
            Action::Dial { target } => format!("intent: dial {} (app)", target.label()),
            Action::Control => "intent: control center (app)".into(),
            // POCKET: move the clipboard into/out of a named register (host-side, arm-gated — it
            // writes the clipboard). The pocket module owns the full-fidelity snapshot + swap.
            Action::Pocket { slot, persist } => crate::pocket::activate(slot, *persist),
            // CURTAIN: a panic privacy screen — an opaque black overlay across every monitor, NOT a
            // monitor power-off. Spawns a worker (never blocks here); first key/click reveals.
            Action::Curtain => crate::curtain::raise(),
            // SYSTEM power verbs — instant + intentional. Arm-gated like every live side-effect so a
            // test/verify pass can never lock or suspend the developer's machine.
            Action::Lock => lock_workstation(),
            Action::Sleep => sleep_system(),
            // OBS — through the obs_hook seam, the exact path a macro's obs_* verb takes, so both
            // tiers are one surface (and one honest failure mode when the host is off).
            Action::Obs { op, arg } => obs_control(*op, arg),
        }
    }
}

/// Fire an OBS control through the [`crate::obs_hook`] seam. Arm-gated like the power verbs —
/// starting a public stream (or killing one) is exactly the class of side effect SAFE mode
/// exists for — and honest when nothing is connected: `None` (sink absent, OBS gate off) OR a
/// `false` from the sink (gate on but the websocket isn't authenticated — closed/reconnecting,
/// so the command was never sent) both surface "tell the user where the switch is", never a
/// silent success.
fn obs_control(op: ObsOp, arg: &str) -> String {
    if !input_armed() {
        return format!("obs {} [disarmed]", op.label());
    }
    let a = arg.trim();
    let or = |dflt: &'static str| -> serde_json::Value {
        if a.is_empty() { dflt.into() } else { a.into() }
    };
    let (verb, value): (&str, serde_json::Value) = match op {
        ObsOp::Stream => ("obs_stream", or("toggle")),
        ObsOp::Record => ("obs_record", or("toggle")),
        ObsOp::RecordPause => ("obs_record", "pause".into()),
        ObsOp::Replay => ("obs_replay", or("save")),
        ObsOp::Scene => {
            if a.is_empty() {
                return "obs scene: name the scene to switch to".into();
            }
            ("obs_scene", a.into())
        }
        ObsOp::Mute => ("obs_mute", or("Mic/Aux")),
    };
    match crate::obs_hook::dispatch(verb, &value) {
        Some((_, msg)) => msg,
        None => "OBS not connected (open SYSTEM \u{2192} CONNECTIONS)".into(),
    }
}

impl ScriptKind {
    /// Short tag for `describe`/logs.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            ScriptKind::Python => "python",
            ScriptKind::Shell => "shell",
            ScriptKind::File => "file",
        }
    }
}

/// Execute a macro sequence step by step against a captured context.
///
/// Hold semantics are REAL, not a tap-then-sleep: when `hold_ms > 0` and the step's action is a
/// `Key`, the key is pressed DOWN, held for `hold_ms`, then released — so "hold W for 200ms"
/// actually walks forward in a game for 200ms (a tap-then-sleep would register a single
/// keystroke and stand still). For non-`Key` actions (or `hold_ms == 0`) the action is a
/// fire-and-forget tap, and `hold_ms` then just pads the timeline like `delay_ms`. After every
/// step we pause `delay_ms` before the next, giving the macro author precise inter-step timing.
///
/// The `ctx` threads through to nested actions (a `Sequence` can contain a context-aware
/// `Script` step) so the whole macro reasons about one consistent snapshot of the world.
fn run_sequence(steps: &[Step], ctx: &Context) -> String {
    // A macro can sleep (held keys, inter-step delays), so it must not run on the live dispatch
    // thread the way a synchronous walk would — that would stall every other binding for the
    // macro's whole duration. Returns at once with a "running" line; the work happens off-thread.
    //
    // It goes to the WARM RUNNER POOL rather than a freshly spawned thread. That is a measured
    // choice, not a stylistic one: spawning cost mean 342µs / p99 2.6ms *on the dispatch thread*,
    // before the macro's first keystroke could go out — the largest controllable cost in the whole
    // press→output path. See `crate::macros::runner`.
    let n = steps.len();
    let plural = if n == 1 { "" } else { "s" };
    let owned_steps = steps.to_vec();
    let owned_ctx = ctx.clone();
    // Carry the press's origin across the thread boundary so the macro's FIRST keystroke still
    // records `press_to_output` against the real press — otherwise the headline number would stop
    // at "we handed the macro to a worker", which is not what the user feels.
    let origin = crate::latency::origin();
    // Scoped tightly to the HANDOFF. If the guard covered the whole `match`, the no-pool fallback's
    // inline run would record the macro's entire duration as "spawn cost" and poison the stage that
    // exists to measure the handoff.
    let submitted = {
        let _t = crate::latency::start(&crate::latency::MACRO_SPAWN);
        crate::macros::runner::submit(move || {
            crate::latency::adopt(origin);
            run_sequence_sync(&owned_steps, &owned_ctx);
        })
    };
    match submitted {
        crate::macros::runner::Submitted::Queued => format!("running macro ({n} step{plural})"),
        // Every worker slot is busy AND the queue is full — something is asking for more macro work
        // than the machine can run. Say so instead of stalling the dispatch thread behind it (see the
        // runner's doc on why refusing is the honest outcome).
        crate::macros::runner::Submitted::Refused => {
            format!("macro skipped ({n} step{plural}) — too many macros already running")
        }
        // No pool could be created at all. Fall back to the old per-fire thread, and only run inline
        // if even that fails — a macro is never silently dropped for an infrastructure failure.
        crate::macros::runner::Submitted::NoPool => {
            let steps2 = steps.to_vec();
            let ctx2 = ctx.clone();
            if crate::worker::spawn_detached("neuron-macro", move || {
                crate::latency::adopt(origin);
                run_sequence_sync(&steps2, &ctx2);
            }) {
                format!("running macro ({n} step{plural})")
            } else {
                run_sequence_sync(steps, ctx)
            }
        }
    }
}

/// The synchronous macro walk — runs every step in order and blocks for its holds/delays. This is the
/// body [`run_sequence`] drives on a worker, and the path a NESTED sequence takes inline (a `Sequence`
/// inside a `Sequence` runs here directly so it keeps the parent's ordering — only the OUTERMOST call
/// spawns a thread).
///
/// Holds are REAL, not tap-then-sleep: a `Key` OR `MouseButton` step with `hold_ms` presses DOWN,
/// waits, then releases — so "hold W 200ms" walks for 200ms and "hold right-click 500ms" drags. Other
/// actions fire-and-forget, and `hold_ms` then just pads the timeline. Each hold/pause is capped so a
/// fat-fingered value can't make a stray macro worker linger for minutes.
fn run_sequence_sync(steps: &[Step], ctx: &Context) -> String {
    use std::time::Duration;
    // A single step shouldn't sleep longer than this — it's off the dispatch thread now, but a
    // runaway value (`delay_ms: 600000`) still shouldn't strand a worker for ten minutes.
    const STEP_MS_CAP: u32 = 60_000;
    for step in steps {
        let hold = step.hold_ms.min(STEP_MS_CAP);
        match (&*step.action, hold) {
            // Held key / mouse button: a physically real down -> wait -> up.
            (Action::Key { key }, h) if h > 0 => {
                hold_key(key, h);
            }
            (Action::MouseButton { button }, h) if h > 0 => {
                hold_mouse(*button, h);
            }
            // A nested Sequence runs INLINE (synchronous) so the parent's ordering holds.
            (Action::Sequence { steps: inner }, _) => {
                run_sequence_sync(inner, ctx);
            }
            // Everything else: fire the action, then (if hold_ms set on a non-holdable) pad the line.
            (action, h) => {
                let _ = action.run_ctx(ctx);
                if h > 0 {
                    crate::timing::sleep_precise(Duration::from_millis(u64::from(h)));
                }
            }
        }
        if step.delay_ms > 0 {
            // `sleep_precise`, NOT `thread::sleep`: on Windows the latter is quantised to the ~15.6ms
            // scheduler tick, so every inter-step delay under ~16ms silently became ~16ms and a macro
            // written with 2ms spacing ran roughly eight times slower than authored. See `crate::timing`.
            crate::timing::sleep_precise(Duration::from_millis(
                u64::from(step.delay_ms.min(STEP_MS_CAP)),
            ));
        }
    }
    format!(
        "ran macro ({} step{})",
        steps.len(),
        if steps.len() == 1 { "" } else { "s" }
    )
}

/// Press a mouse button DOWN, hold `hold_ms`, then release — the mouse analogue of [`hold_key`].
#[cfg(windows)]
fn hold_mouse(button: MouseButtonKind, hold_ms: u32) -> String {
    unsafe { win_mouse::hold(button, u64::from(hold_ms)) };
    format!("held mouse {button:?} {hold_ms}ms")
}

#[cfg(not(windows))]
fn hold_mouse(_button: MouseButtonKind, hold_ms: u32) -> String {
    std::thread::sleep(std::time::Duration::from_millis(hold_ms as u64));
    "hold mouse: windows-only".into()
}

// ── GHOST-PASTE ─────────────────────────────────────────────────────────────────────────────────

/// Read the clipboard, then type it as keystrokes on a worker thread (so a long paste at a calm
/// pace never blocks the dispatch tick). Arm-gated like every synthesis. Returns immediately.
#[cfg(windows)]
fn ghost_paste(speed: GhostSpeed) -> String {
    if !input_armed() {
        return "ghost-paste [disarmed]".into();
    }
    let text = match clipboard_text() {
        ClipText::Text(t) if !t.is_empty() => t,
        // Tell a genuinely empty clipboard apart from one another process is holding RIGHT NOW — the
        // old code reported both as "empty", which lied when the clipboard was just momentarily busy.
        ClipText::Text(_) | ClipText::Empty => return "ghost-paste: clipboard is empty".into(),
        ClipText::Busy => return "ghost-paste: clipboard busy \u{2014} try again".into(),
    };
    let n = text.chars().count();
    // Report the TRUTH: if the OS refuses the typing thread (resource exhaustion), no keystrokes
    // go out, so don't claim typing started — same honesty the sequence path keeps.
    if !crate::worker::spawn_detached("neuron-ghost-paste", move || ghost_type(&text, speed)) {
        return "ghost-paste: couldn't start typing (system busy)".into();
    }
    format!(
        "ghost-paste: typing {n} char{} ({})",
        if n == 1 { "" } else { "s" },
        speed.label()
    )
}

#[cfg(not(windows))]
fn ghost_paste(_speed: GhostSpeed) -> String {
    "ghost-paste: windows-only".into()
}

/// Lock the workstation (the Win+L screen). Arm-gated so a test can never lock the dev out.
#[cfg(windows)]
fn lock_workstation() -> String {
    if !input_armed() {
        return "lock [disarmed]".into();
    }
    // Honour the BOOL: `LockWorkStation` can fail (e.g. another secure-desktop transition in flight).
    // Don't claim "locked" when it didn't.
    let ok = unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() };
    if ok != 0 {
        "locked".into()
    } else {
        "lock failed".into()
    }
}

#[cfg(not(windows))]
fn lock_workstation() -> String {
    "lock: windows-only".into()
}

/// Suspend the machine to standby. Arm-gated so a test never sleeps the dev's box. `SetSuspendState`
/// args = (hibernate?, force?, wake-events-disabled?) — all 0: a normal, ordinary sleep.
#[cfg(windows)]
fn sleep_system() -> String {
    if !input_armed() {
        return "sleep [disarmed]".into();
    }
    // Honour the BOOL: suspend can be refused (no privilege / policy / driver veto). Report honestly
    // rather than fabricating "sleeping" when nothing happened.
    let ok = unsafe { windows_sys::Win32::System::Power::SetSuspendState(0, 0, 0) };
    if ok != 0 {
        "sleeping".into()
    } else {
        "sleep failed".into()
    }
}

#[cfg(not(windows))]
fn sleep_system() -> String {
    "sleep: windows-only".into()
}

/// Type a string as real keystrokes: Unicode scan-codes for printables (layout-independent, types
/// anything), real Enter/Tab for newlines/tabs, jittered per-key timing so the cadence reads
/// organic. Re-checks the arm gate before EVERY key, so a `--safe` flip mid-paste stops it dead.
#[cfg(windows)]
fn ghost_type(text: &str, speed: GhostSpeed) {
    use std::time::Duration;
    let (base, jit) = speed.timing();
    // a cheap xorshift seeded with wall-clock entropy so the cadence VARIES between pastes (the old
    // seed used only `text.len()`, so the same clipboard typed at the same rhythm every single time).
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15
        ^ (text.len() as u64).wrapping_mul(0x2545_F491_4F6C_DD1D)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
    let mut delay = move || -> u64 {
        if jit == 0 {
            return base;
        }
        // organic cadence: a bell-ish jitter with a one-sided SLOW tail, floored at zero, instead of
        // a flat ±uniform — mirrors the Python host's `type_ghost` (base + max(0, gauss(0.1·jit,
        // 0.35·jit))), so both ghost typers share one feel. The sum of three uniforms is the bell.
        let mut u = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng >> 11) as f32 / (1u64 << 53) as f32
        };
        let z = (u() + u() + u() - 1.5) / 0.5; // mean 0, unit variance
        let tail = (0.1 + 0.35 * z) * jit as f32;
        base + tail.max(0.0) as u64
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !input_armed() {
            return;
        }
        let c = chars[i];
        match c {
            '\r' => {
                unsafe { win_key::tap(0x0D) }; // Enter
                if chars.get(i + 1) == Some(&'\n') {
                    i += 1; // \r\n is one newline
                }
            }
            '\n' => unsafe { win_key::tap(0x0D) },
            '\t' => unsafe { win_key::tap(0x09) },
            _ => unsafe { win_key::unicode(c) },
        }
        i += 1;
        let mut d = delay();
        // a longer beat after word and sentence breaks, like a typist finishing a phrase (the same
        // multipliers, applied only mid-text, as the Python host's `type_ghost` — so the two paths
        // stay in step right down to the trailing pause there is no reason to take)
        if i < chars.len() {
            match chars[i - 1] {
                ' ' => d = d * 16 / 10,
                '.' | ',' | ';' | ':' | '!' | '?' | '\u{2026}' => d = d * 24 / 10,
                _ => {}
            }
        }
        if d > 0 {
            std::thread::sleep(Duration::from_millis(d));
        } else if i % 64 == 0 {
            // Instant pace still takes a 1ms breather every so often, so a huge paste can't outrun
            // the target's input queue and drop/reorder characters.
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// What [`clipboard_text`] found: real text, an empty/non-text clipboard, or one another process is
/// holding right now (contended — worth telling the user apart from a truly empty clipboard).
#[cfg(windows)]
enum ClipText {
    Text(String),
    Empty,
    Busy,
}

/// The current clipboard as text (`CF_UNICODETEXT`). Retries the open briefly so a momentary
/// contention reads as [`ClipText::Busy`] rather than a false [`ClipText::Empty`].
#[cfg(windows)]
fn clipboard_text() -> ClipText {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    // Serialize this process's clipboard window against every other clipboard user — see
    // `crate::clipboard` for why there is exactly one process-wide lock.
    let _guard = crate::clipboard::clipboard_guard();
    unsafe {
        // A clipboard manager / RDP / a browser can hold the clipboard for a few ms — retry before
        // giving up so we don't misreport contention as "empty".
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if !opened {
            return ClipText::Busy;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        let out = if h.is_null() {
            ClipText::Empty
        } else {
            let p = GlobalLock(h.cast()) as *const u16;
            if p.is_null() {
                ClipText::Empty
            } else {
                let mut len = 0;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                GlobalUnlock(h.cast());
                ClipText::Text(s)
            }
        };
        CloseClipboard();
        out
    }
}

// ── OUTPUT FLIP ─────────────────────────────────────────────────────────────────────────────────

/// Switch the default render endpoint to the next in `devices` (host-side OS setting). Delegates
/// to the audio layer's hand-rolled `IPolicyConfig`.
#[cfg(windows)]
fn out_flip(devices: &[String]) -> String {
    crate::audio::flip_output(devices)
}

#[cfg(not(windows))]
fn out_flip(_devices: &[String]) -> String {
    "output flip: windows-only".into()
}

/// Press a key DOWN, hold it `hold_ms`, then release — the real-hold primitive macros use.
/// Chord-aware like [`press_key`]: modifiers wrap the whole hold.
#[cfg(windows)]
fn hold_key(name: &str, hold_ms: u32) -> String {
    use std::time::Duration;
    let (mods, key) = parse_combo(name);
    let Some(vk) = vk_for(&key) else {
        return format!("unknown key '{name}'");
    };
    // RAII: every key we press DOWN is released on the way out — on a normal return AND on a
    // panic-unwind during the hold (release builds unwind, so Drop runs). A stranded modifier is the
    // worst macro footgun, and a `sleep` is exactly where a fault could land.
    struct Held(Vec<u16>);
    impl Drop for Held {
        fn drop(&mut self) {
            for vk in self.0.iter().rev() {
                unsafe { win_key::up(*vk) };
            }
        }
    }
    let mut held = Held(Vec::with_capacity(mods.len() + 1));
    unsafe {
        for m in &mods {
            win_key::down(*m);
            held.0.push(*m);
        }
        win_key::down(vk);
        held.0.push(vk);
    }
    // Precise: a "hold W for 2ms" that really holds for 15ms is a different input to a game than the
    // one the macro author wrote. See `crate::timing`.
    crate::timing::sleep_precise(Duration::from_millis(u64::from(hold_ms)));
    drop(held); // release in reverse press order (also fires if the sleep above unwinds)
    format!("held [{name}] {hold_ms}ms (vk 0x{vk:02X})")
}

#[cfg(not(windows))]
fn hold_key(name: &str, hold_ms: u32) -> String {
    std::thread::sleep(std::time::Duration::from_millis(hold_ms as u64));
    format!("hold '{name}': windows-only")
}

fn dev(d: &Option<String>) -> String {
    d.as_ref().map(|s| format!(" [{s}]")).unwrap_or_default()
}

fn run_cmd(cmd: &str) -> String {
    // Gated like every other live side-effect: a shell-out is as real as a keystroke. DISARMED
    // (tests / verify / fuzz) => report what WOULD run, spawn nothing.
    if !process_spawn_armed() {
        return format!("run `{cmd}` [disarmed]");
    }
    // Through the shared macro helper so shell-outs never inherit Neuron's process arm-state and
    // interpreter consoles stay hidden on Windows — and so a spawn fired by a keypress does not stall
    // the dispatch pump for the milliseconds `CreateProcess` takes (see `crate::macros::run_shell`).
    crate::macros::run_shell(cmd)
}

#[cfg(windows)]
fn mic_gain(device: Option<&str>, delta_pct: f32) -> String {
    match crate::audio::resolve_capture(device)
        .and_then(|e| crate::audio::VolumeCtl::open(&e.id).map(|c| (e.name, c)))
    {
        Some((name, ctl)) => {
            let v = ctl.nudge(delta_pct / 100.0);
            format!("{name} gain -> {}%", (v * 100.0).round() as i32)
        }
        None => "no mic".into(),
    }
}

#[cfg(windows)]
fn mic_set(device: Option<&str>, pct: f32) -> String {
    match crate::audio::resolve_capture(device)
        .and_then(|e| crate::audio::VolumeCtl::open(&e.id).map(|c| (e.name, c)))
    {
        Some((name, ctl)) => {
            ctl.set_volume(pct / 100.0);
            format!("{name} gain = {}%", pct.round() as i32)
        }
        None => "no mic".into(),
    }
}

#[cfg(windows)]
fn mic_mute(device: Option<&str>, mode: &str) -> String {
    match crate::audio::resolve_capture(device)
        .and_then(|e| crate::audio::VolumeCtl::open(&e.id).map(|c| (e.id, e.name, c)))
    {
        Some((id, name, ctl)) => {
            // Write, then — only if the OS state actually CHANGED (`set_mute` reports that; a toggle
            // always changes) — open the mic-tap self-write window, so the detector doesn't fire
            // `MicTap`'s bound actions for our OWN write. A no-op write is no transition, so the poll
            // sees no edge and a window would only shadow a real tap. A real transition's ~1s window
            // opens well inside the ~400ms cache lag before the poll could sample it, so there is no
            // race. ONLY the DEFAULT endpoint arms it: `device` may name a SECONDARY mic the detector
            // never samples — arming for it could shadow a real tap on the default.
            let (s, changed) = match mode {
                "on" => (true, ctl.set_mute(true)),
                "off" => (false, ctl.set_mute(false)),
                _ => (ctl.toggle_mute(), true),
            };
            if changed && crate::audio::is_default_capture_id(&id) {
                crate::mic_state::note_self_mute_write();
            }
            format!("{name} mute -> {}", if s { "ON" } else { "off" })
        }
        None => "no mic".into(),
    }
}

#[cfg(not(windows))]
fn mic_gain(_d: Option<&str>, _p: f32) -> String {
    "mic-gain: windows-only".into()
}

#[cfg(not(windows))]
fn mic_set(_d: Option<&str>, _p: f32) -> String {
    "mic-gain-set: windows-only".into()
}

#[cfg(not(windows))]
fn mic_mute(_d: Option<&str>, _m: &str) -> String {
    "mic-mute: windows-only".into()
}

#[cfg(windows)]
fn out_gain(device: Option<&str>, delta_pct: f32) -> String {
    match crate::audio::resolve_render(device)
        .and_then(|e| crate::audio::VolumeCtl::open(&e.id).map(|c| (e.name, c)))
    {
        Some((name, ctl)) => {
            let v = ctl.nudge(delta_pct / 100.0);
            format!("{name} vol -> {}%", (v * 100.0).round() as i32)
        }
        None => "no output device".into(),
    }
}
#[cfg(windows)]
fn out_mute(device: Option<&str>, mode: &str) -> String {
    match crate::audio::resolve_render(device)
        .and_then(|e| crate::audio::VolumeCtl::open(&e.id).map(|c| (e.name, c)))
    {
        Some((name, ctl)) => {
            let s = match mode {
                "on" => {
                    ctl.set_mute(true);
                    true
                }
                "off" => {
                    ctl.set_mute(false);
                    false
                }
                _ => ctl.toggle_mute(),
            };
            format!("{name} mute -> {}", if s { "ON" } else { "off" })
        }
        None => "no output device".into(),
    }
}
#[cfg(not(windows))]
fn out_gain(_d: Option<&str>, _p: f32) -> String {
    "out-gain: windows-only".into()
}
#[cfg(not(windows))]
fn out_mute(_d: Option<&str>, _m: &str) -> String {
    "out-mute: windows-only".into()
}

/// Map a key name to a Windows virtual-key code. Covers the FULL practical keyboard — letters,
/// digits, F-keys, the nav cluster, numpad, punctuation, side-specific modifiers, media transport —
/// plus a transparent `0xNN` hex literal so NOTHING a capture can produce is unnameable. The
/// canonical inverse is [`key_param_for_vk`]; the two round-trip.
#[must_use]
pub fn vk_for(name: &str) -> Option<u16> {
    let n = name.trim().to_lowercase();
    if n.len() == 1 {
        let ch = n.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_uppercase() as u16); // 'A'..'Z' == VK
        }
        if ch.is_ascii_digit() {
            return Some(ch as u16); // '0'..'9' == VK
        }
        // OEM punctuation — the US-layout VK_OEM_* codes (what a capture of these keys yields).
        return Some(match ch {
            ';' => 0xBA,
            '=' => 0xBB,
            ',' => 0xBC,
            '-' => 0xBD,
            '.' => 0xBE,
            '/' => 0xBF,
            '`' => 0xC0,
            '[' => 0xDB,
            '\\' => 0xDC,
            ']' => 0xDD,
            '\'' => 0xDE,
            _ => return None,
        });
    }
    if let Some(f) = n.strip_prefix('f') {
        if let Ok(num) = f.parse::<u16>() {
            if (1..=24).contains(&num) {
                return Some(0x70 + (num - 1)); // VK_F1 = 0x70
            }
        }
    }
    // numpad: "num0".."num9" and the operator keys.
    if let Some(d) = n.strip_prefix("num") {
        if let Some(vk) = match d {
            "*" => Some(0x6A),
            "+" => Some(0x6B),
            "-" => Some(0x6D),
            "." => Some(0x6E),
            "/" => Some(0x6F),
            _ => d.parse::<u16>().ok().filter(|v| *v <= 9).map(|v| 0x60 + v),
        } {
            return Some(vk);
        }
    }
    // the transparent escape hatch: any VK as a hex literal (what key_param_for_vk emits for
    // exotic keys) — we never strand a captured key without a name.
    if let Some(h) = n.strip_prefix("0x") {
        if let Ok(v) = u16::from_str_radix(h, 16) {
            return (v > 0 && v < 256).then_some(v);
        }
    }
    // Media / volume transport names (also reachable typed via `Action::Media`); recognized here
    // so a `Key { key: "media-play-pause" }` an older import produced still resolves to a real VK.
    if let Some(m) = MediaKind::from_key_name(&n) {
        return Some(m.vk());
    }
    Some(match n.as_str() {
        "enter" | "return" => 0x0D,
        "space" | "spacebar" => 0x20,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "shift" => 0x10,
        "ctrl" | "control" => 0x11,
        "alt" => 0x12,
        "win" => 0x5B,
        "apps" | "menu" => 0x5D,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "insert" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "page-up" | "pgup" => 0x21,
        "page-down" | "pgdn" => 0x22,
        "caps-lock" | "caps" => 0x14,
        "num-lock" => 0x90,
        "scroll-lock" => 0x91,
        "pause" => 0x13,
        "print-screen" | "prtsc" => 0x2C,
        "lshift" => 0xA0,
        "rshift" => 0xA1,
        "lctrl" => 0xA2,
        "rctrl" => 0xA3,
        "lalt" => 0xA4,
        "ralt" => 0xA5,
        "browser-back" => 0xA6,
        "browser-forward" => 0xA7,
        "browser-refresh" => 0xA8,
        _ => return None,
    })
}

/// The canonical key-param name for a virtual-key — the EXACT string [`vk_for`] parses back to the
/// same code, so press-to-bind capture can write what the engine reads (no friendly-display-name /
/// config-name mismatch). Total over 1..256: anything without a word name gets the transparent
/// `0xNN` hex form rather than being unrepresentable.
#[must_use]
pub fn key_param_for_vk(vk: u16) -> String {
    match vk {
        v if (0x41..=0x5A).contains(&v) => ((v as u8 as char).to_ascii_lowercase()).to_string(),
        v if (0x30..=0x39).contains(&v) => ((v as u8) as char).to_string(),
        v if (0x70..=0x87).contains(&v) => format!("f{}", v - 0x6F),
        v if (0x60..=0x69).contains(&v) => format!("num{}", v - 0x60),
        0x6A => "num*".into(),
        0x6B => "num+".into(),
        0x6D => "num-".into(),
        0x6E => "num.".into(),
        0x6F => "num/".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        0x0D => "enter".into(),
        0x20 => "space".into(),
        0x09 => "tab".into(),
        0x1B => "esc".into(),
        0x08 => "backspace".into(),
        0x10 => "shift".into(),
        0x11 => "ctrl".into(),
        0x12 => "alt".into(),
        0x5B | 0x5C => "win".into(),
        0x5D => "apps".into(),
        0x26 => "up".into(),
        0x28 => "down".into(),
        0x25 => "left".into(),
        0x27 => "right".into(),
        0x2D => "insert".into(),
        0x2E => "delete".into(),
        0x24 => "home".into(),
        0x23 => "end".into(),
        0x21 => "page-up".into(),
        0x22 => "page-down".into(),
        0x14 => "caps-lock".into(),
        0x90 => "num-lock".into(),
        0x91 => "scroll-lock".into(),
        0x13 => "pause".into(),
        0x2C => "print-screen".into(),
        0xA0 => "lshift".into(),
        0xA1 => "rshift".into(),
        0xA2 => "lctrl".into(),
        0xA3 => "rctrl".into(),
        0xA4 => "lalt".into(),
        0xA5 => "ralt".into(),
        0xA6 => "browser-back".into(),
        0xA7 => "browser-forward".into(),
        0xA8 => "browser-refresh".into(),
        0xB3 => "media-play-pause".into(),
        0xB2 => "media-stop".into(),
        0xB0 => "media-next".into(),
        0xB1 => "media-prev".into(),
        0xAF => "volume-up".into(),
        0xAE => "volume-down".into(),
        0xAD => "volume-mute".into(),
        v => format!("0x{v:02X}"),
    }
}

/// Convert a neuron key-name (the [`key_param_for_vk`] vocabulary — lowercase: `"g"`, `"5"`,
/// `"f5"`, `"-"`, `"space"`, `"up"`, `"lctrl"`) into its RAW HID Keyboard/Keypad usage, for a
/// DEVICE-SIDE button remap ([`crate::writes::set_mouse_button_key`]). `None` for anything a single
/// device usage can't express — chords (`"ctrl+s"`), media keys, numpad, or unknown names — so the
/// caller falls back to host-side dispatch. The inverse of `controls::kbd_usage_name`.
#[must_use]
pub fn hid_usage_for_key(key: &str) -> Option<u8> {
    let raw = key.trim();
    if raw.is_empty() || raw.contains('+') {
        return None; // a chord / multi-key can't be a single device usage
    }
    // Normalize case ONCE, here, and use `k` everywhere below.
    //
    // Case used to be folded in two separate places — per-character for the single-char branch, and
    // again inside the named-key match — which left the function-key branch (`strip_prefix('f')`)
    // matching lowercase only. So `"g"`/`"G"` both worked while `"F5"` silently returned `None` and
    // fell back to host-side dispatch, even though `"f5"` mapped fine. One normalization point makes
    // that whole class of inconsistency unrepresentable rather than fixing the `f` branch alone.
    let lowered = raw.to_ascii_lowercase();
    let k = lowered.as_str();
    if k.len() == 1 {
        let c = k.as_bytes()[0];
        match c {
            b'a'..=b'z' => return Some(0x04 + (c - b'a')),
            b'1'..=b'9' => return Some(0x1E + (c - b'1')),
            b'0' => return Some(0x27),
            b'-' => return Some(0x2D),
            b'=' => return Some(0x2E),
            b'[' => return Some(0x2F),
            b']' => return Some(0x30),
            b'\\' => return Some(0x31),
            b';' => return Some(0x33),
            b'\'' => return Some(0x34),
            b'`' => return Some(0x35),
            b',' => return Some(0x36),
            b'.' => return Some(0x37),
            b'/' => return Some(0x38),
            _ => return None,
        }
    }
    // fN function keys (f1..f12 -> 0x3A.., f13..f24 -> 0x68..)
    if let Some(n) = k
        .strip_prefix('f')
        .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<u8>().ok())
    {
        return match n {
            1..=12 => Some(0x3A + (n - 1)),
            13..=24 => Some(0x68 + (n - 13)),
            _ => None,
        };
    }
    Some(match k {
        "space" => 0x2C,
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2A,
        "tab" => 0x2B,
        "caps-lock" => 0x39,
        "up" => 0x52,
        "down" => 0x51,
        "left" => 0x50,
        "right" => 0x4F,
        "insert" => 0x49,
        "delete" | "del" => 0x4C,
        "home" => 0x4A,
        "end" => 0x4D,
        "page-up" => 0x4B,
        "page-down" => 0x4E,
        "num-lock" => 0x53,
        "scroll-lock" => 0x47,
        "print-screen" => 0x46,
        "pause" => 0x48,
        "menu" | "apps" => 0x65,
        "lctrl" | "ctrl" | "control" => 0xE0,
        "lshift" | "shift" => 0xE1,
        "lalt" | "alt" => 0xE2,
        "lwin" | "win" => 0xE3,
        "rctrl" => 0xE4,
        "rshift" => 0xE5,
        "ralt" => 0xE6,
        "rwin" => 0xE7,
        _ => return None,
    })
}

/// Split a `+`-chord key name into `(modifier VKs, the final key name)`. Only the canonical
/// modifier names chord (`ctrl`/`shift`/`alt`/`win` and their sided forms) so names that legally
/// CONTAIN a `+` — `num+`, `=` — never mis-split. A bare modifier name is just a key, not a chord.
#[must_use]
pub fn parse_combo(name: &str) -> (Vec<u16>, String) {
    const MODS: &[(&str, u16)] = &[
        ("ctrl", 0x11),
        ("control", 0x11),
        ("shift", 0x10),
        ("alt", 0x12),
        ("win", 0x5B),
        ("lctrl", 0xA2),
        ("rctrl", 0xA3),
        ("lshift", 0xA0),
        ("rshift", 0xA1),
        ("lalt", 0xA4),
        ("ralt", 0xA5),
    ];
    let mut rest = name.trim();
    let mut mods = Vec::new();
    'strip: loop {
        for (m, vk) in MODS {
            // case-insensitive "<mod>+<more>" prefix, byte-indexed (the mod names are ASCII).
            if rest.len() > m.len() + 1
                && rest.as_bytes()[m.len()] == b'+'
                && rest[..m.len()].eq_ignore_ascii_case(m)
            {
                mods.push(*vk);
                rest = &rest[m.len() + 1..];
                continue 'strip;
            }
        }
        break;
    }
    (mods, rest.to_string())
}

#[cfg(windows)]
fn press_key(name: &str) -> String {
    // chord-aware: "ctrl+shift+s" presses the modifiers down, taps the key, releases in reverse.
    let (mods, key) = parse_combo(name);
    let Some(vk) = vk_for(&key) else {
        return format!("unknown key '{name}'");
    };
    unsafe {
        for m in &mods {
            win_key::down(*m);
        }
        win_key::tap(vk);
        for m in mods.iter().rev() {
            win_key::up(*m);
        }
    }
    format!("pressed [{name}] (vk 0x{vk:02X})")
}

#[cfg(not(windows))]
fn press_key(name: &str) -> String {
    format!("key '{name}': windows-only")
}

/// Press and HOLD a key combo (modifiers + key), returning the VKs now held in press order — so the
/// caller releases them on the trigger's UP edge via [`release_keys`]. The held twin of
/// [`press_key`]'s tap: an input→key REMAP holds the output key while the control is held, so it
/// behaves like the real key (Windows supplies the auto-repeat). Empty result = unknown key (no-op).
/// (Distinct from the timed `hold_key(name, hold_ms)` above, which is a single press-wait-release.)
#[cfg(windows)]
#[must_use]
pub fn press_and_hold(name: &str) -> Vec<u16> {
    let (mods, key) = parse_combo(name);
    let Some(vk) = vk_for(&key) else {
        return Vec::new();
    };
    let mut held = Vec::with_capacity(mods.len() + 1);
    unsafe {
        for m in &mods {
            win_key::down(*m);
            held.push(*m);
        }
        win_key::down(vk);
    }
    held.push(vk);
    held
}

/// Release keys held by [`hold_key`], in reverse press order (the key first, then the modifiers).
#[cfg(windows)]
pub fn release_keys(vks: &[u16]) {
    unsafe {
        for &vk in vks.iter().rev() {
            win_key::up(vk);
        }
    }
}

#[cfg(not(windows))]
pub fn press_and_hold(_name: &str) -> Vec<u16> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn release_keys(_vks: &[u16]) {}

#[cfg(windows)]
fn press_media(key: MediaKind) -> String {
    unsafe { win_key::tap(key.vk()) };
    format!("media {} (vk 0x{:02X})", key.label(), key.vk())
}

#[cfg(not(windows))]
fn press_media(key: MediaKind) -> String {
    format!("media {}: windows-only", key.label())
}

#[cfg(windows)]
fn press_mouse(button: MouseButtonKind) -> String {
    unsafe { win_mouse::click(button) };
    format!("mouse {}", button.label())
}

#[cfg(not(windows))]
fn press_mouse(button: MouseButtonKind) -> String {
    format!("mouse {}: windows-only", button.label())
}

/// Narrow native effects exposed to the BOUND macro broker. These reuse the same SendInput gate and
/// key/mouse primitives as ordinary Actions; Python no longer owns a parallel input implementation.
pub(crate) fn macro_key(name: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    press_key(name)
}

pub(crate) fn macro_hotkey(keys: &[String]) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    let mut vks = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(vk) = vk_for(key) else {
            return format!("[unknown key {key:?}]");
        };
        vks.push(vk);
    }
    #[cfg(windows)]
    unsafe {
        for vk in &vks {
            win_key::down(*vk);
        }
        for vk in vks.iter().rev() {
            win_key::up(*vk);
        }
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = vks;
        "[unsupported]".into()
    }
}

pub(crate) fn macro_key_down(name: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    let Some(vk) = vk_for(name) else {
        return format!("[unknown key {name:?}]");
    };
    #[cfg(windows)]
    unsafe {
        win_key::down(vk);
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        "[unsupported]".into()
    }
}

pub(crate) fn macro_key_up(name: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    let Some(vk) = vk_for(name) else {
        return format!("[unknown key {name:?}]");
    };
    #[cfg(windows)]
    unsafe {
        win_key::up(vk);
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        "[unsupported]".into()
    }
}

pub(crate) fn macro_type_text(text: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    #[cfg(windows)]
    unsafe {
        for ch in text.chars() {
            win_key::unicode(ch);
        }
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        "[unsupported]".into()
    }
}

pub(crate) fn macro_type_ghost(text: &str, speed: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    #[cfg(windows)]
    {
        ghost_type(text, GhostSpeed::parse(speed));
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = (text, speed);
        "[unsupported]".into()
    }
}

fn macro_mouse_button_kind(button: &str) -> Option<MouseButtonKind> {
    match button.trim().to_lowercase().as_str() {
        "left" => Some(MouseButtonKind::Left),
        "right" => Some(MouseButtonKind::Right),
        "middle" => Some(MouseButtonKind::Middle),
        "back" | "x1" => Some(MouseButtonKind::Back),
        "forward" | "x2" => Some(MouseButtonKind::Forward),
        _ => None,
    }
}

pub(crate) fn macro_click(button: &str) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    let Some(button) = macro_mouse_button_kind(button) else {
        return format!("[unknown mouse button {button:?}]");
    };
    press_mouse(button)
}

pub(crate) fn macro_scroll(notches: i32) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    #[cfg(windows)]
    unsafe {
        win_mouse::scroll_vertical(notches);
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = notches;
        "[unsupported]".into()
    }
}

pub(crate) fn macro_mouse_move(dx: i32, dy: i32) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    #[cfg(windows)]
    unsafe {
        win_mouse::move_relative(dx, dy);
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = (dx, dy);
        "[unsupported]".into()
    }
}

pub(crate) fn macro_mouse_to(x: i32, y: i32) -> String {
    if !input_armed() {
        return "[disarmed]".into();
    }
    #[cfg(windows)]
    unsafe {
        win_mouse::move_absolute_screen(x, y);
        "ok".into()
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        "[unsupported]".into()
    }
}

/// Arm or disarm real input synthesis process-wide.
///
/// The authority lives in [`crate::safety`], not in the process environment. The Macro Host receives
/// explicit arm-state control frames; child process launchers still strip the historical
/// `NEURON_INPUT_ARMED` variable as defense-in-depth for old nested processes.
pub fn arm_input(on: bool) {
    crate::safety::set_input_armed(on);
}

/// Whether real keyboard/mouse synthesis is currently permitted (default `false`).
#[must_use]
pub fn input_armed() -> bool {
    crate::safety::input_armed()
}

/// Send Windows input only while the process-wide arm gate is open. Returns the number of
/// events accepted by Windows; zero means disarmed or no event was accepted.
#[cfg(windows)]
pub fn send_win_input(inputs: &[windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> u32 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    if !input_armed() || inputs.is_empty() {
        return 0;
    }
    let Ok(count) = u32::try_from(inputs.len()) else { return 0; };
    let start = std::time::Instant::now();
    // SAFETY: the slice is valid for `count` INPUT values for the duration of this call.
    let sent = unsafe { SendInput(count, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    crate::latency::SEND_INPUT.record(start.elapsed());
    if sent > 0 {
        crate::latency::mark_output();
    }
    sent
}

/// Whether process-spawning side-effects (`Action::Run`, macro `ScriptKind::Shell` / `File`, and
/// the macro prelude's `run()` helper) are currently permitted. Tied to the SAME process-wide arm
/// gate as [`input_armed`]: a macro that shells out `calc.exe` or `format C:` is just as much a
/// live side-effect as a synthesized keystroke, so the verify/fuzz pass and `cargo test` (both
/// DISARMED by default) must NOT actually spawn anything. The live daemon / GUI arm it once at
/// startup via [`arm_input`]; tests never do.
///
/// This deliberately closes the gap the verify audit flagged: `Action::Run` / `ScriptKind::Shell`
/// previously spawned processes unconditionally, bypassing the gate that protects every other real
/// side-effect. Build-toolchain spawns (rustc/rustup in the compile + verify pipeline) are NOT
/// routed through here — they are the macro *compiler*, not the macro's runtime behavior, and must
/// run during verification regardless of arm state.
#[must_use]
pub fn process_spawn_armed() -> bool {
    input_armed()
}

#[cfg(windows)]
mod win_key {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE,
    };

    /// Build one keyboard `INPUT` for `vk` with the given event flags.
    fn mk(vk: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Send a single keyboard event (down or up).
    unsafe fn send_one(input: INPUT) {
        super::send_win_input(std::slice::from_ref(&input));
    }

    /// Press a key down (no release) — pairs with [`up`] for real held-key macros.
    pub unsafe fn down(vk: u16) {
        send_one(mk(vk, 0));
    }

    /// Release a key.
    pub unsafe fn up(vk: u16) {
        send_one(mk(vk, KEYEVENTF_KEYUP));
    }

    /// A full keystroke: down then up.
    pub unsafe fn tap(vk: u16) {
        if !super::input_armed() {
            return;
        }
        let inputs = [mk(vk, 0), mk(vk, KEYEVENTF_KEYUP)];
        super::send_win_input(&inputs);
    }

    /// Type ONE character as a Unicode scan-code (down+up per UTF-16 unit) — layout-independent,
    /// so ghost-paste types any glyph regardless of the active keyboard. Surrogate pairs send
    /// both units. The synthesised events carry `KEYEVENTF_UNICODE` (wVk = 0).
    pub unsafe fn unicode(c: char) {
        if !super::input_armed() {
            return;
        }
        let uni = |unit: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        let mut seq: Vec<INPUT> = Vec::with_capacity(units.len() * 2);
        for &u in units.iter() {
            seq.push(uni(u, false));
            seq.push(uni(u, true));
        }
        super::send_win_input(&seq);
    }
}

#[cfg(windows)]
mod win_mouse {
    use super::MouseButtonKind;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    };

    // These live under `WindowsAndMessaging` in windows-sys; inlined as literals (their values are
    // ABI-stable Win32 constants) to avoid pulling that whole module's import surface in here.
    const WHEEL_DELTA: i32 = 120; // one wheel notch
    const XBUTTON1: i32 = 1; // X1 == "Back"
    const XBUTTON2: i32 = 2; // X2 == "Forward"

    /// Build one mouse `INPUT`. `mouse_data` carries the X-button id (XBUTTON1/2) for X events or
    /// the signed wheel delta for wheel events; 0 otherwise.
    fn mk(flags: MOUSE_EVENT_FLAGS, mouse_data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    // mouseData is a u32 field; wheel deltas are signed and reinterpreted.
                    mouseData: mouse_data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Send a slice of mouse inputs through the shared process-wide arm gate.
    unsafe fn send(inputs: &[INPUT]) {
        super::send_win_input(inputs);
    }

    /// Synthesize a full click (down+up) for a button, or one wheel notch for a scroll direction.
    pub unsafe fn click(button: MouseButtonKind) {
        match button {
            MouseButtonKind::Left => {
                send(&[mk(MOUSEEVENTF_LEFTDOWN, 0), mk(MOUSEEVENTF_LEFTUP, 0)]);
            }
            MouseButtonKind::Right => {
                send(&[mk(MOUSEEVENTF_RIGHTDOWN, 0), mk(MOUSEEVENTF_RIGHTUP, 0)]);
            }
            MouseButtonKind::Middle => {
                send(&[mk(MOUSEEVENTF_MIDDLEDOWN, 0), mk(MOUSEEVENTF_MIDDLEUP, 0)]);
            }
            MouseButtonKind::Back => {
                send(&[
                    mk(MOUSEEVENTF_XDOWN, XBUTTON1),
                    mk(MOUSEEVENTF_XUP, XBUTTON1),
                ]);
            }
            MouseButtonKind::Forward => {
                send(&[
                    mk(MOUSEEVENTF_XDOWN, XBUTTON2),
                    mk(MOUSEEVENTF_XUP, XBUTTON2),
                ]);
            }
            // Tilt-scroll: one horizontal wheel notch. Positive HWHEEL delta = right, negative =
            // left (Win32 convention).
            MouseButtonKind::ScrollLeft => {
                send(&[mk(MOUSEEVENTF_HWHEEL, -WHEEL_DELTA)]);
            }
            MouseButtonKind::ScrollRight => {
                send(&[mk(MOUSEEVENTF_HWHEEL, WHEEL_DELTA)]);
            }
        }
    }

    pub unsafe fn scroll_vertical(notches: i32) {
        send(&[mk(MOUSEEVENTF_WHEEL, notches.saturating_mul(WHEEL_DELTA))]);
    }

    pub unsafe fn move_relative(dx: i32, dy: i32) {
        let mut input = mk(MOUSEEVENTF_MOVE, 0);
        input.Anonymous.mi.dx = dx;
        input.Anonymous.mi.dy = dy;
        send(&[input]);
    }

    pub unsafe fn move_absolute_screen(x: i32, y: i32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
        let w = GetSystemMetrics(0).max(1);
        let h = GetSystemMetrics(1).max(1);
        let mut input = mk(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0);
        input.Anonymous.mi.dx = x.saturating_mul(65535) / w;
        input.Anonymous.mi.dy = y.saturating_mul(65535) / h;
        send(&[input]);
    }

    /// Press a button DOWN, hold `ms`, then release — the mouse analogue of a held key. The release
    /// is RAII-guarded so a panic-unwind during the hold can't strand the button down (a stuck drag).
    /// Scroll directions can't be "held" (a wheel notch is instantaneous): they fire one notch.
    pub unsafe fn hold(button: MouseButtonKind, ms: u64) {
        let (down, up, data): (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, i32) = match button {
            MouseButtonKind::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            MouseButtonKind::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            MouseButtonKind::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            MouseButtonKind::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
            MouseButtonKind::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
            MouseButtonKind::ScrollLeft => {
                send(&[mk(MOUSEEVENTF_HWHEEL, -WHEEL_DELTA)]);
                return;
            }
            MouseButtonKind::ScrollRight => {
                send(&[mk(MOUSEEVENTF_HWHEEL, WHEEL_DELTA)]);
                return;
            }
        };
        send(&[mk(down, data)]);
        // RAII release — fires on normal completion AND on a panic-unwind during the sleep below.
        struct Release(MOUSE_EVENT_FLAGS, i32);
        impl Drop for Release {
            fn drop(&mut self) {
                unsafe { send(&[mk(self.0, self.1)]) };
            }
        }
        let _release = Release(up, data);
        // Precise for the same reason as the key hold: a drag or a short click-hold that overshoots
        // to the ~15.6ms scheduler tick is not the input the macro asked for. See `crate::timing`.
        crate::timing::sleep_precise(std::time::Duration::from_millis(ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hid_usage_for_key` maps neuron key-names to raw HID usages, is the inverse of controls'
    /// `kbd_usage_name` for the keys both cover, and rejects anything a single device usage can't
    /// express (chords, media, numpad, unknown).
    #[test]
    fn hid_usage_for_key_maps_and_rejects() {
        // letters, digits, symbols
        assert_eq!(hid_usage_for_key("a"), Some(0x04));
        assert_eq!(hid_usage_for_key("g"), Some(0x0A));
        assert_eq!(hid_usage_for_key("z"), Some(0x1D));
        assert_eq!(hid_usage_for_key("1"), Some(0x1E));
        assert_eq!(hid_usage_for_key("9"), Some(0x26));
        assert_eq!(hid_usage_for_key("0"), Some(0x27));
        assert_eq!(hid_usage_for_key("-"), Some(0x2D));
        assert_eq!(hid_usage_for_key("="), Some(0x2E));
        // case-insensitive
        assert_eq!(hid_usage_for_key("G"), Some(0x0A));
        // function keys, both banks
        assert_eq!(hid_usage_for_key("f5"), Some(0x3E));
        assert_eq!(hid_usage_for_key("f13"), Some(0x68));
        assert_eq!(hid_usage_for_key("f24"), Some(0x73));
        assert_eq!(hid_usage_for_key("f"), Some(0x09)); // the LETTER f, not a function key
        // ...and case-insensitively for EVERY shape, not just single characters. `F5` is how a
        // person naturally writes a function key; it used to return None (silently falling back to
        // host-side dispatch) because case was folded in two places and the `f` prefix check saw the
        // un-normalized string.
        assert_eq!(hid_usage_for_key("F5"), Some(0x3E), "uppercase function keys must map");
        assert_eq!(hid_usage_for_key("F13"), Some(0x68));
        assert_eq!(hid_usage_for_key("F"), Some(0x09), "uppercase letter f is still the letter");
        assert_eq!(hid_usage_for_key("SPACE"), Some(0x2C), "uppercase named keys must map");
        assert_eq!(hid_usage_for_key("Up"), Some(0x52), "mixed-case named keys must map");
        // named keys + modifiers
        assert_eq!(hid_usage_for_key("space"), Some(0x2C));
        assert_eq!(hid_usage_for_key("up"), Some(0x52));
        assert_eq!(hid_usage_for_key("lctrl"), Some(0xE0));
        assert_eq!(hid_usage_for_key("shift"), Some(0xE1));
        // rejects
        assert_eq!(hid_usage_for_key("ctrl+s"), None, "chord");
        assert_eq!(hid_usage_for_key("volume-up"), None, "media");
        assert_eq!(hid_usage_for_key("num5"), None, "numpad");
        assert_eq!(hid_usage_for_key("f25"), None, "out of range fN");
        assert_eq!(hid_usage_for_key(""), None);
    }

    /// Every named VK round-trips: `vk_for(key_param_for_vk(vk)) == vk` over the full practical
    /// space — what press-to-bind capture writes is exactly what the engine presses. (The sided
    /// modifiers canonicalize: 0x5C right-win names as "win" -> 0x5B; that one pair is the
    /// deliberate exception, both sides press the same logical key.)
    #[test]
    fn key_params_round_trip_for_every_vk() {
        for vk in 1u16..256 {
            let name = key_param_for_vk(vk);
            let back = vk_for(&name);
            let expect = if vk == 0x5C { 0x5B } else { vk };
            assert_eq!(
                back,
                Some(expect),
                "vk 0x{vk:02X} named '{name}' did not round-trip"
            );
        }
    }

    #[test]
    fn vk_for_covers_the_practical_keyboard() {
        assert_eq!(vk_for("num7"), Some(0x67));
        assert_eq!(vk_for("num+"), Some(0x6B));
        assert_eq!(vk_for(";"), Some(0xBA));
        assert_eq!(vk_for("page-down"), Some(0x22));
        assert_eq!(vk_for("delete"), Some(0x2E));
        assert_eq!(vk_for("ralt"), Some(0xA5));
        assert_eq!(
            vk_for("0x73"),
            Some(0x73),
            "the transparent hex escape hatch"
        );
        assert_eq!(vk_for("f24"), Some(0x87));
        assert_eq!(vk_for("not-a-key"), None);
    }

    #[test]
    fn bound_mouse_button_parser_fails_closed() {
        assert_eq!(macro_mouse_button_kind("left"), Some(MouseButtonKind::Left));
        assert_eq!(macro_mouse_button_kind("x1"), Some(MouseButtonKind::Back));
        assert_eq!(macro_mouse_button_kind("forward"), Some(MouseButtonKind::Forward));
        assert_eq!(macro_mouse_button_kind("garbage"), None);
        assert_eq!(macro_mouse_button_kind(""), None);
    }

    #[test]
    fn combos_parse_and_protect_plus_keys() {
        assert_eq!(parse_combo("ctrl+shift+s"), (vec![0x11, 0x10], "s".into()));
        assert_eq!(parse_combo("ALT+F4"), (vec![0x12], "F4".into()));
        // names that legally CONTAIN '+' never mis-split…
        assert_eq!(parse_combo("num+"), (vec![], "num+".into()));
        assert_eq!(parse_combo("ctrl+num+"), (vec![0x11], "num+".into()));
        // …and a bare modifier is a key, not a chord of nothing.
        assert_eq!(parse_combo("shift"), (vec![], "shift".into()));
        assert_eq!(parse_combo("lctrl+x"), (vec![0xA2], "x".into()));
    }

    #[test]
    fn action_toml_round_trips() {
        let a = Action::Key { key: "f5".into() };
        let s = toml::to_string(&a).unwrap();
        assert!(s.contains("type = \"key\""));
        let back: Action = toml::from_str(&s).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn sequence_and_script_round_trip() {
        // A macro: tap F, hold A 50ms, then a 100ms pause.
        let seq = Action::Sequence {
            steps: vec![
                Step {
                    action: Box::new(Action::Key { key: "f".into() }),
                    delay_ms: 0,
                    hold_ms: 0,
                },
                Step {
                    action: Box::new(Action::Key { key: "a".into() }),
                    delay_ms: 100,
                    hold_ms: 50,
                },
            ],
        };
        let s = serde_json::to_string(&seq).unwrap();
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, seq);

        let script = Action::Script {
            script: ScriptRef {
                id: "open_cmd_here".into(),
                kind: ScriptKind::Python,
            },
        };
        let s = serde_json::to_string(&script).unwrap();
        assert!(s.contains("\"python\""));
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, script);
    }

    #[test]
    fn needs_context_only_true_for_macro_tiers() {
        // Plain host actions never read ctx — the dispatcher can skip the clipboard/window capture.
        assert!(!Action::Noop.needs_context());
        assert!(!Action::Key { key: "f".into() }.needs_context());
        assert!(!Action::DpiSet { dpi: 800 }.needs_context());
        assert!(!Action::MicMute {
            device: None,
            mode: "toggle".into()
        }
        .needs_context());
        // A Script reads ctx; a Sequence/Turbo containing one is transitively true.
        let script = Action::Script {
            script: ScriptRef {
                id: "m".into(),
                kind: ScriptKind::Python,
            },
        };
        assert!(script.needs_context());
        let seq = Action::Sequence {
            steps: vec![
                Step {
                    action: Box::new(Action::Noop),
                    delay_ms: 0,
                    hold_ms: 0,
                },
                Step {
                    action: Box::new(script.clone()),
                    delay_ms: 0,
                    hold_ms: 0,
                },
            ],
        };
        assert!(
            seq.needs_context(),
            "a Sequence with a Script step needs context"
        );
        // A Sequence of only plain steps does not.
        let plain = Action::Sequence {
            steps: vec![Step {
                action: Box::new(Action::Key { key: "a".into() }),
                delay_ms: 0,
                hold_ms: 0,
            }],
        };
        assert!(!plain.needs_context());
        assert!(Action::Turbo {
            action: Box::new(script),
            cps: 10
        }
        .needs_context());
    }

    #[test]
    fn new_variants_describe() {
        let seq = Action::Sequence {
            steps: vec![Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            }],
        };
        assert!(seq.describe().contains("macro"));
        let script = Action::Script {
            script: ScriptRef {
                id: "m1".into(),
                kind: ScriptKind::Shell,
            },
        };
        assert!(script.describe().contains("script"));
        assert!(script.describe().contains("shell"));
    }

    #[test]
    fn action_variants_parse_from_config() {
        let run: Action = toml::from_str(
            r#"type = "run"
cmd = "echo hi""#,
        )
        .unwrap();
        assert_eq!(
            run,
            Action::Run {
                cmd: "echo hi".into()
            }
        );
        let mute: Action = toml::from_str(r#"type = "mic-mute""#).unwrap();
        assert_eq!(
            mute,
            Action::MicMute {
                device: None,
                mode: "toggle".into()
            }
        );
    }

    #[test]
    fn vk_mapping_covers_keys_digits_fkeys_named() {
        assert_eq!(vk_for("a"), Some(0x41));
        assert_eq!(vk_for("Z"), Some(0x5A));
        assert_eq!(vk_for("1"), Some(0x31));
        assert_eq!(vk_for("f5"), Some(0x74));
        assert_eq!(vk_for("enter"), Some(0x0D));
        assert_eq!(vk_for("space"), Some(0x20));
        assert_eq!(vk_for("nonsense"), None);
    }

    #[test]
    fn describe_is_human() {
        assert_eq!(Action::Key { key: "g".into() }.describe(), "press [g]");
        assert!(Action::Run { cmd: "x".into() }.describe().contains("run"));
    }

    #[test]
    fn noop_run_and_run_ctx_agree() {
        // run() is run_ctx(&Context::default()); for context-free actions they're identical.
        let a = Action::Noop;
        assert_eq!(a.run(), "noop");
        assert_eq!(a.run_ctx(&Context::default()), "noop");
    }

    #[test]
    fn empty_sequence_runs_clean() {
        let seq = Action::Sequence { steps: vec![] };
        let r = seq.run();
        assert!(r.contains("0 steps"), "got: {r}");
    }

    #[test]
    fn sequence_summary_pluralizes() {
        let one = Action::Sequence {
            steps: vec![Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            }],
        };
        let r = one.run();
        assert!(r.contains("1 step") && !r.contains("1 steps"), "got: {r}");
    }

    #[test]
    fn sequence_runs_nested_noops_without_input() {
        // A macro of pure no-ops (no real input synthesized) must run end-to-end and report the right
        // step count. Drive the SYNCHRONOUS core directly (the live `run_ctx` spawns this on a worker
        // and returns "running …", so we test the walk itself here, deterministically).
        let steps = vec![
            Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            },
            Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            },
            Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            },
        ];
        assert_eq!(
            run_sequence_sync(&steps, &Context::default()),
            "ran macro (3 steps)"
        );
    }

    #[test]
    fn nested_sequence_in_sequence() {
        // A Sequence step whose action is itself a Sequence — the boxed recursion must execute inline
        // (only the OUTERMOST run_sequence spawns a worker; nested ones run synchronously here).
        let inner = Action::Sequence {
            steps: vec![Step {
                action: Box::new(Action::Noop),
                delay_ms: 0,
                hold_ms: 0,
            }],
        };
        let outer_steps = vec![Step {
            action: Box::new(inner),
            delay_ms: 0,
            hold_ms: 0,
        }];
        assert_eq!(
            run_sequence_sync(&outer_steps, &Context::default()),
            "ran macro (1 step)"
        );
    }

    #[test]
    fn vk_mapping_directions_and_fkey_bounds() {
        assert_eq!(vk_for("up"), Some(0x26));
        assert_eq!(vk_for("right"), Some(0x27));
        assert_eq!(vk_for("f24"), Some(0x87)); // 0x70 + 23
        assert_eq!(vk_for("f25"), None, "F25 is out of range");
        assert_eq!(vk_for("f0"), None, "there is no F0");
    }

    #[test]
    fn script_describe_covers_all_kinds() {
        for (kind, tag) in [
            (ScriptKind::Python, "python"),
            (ScriptKind::Shell, "shell"),
            (ScriptKind::File, "file"),
        ] {
            let s = Action::Script {
                script: ScriptRef {
                    id: "m".into(),
                    kind,
                },
            };
            assert!(s.describe().contains(tag), "describe missing tag {tag}");
        }
    }

    // ── new variants: serde round-trip ─────────────────────────────────────────────────────────

    /// Every new variant must serde round-trip (the config-row contract the GUI relies on).
    #[test]
    fn new_variants_round_trip() {
        let variants = vec![
            Action::MouseButton {
                button: MouseButtonKind::Middle,
            },
            Action::MouseButton {
                button: MouseButtonKind::Back,
            },
            Action::MouseButton {
                button: MouseButtonKind::ScrollLeft,
            },
            Action::Media {
                key: MediaKind::PlayPause,
            },
            Action::Media {
                key: MediaKind::VolumeMute,
            },
            Action::DpiCycle { dir: Direction::Up },
            Action::DpiSet { dpi: 1600 },
            Action::ScrollStageCycle {
                dir: Direction::Down,
            },
            Action::ProfileSwitch {
                name: "game".into(),
            },
            Action::ProfileCycle { dir: Direction::Up },
            Action::Turbo {
                action: Box::new(Action::MouseButton {
                    button: MouseButtonKind::Left,
                }),
                cps: 12,
            },
            Action::Control, // the control-center prime (unit variant, app intent)
        ];
        for a in variants {
            let s = serde_json::to_string(&a).unwrap();
            let back: Action = serde_json::from_str(&s).unwrap();
            assert_eq!(back, a, "json round-trip failed for {a:?}");
            // TOML too (the on-disk config format).
            let t = toml::to_string(&a).unwrap();
            let back: Action = toml::from_str(&t).unwrap();
            assert_eq!(back, a, "toml round-trip failed for {a:?}");
        }
    }

    /// The kebab-case tags the GUI/config use are stable.
    #[test]
    fn new_variant_tags_are_kebab_case() {
        let cases = [
            (
                Action::MouseButton {
                    button: MouseButtonKind::ScrollLeft,
                },
                "\"mouse-button\"",
                "\"scroll-left\"",
            ),
            (
                Action::Media {
                    key: MediaKind::VolumeUp,
                },
                "\"media\"",
                "\"volume-up\"",
            ),
            (
                Action::DpiCycle { dir: Direction::Up },
                "\"dpi-cycle\"",
                "\"up\"",
            ),
            (
                Action::ProfileSwitch { name: "x".into() },
                "\"profile-switch\"",
                "x",
            ),
        ];
        for (a, tag, payload) in cases {
            let s = serde_json::to_string(&a).unwrap();
            assert!(s.contains(tag), "{s} missing tag {tag}");
            assert!(s.contains(payload), "{s} missing payload {payload}");
        }
    }

    /// Daemon-handled variants expose the right `Intent`; host actions expose `None`.
    #[test]
    fn intent_maps_daemon_actions_only() {
        assert_eq!(
            Action::DpiCycle { dir: Direction::Up }.intent(),
            Some(Intent::DpiCycle(Direction::Up))
        );
        assert_eq!(
            Action::DpiSet { dpi: 800 }.intent(),
            Some(Intent::DpiSet(800))
        );
        assert_eq!(
            Action::ScrollStageCycle {
                dir: Direction::Down
            }
            .intent(),
            Some(Intent::ScrollStageCycle(Direction::Down))
        );
        assert_eq!(
            Action::ProfileSwitch { name: "g".into() }.intent(),
            Some(Intent::ProfileSwitch("g".into()))
        );
        assert_eq!(
            Action::ProfileCycle { dir: Direction::Up }.intent(),
            Some(Intent::ProfileCycle(Direction::Up))
        );
        // Host-executable actions have no daemon intent.
        assert_eq!(Action::Noop.intent(), None);
        assert_eq!(Action::Key { key: "f".into() }.intent(), None);
        assert_eq!(
            Action::MouseButton {
                button: MouseButtonKind::Left
            }
            .intent(),
            None
        );
        assert_eq!(
            Action::Media {
                key: MediaKind::Next
            }
            .intent(),
            None
        );
    }

    /// `turbo()` exposes (cps, inner) only for `Turbo`.
    #[test]
    fn turbo_exposes_rate_and_inner() {
        let inner = Action::Key { key: "f".into() };
        let t = Action::Turbo {
            action: Box::new(inner.clone()),
            cps: 20,
        };
        let (cps, got) = t.turbo().expect("turbo exposes rate");
        assert_eq!(cps, 20);
        assert_eq!(*got, inner);
        assert!(Action::Noop.turbo().is_none());
    }

    /// `run()` smoke: every new variant runs without panicking and reports something sensible.
    /// (On non-Windows the synth actions report "windows-only" but still don't panic.) The
    /// daemon-intent variants must NOT claim to have done device work — they report an "intent".
    #[test]
    fn new_variants_run_smoke() {
        // Daemon-handled actions report an intent, never a fake success.
        assert!(Action::DpiCycle { dir: Direction::Up }
            .run()
            .contains("intent"));
        assert!(Action::DpiSet { dpi: 1600 }.run().contains("intent"));
        assert!(Action::ScrollStageCycle {
            dir: Direction::Down
        }
        .run()
        .contains("intent"));
        assert!(Action::ProfileSwitch { name: "g".into() }
            .run()
            .contains("intent"));
        assert!(Action::ProfileCycle {
            dir: Direction::Down
        }
        .run()
        .contains("intent"));
        // Host actions run (string is platform-dependent but non-empty + mentions the action).
        assert!(Action::Media {
            key: MediaKind::PlayPause
        }
        .run()
        .contains("media"));
        assert!(Action::MouseButton {
            button: MouseButtonKind::Middle
        }
        .run()
        .contains("mouse"));
        // A turbo of a Noop fires the inner once (the single-press fallback).
        assert_eq!(
            Action::Turbo {
                action: Box::new(Action::Noop),
                cps: 10
            }
            .run(),
            "noop"
        );
    }

    /// The `describe()` labels for the new variants are human-readable.
    #[test]
    fn new_variants_describe_is_human() {
        assert!(Action::MouseButton {
            button: MouseButtonKind::Back
        }
        .describe()
        .contains("back"));
        assert!(Action::Media {
            key: MediaKind::VolumeMute
        }
        .describe()
        .contains("mute"));
        assert!(Action::DpiSet { dpi: 3200 }.describe().contains("3200"));
        assert!(Action::ProfileSwitch { name: "fps".into() }
            .describe()
            .contains("fps"));
        let t = Action::Turbo {
            action: Box::new(Action::MouseButton {
                button: MouseButtonKind::Left,
            }),
            cps: 15,
        };
        let d = t.describe();
        assert!(
            d.contains("turbo") && d.contains("15") && d.contains("left"),
            "got: {d}"
        );
    }

    /// The migration importer emits `Key { key: "media-*" }`; those names must resolve to a VK now
    /// (previously "unknown key"), and the typed `MediaKind` agrees with that VK.
    #[test]
    fn media_key_names_resolve_and_match_typed() {
        assert_eq!(vk_for("media-play-pause"), Some(0xB3));
        assert_eq!(vk_for("volume-up"), Some(0xAF));
        assert_eq!(vk_for("volume-mute"), Some(0xAD));
        assert_eq!(
            MediaKind::from_key_name("media-play-pause"),
            Some(MediaKind::PlayPause)
        );
        assert_eq!(
            MediaKind::from_key_name("volume-up"),
            Some(MediaKind::VolumeUp)
        );
        assert_eq!(MediaKind::from_key_name("not-media"), None);
        // typed VK and the by-name VK agree.
        assert_eq!(
            MediaKind::PlayPause.vk(),
            vk_for("media-play-pause").unwrap()
        );
    }

    /// `Direction::step` is the +1/-1 the daemon applies to a cycling index.
    #[test]
    fn direction_step_and_label() {
        assert_eq!(Direction::Up.step(), 1);
        assert_eq!(Direction::Down.step(), -1);
        assert_eq!(Direction::Up.label(), "up");
    }

    /// `Action::Run` (and the macro Shell/File tiers, via the same `process_spawn_armed` gate) must
    /// NOT spawn a process while DISARMED — which is the state every test runs in (no test ever
    /// arms input). This closes the verify-audit gap where `Run`/`Shell` bypassed the arm gate.
    /// The test asserts the disarmed report without ever arming (preserving the input-safety rule).
    #[test]
    fn run_action_is_gated_disarmed_in_tests() {
        // Default process-spawn gate is OFF (tests never arm).
        assert!(!process_spawn_armed(), "tests must run disarmed");
        // A command that would be obvious if it actually ran; gated => reports [disarmed], spawns
        // nothing.
        let r = Action::Run {
            cmd: "echo neuron-should-not-run".into(),
        }
        .run();
        assert!(r.contains("[disarmed]"), "disarmed Run must not spawn: {r}");
    }

    /// EVERY `Action` variant must serde round-trip through BOTH JSON and TOML (the config-row
    /// contract the GUI + importer rely on). This is the exhaustive sibling of
    /// `new_variants_round_trip`: it covers the audio variants — `MicMute`/`MicGain` and the NEW
    /// `OutputMute`/`OutputGain` — plus `Noop`, which the per-feature tests above omit. A new
    /// variant added to `Action` without a round-trip case here should make the maintainer notice
    /// (this list is the canonical "did you serde-test it?" ledger).
    #[test]
    fn every_action_variant_round_trips_json_and_toml() {
        let variants = vec![
            Action::Noop,
            Action::Run {
                cmd: "echo hi".into(),
            },
            Action::Key { key: "f".into() },
            Action::MicMute {
                device: None,
                mode: "toggle".into(),
            },
            Action::MicMute {
                device: Some("seiren".into()),
                mode: "on".into(),
            },
            Action::MicGain {
                device: None,
                delta_pct: 4.0,
            },
            Action::MicGain {
                device: Some("seiren".into()),
                delta_pct: -4.0,
            },
            Action::OutputMute {
                device: None,
                mode: "toggle".into(),
            },
            Action::OutputMute {
                device: Some("razer".into()),
                mode: "off".into(),
            },
            Action::OutputGain {
                device: None,
                delta_pct: 5.0,
            },
            Action::OutputGain {
                device: Some("razer".into()),
                delta_pct: -2.5,
            },
            Action::Sequence {
                steps: vec![Step {
                    action: Box::new(Action::Key { key: "a".into() }),
                    delay_ms: 10,
                    hold_ms: 5,
                }],
            },
            Action::Script {
                script: ScriptRef {
                    id: "m1".into(),
                    kind: ScriptKind::Python,
                },
            },
            Action::Script {
                script: ScriptRef {
                    id: "m2".into(),
                    kind: ScriptKind::Shell,
                },
            },
            Action::Script {
                script: ScriptRef {
                    id: "m3".into(),
                    kind: ScriptKind::File,
                },
            },
            Action::MouseButton {
                button: MouseButtonKind::Middle,
            },
            Action::Media {
                key: MediaKind::PlayPause,
            },
            Action::DpiCycle { dir: Direction::Up },
            Action::DpiSet { dpi: 1600 },
            Action::ScrollStageCycle {
                dir: Direction::Down,
            },
            Action::ProfileSwitch { name: "fps".into() },
            Action::ProfileCycle { dir: Direction::Up },
            Action::Turbo {
                action: Box::new(Action::MouseButton {
                    button: MouseButtonKind::Left,
                }),
                cps: 12,
            },
        ];
        for a in variants {
            let j = serde_json::to_string(&a).unwrap();
            let back: Action = serde_json::from_str(&j).unwrap();
            assert_eq!(back, a, "json round-trip failed for {a:?}");
            let t = toml::to_string(&a).unwrap();
            let back: Action = toml::from_str(&t).unwrap();
            assert_eq!(back, a, "toml round-trip failed for {a:?}");
        }
    }

    /// The NEW output-audio variants (`OutputMute`/`OutputGain`) carry the same shape as their mic
    /// siblings: an optional `device` name substring + a mode/delta. Defaults must parse (a bare
    /// `output-mute` = toggle the active output), and the kebab-case tags are stable for the config.
    #[test]
    fn output_audio_variants_parse_defaults_and_tags() {
        // Bare output-mute defaults to "toggle" and the system-active output (device = None).
        let mute: Action = toml::from_str(r#"type = "output-mute""#).unwrap();
        assert_eq!(
            mute,
            Action::OutputMute {
                device: None,
                mode: "toggle".into()
            }
        );
        // Explicit device + mode.
        let mute2: Action =
            toml::from_str("type = \"output-mute\"\ndevice = \"razer\"\nmode = \"off\"").unwrap();
        assert_eq!(
            mute2,
            Action::OutputMute {
                device: Some("razer".into()),
                mode: "off".into()
            }
        );
        // Output-gain requires its delta; device optional.
        let gain: Action = toml::from_str("type = \"output-gain\"\ndelta_pct = 5.0").unwrap();
        assert_eq!(
            gain,
            Action::OutputGain {
                device: None,
                delta_pct: 5.0
            }
        );
        // Stable kebab-case tags.
        let j = serde_json::to_string(&Action::OutputMute {
            device: None,
            mode: "toggle".into(),
        })
        .unwrap();
        assert!(j.contains("\"output-mute\""), "tag: {j}");
        let j = serde_json::to_string(&Action::OutputGain {
            device: None,
            delta_pct: 1.0,
        })
        .unwrap();
        assert!(j.contains("\"output-gain\""), "tag: {j}");
    }

    /// `describe()` for the output-audio variants is human-readable and reflects mode/sign/device.
    #[test]
    fn output_audio_variants_describe_is_human() {
        assert!(Action::OutputMute {
            device: None,
            mode: "off".into()
        }
        .describe()
        .contains("output mute"));
        assert!(Action::OutputMute {
            device: None,
            mode: "off".into()
        }
        .describe()
        .contains("off"));
        let g = Action::OutputGain {
            device: Some("razer".into()),
            delta_pct: 5.0,
        }
        .describe();
        assert!(g.contains("output vol"), "got: {g}");
        assert!(g.contains("+5"), "shows the signed delta: {g}");
        assert!(g.contains("razer"), "names the device: {g}");
    }

    /// The audio variants are HOST-executed (no daemon cooperation), so `intent()` is `None` for all
    /// of them — they fully run in `run_ctx`. Pinned so a future refactor can't silently turn an
    /// audio action into a daemon-routed one (which would make it a no-op outside the daemon).
    #[test]
    fn audio_variants_have_no_daemon_intent() {
        assert_eq!(
            Action::MicMute {
                device: None,
                mode: "toggle".into()
            }
            .intent(),
            None
        );
        assert_eq!(
            Action::MicGain {
                device: None,
                delta_pct: 1.0
            }
            .intent(),
            None
        );
        assert_eq!(
            Action::OutputMute {
                device: None,
                mode: "toggle".into()
            }
            .intent(),
            None
        );
        assert_eq!(
            Action::OutputGain {
                device: None,
                delta_pct: 1.0
            }
            .intent(),
            None
        );
    }

    /// `run()` smoke for the output-audio variants: they must not panic and report something
    /// sensible. On a CI box with no audio endpoint they report "no output device"; on a real box
    /// they report the endpoint + new state. Either way the call is infallible and mentions output.
    /// (Audio control is a reversible OS setting, not a gated device write, so this is safe to call —
    /// but on a headless runner there's simply no render endpoint, so it no-ops cleanly.)
    #[test]
    fn output_audio_variants_run_smoke() {
        let r = Action::OutputMute {
            device: Some("\u{0}no-such-device\u{0}".into()),
            mode: "toggle".into(),
        }
        .run();
        assert!(!r.is_empty(), "output-mute run reports something");
        let r = Action::OutputGain {
            device: Some("\u{0}no-such-device\u{0}".into()),
            delta_pct: 1.0,
        }
        .run();
        assert!(!r.is_empty(), "output-gain run reports something");
    }

    /// A turbo wrapping a sequence still round-trips and reports its inner correctly (turbo is
    /// composable over any action, per the model decision).
    #[test]
    fn turbo_over_sequence_round_trips() {
        let seq = Action::Sequence {
            steps: vec![Step {
                action: Box::new(Action::Key { key: "a".into() }),
                delay_ms: 10,
                hold_ms: 0,
            }],
        };
        let t = Action::Turbo {
            action: Box::new(seq),
            cps: 5,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: Action = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(t.turbo().unwrap().0, 5);
    }
}
