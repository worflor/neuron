//! The editors — turn UI selections into real config on disk. Press-to-bind authored rules, the
//! per-sector radial editor, gesture->action binding, sniper, and the surfaced perf controls all
//! write the SAME plain TOML the CLI + live runtime read. No hardcoded buttons; nothing faked.
//!
//! Authored spine bindings live in `profiles/gui.rules.toml` — a `Vec<Rule>` sidecar exactly like
//! the migration importer writes, so `controls::load_rule_sidecars` (and thus `build_runtime`, the
//! live dispatch path) picks them up verbatim, HyperShift layer tags and all. This is strictly more
//! powerful than `bindings.toml` (the full typed `Action` enum, not 4 string actions), and keeps the
//! GUI's authored binds in one inspectable file.

use neuron::action::{Action, Direction, MediaKind, MouseButtonKind, Step};
use neuron::cast::CastConfig;
use neuron::engine::{Rule, Trigger};

/// The Action palette the picker offers — THE one source for every editor (bindings / wedges /
/// glyphs): `(id, label, param_hint, group)`. `param_hint` empty = no field; `group` is the
/// section header the picker renders (consecutive same-group entries share one). If the engine
/// can do it, it belongs here — the palette must never be narrower than the spine.
/// `(id, label, param_hint, group, required, tier)` — see notes below the array.
/// `required`: a blank param won't commit. `tier`: maturity 0=placeholder · 1-4=WIP · 5=done.
pub const ACTION_PALETTE: &[(&str, &str, &str, &str, bool, u8)] = &[
    (
        "key",
        "press a key",
        "f5 · ctrl+shift+s · esc",
        "press & type",
        true,
        5,
    ),
    (
        "mouse",
        "mouse button",
        "(left) · right · middle · back · forward",
        "press & type",
        false,
        5,
    ),
    (
        "turbo",
        "turbo a key",
        "key · cps  (e.g. f · 12)",
        "press & type",
        true,
        5,
    ),
    (
        "media",
        "media key",
        "(play-pause) · next · prev · vol-up · vol-down · mute",
        "press & type",
        false,
        5,
    ),
    (
        "keys",
        "record a key sequence",
        "w:180 ~90 a space  ·  :hold ~pause, ms",
        "press & type",
        true,
        4,
    ),
    (
        "ghost-paste",
        "ghost-paste",
        "instant · fast · normal",
        "clipboard",
        false,
        4,
    ),
    (
        "pocket",
        "pocket",
        "name · keep = persist",
        "clipboard",
        false,
        4,
    ),
    ("macro", "python macro", "macro name", "run & code", true, 4),
    (
        "run",
        "run a command",
        "command line",
        "run & code",
        true,
        5,
    ),
    ("echo", "echo", "", "run & code", false, 5),
    ("whiteboard", "whiteboard", "", "instruments", false, 5),
    ("knockback", "knockback", "", "instruments", false, 2),
    ("teleport", "teleport", "", "instruments", false, 5),
    ("control", "control center", "", "instruments", false, 3),
    ("curtain", "curtain", "", "instruments", false, 5),
    (
        "summon",
        "summon a window",
        "window  ·  here | toggle  (blank = focus)",
        "windows",
        true,
        5,
    ),
    (
        "glance",
        "glance a window",
        "window title or exe  (e.g. obs)",
        "windows",
        true,
        5,
    ),
    (
        "banish",
        "banish a window",
        "(focused) · hover · behind",
        "windows",
        false,
        5,
    ),
    (
        "pin",
        "pin a window",
        "(focused) · hover",
        "windows",
        false,
        5,
    ),
    (
        "kill",
        "kill a window",
        "(focused) · hover",
        "windows",
        false,
        5,
    ),
    (
        "tether",
        "tether",
        "name · wormhole",
        "windows",
        false,
        4,
    ),
    (
        "system",
        "system",
        "(lock) · sleep",
        "system",
        false,
        4,
    ),
    // CONSOLIDATED obs — one entry, the op lives in the param (like system/volume/mute). Needs
    // SYSTEM → CONNECTIONS with the obs gate on; fires honestly-unavailable otherwise.
    (
        "obs",
        "obs",
        "(stream) · record · pause · replay · scene name · mute input",
        "obs",
        false,
        5,
    ),
    (
        "volume",
        "volume",
        "output | mic  · +4 / -4 / dial",
        "audio",
        false,
        5,
    ),
    (
        "mute",
        "mute",
        "(output) | mic  · on / off",
        "audio",
        false,
        5,
    ),
    (
        "momentary-mic",
        "momentary mic",
        "(flip) · talk · mute   [· device]",
        "audio",
        false,
        2,
    ),
    (
        "output-flip",
        "flip output",
        "(blank = cycle all)  headset · speakers",
        "audio",
        false,
        5,
    ),
    ("dpi", "dpi", "800  |  up / down", "device", false, 4),
    (
        "scroll-stage",
        "scroll stage",
        "(up) · down",
        "device",
        false,
        4,
    ),
    (
        "profile",
        "profile",
        "name  |  up / down",
        "profiles",
        false,
        5,
    ),
    ("noop", "unbind", "", "", false, 5),
];

/// Build a typed [`Action`] from a palette id + the single parameter string. Returns `Noop` for an
/// unknown id, and tolerates loose parameter formats (the UI shows the hint). Pure + testable.
pub fn build_action(id: &str, param: &str) -> Action {
    let p = param.trim();
    match id {
        "key" => Action::Key { key: p.to_string() },
        "run" => Action::Run { cmd: p.to_string() },
        // CONSOLIDATED volume: nudge (a ±step) OR the analog dial, for output OR mic — one entry, the
        // submode lives in the param. See build_volume.
        "volume" => build_volume(p),
        // CONSOLIDATED mute: output OR mic, toggle/on/off — one entry. (Hold-to-talk lives in its own
        // `momentary-mic` entry.) See build_mute.
        "mute" => build_mute(p),
        "media" => Action::Media {
            key: parse_media(p),
        },
        "mouse" => Action::MouseButton {
            button: parse_mouse(p),
        },
        // "key · cps": autofire the key while the trigger is held (cps defaults to 10).
        "turbo" => {
            let (key, cps) = split_device(p);
            Action::Turbo {
                action: Box::new(Action::Key { key }),
                cps: cps
                    .and_then(|c| c.parse::<u16>().ok())
                    .unwrap_or(10)
                    .clamp(1, 50),
            }
        }
        // CONSOLIDATED dpi: a number SETS an absolute DPI; "up"/"down"/blank CYCLES the stage list
        // (blank = the next stage up).
        "dpi" => match p.parse::<u16>() {
            Ok(v) => Action::DpiSet { dpi: v },
            Err(_) => Action::DpiCycle { dir: parse_dir(p) },
        },
        "scroll-stage" => Action::ScrollStageCycle { dir: parse_dir(p) },
        // CONSOLIDATED profile: a NAME switches to it; "up"/"down"/blank CYCLES (blank = next).
        "profile" => {
            let l = p.to_lowercase();
            if p.is_empty() || l == "up" || l == "down" || l == "next" || l == "prev" {
                Action::ProfileCycle { dir: parse_dir(p) }
            } else {
                Action::ProfileSwitch {
                    name: p.to_string(),
                }
            }
        }
        "teleport" => Action::Teleport,
        "whiteboard" => Action::Whiteboard,
        "knockback" => Action::Knockback,
        "control" => Action::Control,
        "glance" => Action::Glance {
            target: p.to_string(),
        },
        "summon" => {
            // "window · mode" — the window matcher, then the delivery mode (default focus).
            let (window, mode) = match p.split_once('\u{00b7}') {
                Some((w, m)) => (w.trim().to_string(), neuron::action::SummonMode::parse(m)),
                None => (p.to_string(), neuron::action::SummonMode::Focus),
            };
            Action::Summon { window, mode }
        }
        "banish" => Action::Banish {
            pick: neuron::action::WindowPick::parse(p),
        },
        "pin" => Action::Pin {
            // pin doesn't sweep — "behind" degrades to the focused window
            pick: match neuron::action::WindowPick::parse(p) {
                neuron::action::WindowPick::Behind => neuron::action::WindowPick::Focused,
                other => other,
            },
        },
        // kill doesn't sweep either — "behind" degrades to focused (no mass-terminate footgun)
        "kill" => Action::Kill {
            pick: match neuron::action::WindowPick::parse(p) {
                neuron::action::WindowPick::Behind => neuron::action::WindowPick::Focused,
                other => other,
            },
        },
        "echo" => Action::Echo,
        // CURTAIN: a panic privacy screen (overlay, not a power action). Its own top-level entry now,
        // sitting with the other instruments — it is NOT a system action.
        "curtain" => Action::Curtain,
        // CONSOLIDATED system — the real power verbs only: "sleep"/"suspend" = suspend, anything else
        // (incl. blank) = lock the session. (Curtain used to live here as "monitors off"; it left.)
        "system" => match p.to_lowercase().as_str() {
            "sleep" | "suspend" => Action::Sleep,
            _ => Action::Lock,
        },
        // CONSOLIDATED obs — first word picks the op, the rest is its argument:
        // (blank)/"stream [start|stop]" · "record [start|stop]" · "pause" · "replay [start|stop]"
        // · "scene <name>" · "mute [input]". Anything unrecognized degrades to the stream toggle
        // (harmless and visible) rather than Noop (a dead knob).
        "obs" => build_obs(p),
        "pocket" => {
            // "name" or "name · keep" — the second token enables on-disk persistence.
            let mut parts = p.split('\u{00b7}').map(str::trim);
            let slot = parts.next().unwrap_or("").to_string();
            let persist = parts.any(|t| matches!(t, "keep" | "persist" | "durable"));
            Action::Pocket { slot, persist }
        }
        // "name · wormhole" / just "wormhole" / just "name" / blank. The default (Mark) is the
        // automagical warpstone — one press marks / warps back / releases, zero config. The word
        // "wormhole" anywhere switches to the two-anchor portal (swap A⇄B); the name is optional.
        "tether" => {
            use neuron::action::TetherMode;
            let lower = p.to_lowercase();
            let mode =
                if lower.contains("wormhole") || lower.contains("portal") || lower.contains("swap")
                {
                    TetherMode::Wormhole
                } else {
                    TetherMode::Mark
                };
            let slot = p
                .split('\u{00b7}')
                .flat_map(|seg| seg.split_whitespace())
                .filter(|t| {
                    !matches!(
                        t.to_ascii_lowercase().as_str(),
                        "wormhole" | "portal" | "swap" | "mark"
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            Action::Tether { slot, mode }
        }
        "ghost-paste" => Action::GhostPaste {
            speed: neuron::action::GhostSpeed::parse(p),
        },
        "momentary-mic" => {
            // "mode · device" — the flip behaviour, then an optional mic-name substring.
            let (mode, device) = match p.split_once('\u{00b7}') {
                Some((m, d)) => {
                    let d = d.trim();
                    (
                        neuron::action::MomentaryMode::parse(m),
                        (!d.is_empty()).then(|| d.to_string()),
                    )
                }
                None => (neuron::action::MomentaryMode::parse(p), None),
            };
            Action::MomentaryMic { device, mode }
        }
        "output-flip" => Action::OutputFlip {
            // "headset · speakers · …" — the device set to cycle (blank = every connected output).
            devices: p
                .split('\u{00b7}')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        },
        // the recorder's grammar: a timed key sequence as plain readable text (see parse_keyseq).
        "keys" => Action::Sequence {
            steps: parse_keyseq(p),
        },
        // draw a rune, run code: a warm Macro Host macro by name — the full python tier behind any
        // trigger the spine knows (glyph, wedge, button, app rule).
        "macro" => Action::Script {
            script: neuron::action::ScriptRef {
                id: p.to_string(),
                kind: neuron::action::ScriptKind::Python,
            },
        },
        _ => Action::Noop,
    }
}

fn parse_media(s: &str) -> MediaKind {
    match s.to_lowercase().replace('_', "-").as_str() {
        "next" | "next-track" => MediaKind::Next,
        "prev" | "previous" | "prev-track" => MediaKind::Prev,
        "stop" => MediaKind::Stop,
        "vol-up" | "volume-up" | "vol+" => MediaKind::VolumeUp,
        "vol-down" | "volume-down" | "vol-" => MediaKind::VolumeDown,
        "mute" | "vol-mute" | "volume-mute" => MediaKind::VolumeMute,
        _ => MediaKind::PlayPause,
    }
}

fn parse_mouse(s: &str) -> MouseButtonKind {
    match s.to_lowercase().as_str() {
        "right" => MouseButtonKind::Right,
        "middle" => MouseButtonKind::Middle,
        "back" | "x1" => MouseButtonKind::Back,
        "forward" | "x2" => MouseButtonKind::Forward,
        "scroll-left" => MouseButtonKind::ScrollLeft,
        "scroll-right" => MouseButtonKind::ScrollRight,
        _ => MouseButtonKind::Left,
    }
}

/// Split an output-action parameter into its `(value, optional device-name substring)`. The device
/// is whatever follows a `·` (middle-dot) separator, so "+4 · razer" targets the Razer sound card
/// and a bare "+4" targets the system default output. An empty device resolves to `None`.
fn split_device(p: &str) -> (String, Option<String>) {
    match p.split_once('·') {
        Some((val, dev)) => {
            let dev = dev.trim();
            (
                val.trim().to_string(),
                (!dev.is_empty()).then(|| dev.to_string()),
            )
        }
        None => (p.trim().to_string(), None),
    }
}

/// Parse the SEQUENCE grammar — the macro recorder's output and the hand-editable form of
/// [`Action::Sequence`]. Whitespace-separated tokens:
///   `w`        tap the key (chords fine: `ctrl+s`)
///   `w:180`    hold the key 180 ms (a REAL down-wait-up hold)
///   `~90`      pause 90 ms before the next step
///   `left`     mouse names tap the button
/// The recorder writes this from your actual presses with your actual timing; you can then trim
/// or retime it by hand — the macro is text, never a black box.
pub fn parse_keyseq(p: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for tok in p.split_whitespace() {
        if let Some(ms) = tok.strip_prefix('~') {
            let ms = ms.parse::<u32>().unwrap_or(0);
            // a pause rides the previous step's delay; a LEADING pause gets a noop carrier.
            match steps.last_mut() {
                Some(s) => s.delay_ms += ms,
                None => steps.push(Step {
                    action: Box::new(Action::Noop),
                    delay_ms: ms,
                    hold_ms: 0,
                }),
            }
            continue;
        }
        let (name, hold) = match tok.rsplit_once(':') {
            Some((n, h)) if h.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => {
                (n, h.parse::<u32>().unwrap_or(0))
            }
            _ => (tok, 0),
        };
        let action = match mouse_token(name) {
            Some(b) => Action::MouseButton { button: b },
            None => Action::Key {
                key: name.to_string(),
            },
        };
        steps.push(Step {
            action: Box::new(action),
            delay_ms: 0,
            hold_ms: hold,
        });
    }
    steps
}

/// Inverse of [`parse_keyseq`] — render a Sequence back to the grammar, if every step is
/// representable (Key / mouse-button / noop-pause steps). `None` for richer sequences (config-only).
pub fn keyseq_to_param(steps: &[Step]) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for s in steps {
        match s.action.as_ref() {
            Action::Key { key } => {
                if key.contains(char::is_whitespace) || key.contains('~') {
                    return None; // unrepresentable in the whitespace grammar
                }
                out.push(if s.hold_ms > 0 {
                    format!("{key}:{}", s.hold_ms)
                } else {
                    key.clone()
                });
            }
            Action::MouseButton { button } => {
                let name = match button {
                    MouseButtonKind::Left => "lclick",
                    MouseButtonKind::Right => "rclick",
                    MouseButtonKind::Middle => "middle",
                    MouseButtonKind::Back => "mouse4",
                    MouseButtonKind::Forward => "mouse5",
                    MouseButtonKind::ScrollLeft => "scroll-left",
                    MouseButtonKind::ScrollRight => "scroll-right",
                };
                out.push(if s.hold_ms > 0 {
                    format!("{name}:{}", s.hold_ms)
                } else {
                    name.into()
                });
            }
            Action::Noop if s.hold_ms == 0 => {} // pause carrier; the delay below emits it
            _ => return None,
        }
        if s.delay_ms > 0 {
            out.push(format!("~{}", s.delay_ms));
        }
    }
    Some(out.join(" "))
}

/// A sequence token that names a mouse button (vs a key). STRICT names only — a key called
/// "left" (arrow) must stay a key, so the arrow keys win and mouse buttons use their full names.
fn mouse_token(s: &str) -> Option<MouseButtonKind> {
    Some(match s.to_lowercase().as_str() {
        "lclick" | "left-click" => MouseButtonKind::Left,
        "rclick" | "right-click" => MouseButtonKind::Right,
        "mclick" | "middle-click" | "middle" => MouseButtonKind::Middle,
        "mouse4" | "back-click" => MouseButtonKind::Back,
        "mouse5" | "forward-click" => MouseButtonKind::Forward,
        "scroll-left" => MouseButtonKind::ScrollLeft,
        "scroll-right" => MouseButtonKind::ScrollRight,
        _ => return None,
    })
}

fn parse_dir(s: &str) -> Direction {
    match s.to_lowercase().as_str() {
        "down" | "-" | "prev" => Direction::Down,
        _ => Direction::Up,
    }
}

/// `volume` palette entry → a discrete step (`Gain`) or the analog slide (`Dial`), for output OR mic.
/// Grammar (order-free, `·`-separates an optional device): `output|mic` picks the target (default
/// output); a `±number` is a step; the word `dial`/`slide` or NO amount is the continuous knob.
fn build_volume(p: &str) -> Action {
    use neuron::action::DialTarget;
    let (target_amt, device) = split_device(p);
    let mic = target_amt
        .split_whitespace()
        .any(|w| w.eq_ignore_ascii_case("mic"));
    let amt = target_amt
        .split_whitespace()
        .filter(|w| {
            !matches!(
                w.to_ascii_lowercase().as_str(),
                "mic" | "output" | "out" | "speaker" | "speakers"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let amt = amt.trim();
    if amt.is_empty() || amt.eq_ignore_ascii_case("dial") || amt.eq_ignore_ascii_case("slide") {
        Action::Dial {
            target: if mic {
                DialTarget::MicVolume
            } else {
                DialTarget::OutputVolume
            },
        }
    } else if mic && amt.starts_with('=') {
        Action::MicGainSet {
            device,
            pct: amt.trim_start_matches('=').parse::<f32>().unwrap_or(0.0),
        }
    } else {
        let delta = amt.trim_start_matches('+').parse::<f32>().unwrap_or(0.0);
        if mic {
            Action::MicGain {
                device,
                delta_pct: delta,
            }
        } else {
            Action::OutputGain {
                device,
                delta_pct: delta,
            }
        }
    }
}

/// `mute` palette entry → `MicMute` / `OutputMute`. Grammar (order-free, `·`-separates an optional
/// device): `output|mic` picks the target (default output); `on`/`off`/(blank)→`toggle`. The held
/// push-to-talk lives in its own `momentary-mic` entry, so this stays a clean static toggle.
fn build_mute(p: &str) -> Action {
    let (target_mode, device) = split_device(p);
    let toks: Vec<String> = target_mode
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let mic = toks.iter().any(|t| t == "mic");
    let mode = if toks.iter().any(|t| t == "on") {
        "on"
    } else if toks.iter().any(|t| t == "off") {
        "off"
    } else {
        "toggle"
    }
    .to_string();
    if mic {
        Action::MicMute { device, mode }
    } else {
        Action::OutputMute { device, mode }
    }
}

/// The consolidated `obs` grammar: first word picks the op, the remainder is its argument.
/// Tolerant like every builder (the strict front door is `validate_action`): an unrecognized
/// first word degrades to the stream toggle — visible and harmless, never a dead Noop.
fn build_obs(p: &str) -> Action {
    use neuron::action::ObsOp;
    let (head, rest) = match p.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (p, ""),
    };
    let (op, arg) = match head.to_ascii_lowercase().as_str() {
        "" | "stream" => (ObsOp::Stream, rest),
        "record" => (ObsOp::Record, rest),
        "pause" | "record-pause" => (ObsOp::RecordPause, ""),
        "replay" | "clip" => (ObsOp::Replay, rest),
        "scene" => (ObsOp::Scene, rest),
        "mute" => (ObsOp::Mute, rest),
        _ => (ObsOp::Stream, ""),
    };
    Action::Obs {
        op,
        arg: arg.to_string(),
    }
}

/// Validate a palette selection BEFORE building/committing it — the "no silently-broken rules"
/// gate every editor flow (add-binding / wedge / glyph) calls first. `build_action` stays tolerant
/// (it must parse whatever is already on disk); this is the strict front door for NEW input.
pub fn validate_action(id: &str, param: &str) -> Result<(), String> {
    let p = param.trim();
    // the palette's `required` flag is the single source of truth for "blank won't commit" — the
    // optional ones (volume, mute, tether, banish, …) default sensibly, so a blank is fine.
    let entry = ACTION_PALETTE.iter().find(|t| t.0 == id);
    let hint = entry.map(|t| t.2).unwrap_or("");
    let required = entry.map(|t| t.4).unwrap_or(false);
    if required && p.is_empty() {
        return Err(format!("this action needs a parameter — {hint}"));
    }
    match id {
        // a NUMBER sets an absolute DPI; up/down/blank cycles — all valid; only junk errors.
        "dpi" => {
            let l = p.to_lowercase();
            if p.is_empty() || l == "up" || l == "down" || l == "next" || l == "prev" {
                Ok(())
            } else {
                match p.parse::<u16>() {
                    Ok(v) if (100..=30_000).contains(&v) => Ok(()),
                    _ => Err("dpi: a number 100–30000, or up / down".into()),
                }
            }
        }
        // summon needs a window to find; the mode after the · is optional.
        "summon" => {
            let window = p.split_once('\u{00b7}').map(|(w, _)| w.trim()).unwrap_or(p);
            if window.is_empty() {
                Err("summon needs a window (title or exe) to find".into())
            } else {
                Ok(())
            }
        }
        // a key the engine can't resolve to a VK would bind-then-do-nothing; reject it up front.
        "key" => check_key(p),
        "keys" => {
            let steps = parse_keyseq(p);
            if steps
                .iter()
                .all(|s| matches!(s.action.as_ref(), Action::Noop))
            {
                return Err(
                    "the sequence needs at least one key — RECORD it or type tokens".into(),
                );
            }
            for s in &steps {
                if let Action::Key { key } = s.action.as_ref() {
                    check_key(key).map_err(|e| format!("in the sequence: {e}"))?;
                }
            }
            Ok(())
        }
        "turbo" => {
            let (key, cps) = split_device(p);
            if key.is_empty() {
                return Err("turbo needs a key, like  f · 12".into());
            }
            check_key(&key)?;
            match cps {
                None => Ok(()), // bare key = default 10 cps
                Some(c) => match c.parse::<u16>() {
                    Ok(v) if (1..=50).contains(&v) => Ok(()),
                    _ => Err("turbo cps must be 1–50".into()),
                },
            }
        }
        // volume: a STEP needs a non-zero ±number; "dial"/blank (the slide) is always fine.
        "volume" => {
            let (target_amt, _) = split_device(p);
            let amt = target_amt
                .split_whitespace()
                .filter(|w| {
                    !matches!(
                        w.to_ascii_lowercase().as_str(),
                        "mic" | "output" | "out" | "speaker" | "speakers"
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            let amt = amt.trim();
            if amt.is_empty()
                || amt.eq_ignore_ascii_case("dial")
                || amt.eq_ignore_ascii_case("slide")
            {
                Ok(())
            } else if amt.starts_with('=') {
                if target_amt
                    .split_whitespace()
                    .any(|w| w.eq_ignore_ascii_case("mic"))
                {
                    match amt.trim_start_matches('=').parse::<f32>() {
                        Ok(v) if (0.0..=100.0).contains(&v) => Ok(()),
                        _ => Err("volume: mic =40 sets absolute mic gain 0-100".into()),
                    }
                } else {
                    Err("volume: absolute set is only supported for mic, e.g. mic =40".into())
                }
            } else {
                match amt.trim_start_matches('+').parse::<f32>() {
                    Ok(v) if v != 0.0 => Ok(()),
                    _ => Err("volume: +4 / -4 to step, or 'dial' to slide".into()),
                }
            }
        }
        // profile: up/down/blank cycles (always ok); a name must be an existing profile.
        "profile" => {
            let l = p.to_lowercase();
            if p.is_empty() || l == "up" || l == "down" || l == "next" || l == "prev" {
                Ok(())
            } else if neuron::profile::list().iter().any(|n| n == p) {
                Ok(())
            } else {
                Err(format!(
                    "no profile '{p}' — save it first, or use up / down to cycle"
                ))
            }
        }
        "macro" => {
            // the strict front door checks the macro actually EXISTS (saved on disk) — a rule
            // pointing at a typo'd macro would dispatch into "not registered" forever.
            if neuron::macros::macro_host::list_macros()
                .iter()
                .any(|n| n == p)
            {
                Ok(())
            } else {
                Err(format!(
                    "no macro '{p}' — save + register it first (MACRO · PYTHON)"
                ))
            }
        }
        // obs: the ops that take an argument must actually have one where it's not defaultable —
        // a scene switch to nowhere would fire "name the scene" forever.
        "obs" => {
            let head = p.split_whitespace().next().unwrap_or("");
            let rest = p[head.len()..].trim();
            match head.to_ascii_lowercase().as_str() {
                "scene" if rest.is_empty() => {
                    Err("obs scene needs a name, like  scene Gameplay".into())
                }
                "" | "stream" | "record" | "pause" | "record-pause" | "replay" | "clip"
                | "scene" | "mute" => Ok(()),
                other => Err(format!(
                    "'{other}' isn't an obs op — stream · record · pause · replay · scene name · mute input"
                )),
            }
        }
        _ => Ok(()),
    }
}

/// A key name (or `+`-chord) the engine can really press — [`neuron::action::vk_for`] resolves it.
fn check_key(name: &str) -> Result<(), String> {
    let (_, key) = neuron::action::parse_combo(name);
    match neuron::action::vk_for(&key) {
        Some(_) => Ok(()),
        None => Err(format!(
            "'{key}' isn't a key the engine knows — CAPTURE it instead"
        )),
    }
}

/// Inverse of [`build_action`]: map a typed Action back to its palette `(id, param)` so an editor
/// opening on an EXISTING binding presets the picker to what's actually there (edit-shows-current).
/// Anything the GUI palette can't author maps to `("noop", "")`. Round-trips through `build_action`.
pub fn action_to_palette(a: &Action) -> (&'static str, String) {
    match a {
        Action::Key { key } => ("key", key.clone()),
        Action::Run { cmd } => ("run", cmd.clone()),
        // → the consolidated `volume` (mic step) / `mute` (mic toggle) entries.
        Action::MicGain { device, delta_pct } => (
            "volume",
            match device {
                Some(d) => format!("mic {delta_pct:+} · {d}"),
                None => format!("mic {delta_pct:+}"),
            },
        ),
        Action::MicGainSet { device, pct } => (
            "volume",
            match device {
                Some(d) => format!("mic ={pct} · {d}"),
                None => format!("mic ={pct}"),
            },
        ),
        Action::MicMute { device, mode } => (
            "mute",
            match (mode.as_str(), device) {
                ("toggle", None) => "mic".to_string(),
                (m, None) => format!("mic {m}"),
                ("toggle", Some(d)) => format!("mic · {d}"),
                (m, Some(d)) => format!("mic {m} · {d}"),
            },
        ),
        // → `volume` (output step, the default target) / `mute` (output toggle, the blank default).
        Action::OutputGain { device, delta_pct } => (
            "volume",
            match device {
                Some(d) => format!("{delta_pct:+} · {d}"),
                None => format!("{delta_pct:+}"),
            },
        ),
        Action::OutputMute { device, mode } => (
            "mute",
            match (mode.as_str(), device) {
                ("toggle", None) => String::new(),
                (m, None) => m.to_string(),
                ("toggle", Some(d)) => format!("· {d}"),
                (m, Some(d)) => format!("{m} · {d}"),
            },
        ),
        Action::Media { key } => (
            "media",
            match key {
                MediaKind::Next => "next",
                MediaKind::Prev => "prev",
                MediaKind::Stop => "stop",
                MediaKind::VolumeUp => "vol-up",
                MediaKind::VolumeDown => "vol-down",
                MediaKind::VolumeMute => "mute",
                MediaKind::PlayPause => "play-pause",
            }
            .into(),
        ),
        Action::MouseButton { button } => (
            "mouse",
            match button {
                MouseButtonKind::Right => "right",
                MouseButtonKind::Middle => "middle",
                MouseButtonKind::Back => "back",
                MouseButtonKind::Forward => "forward",
                MouseButtonKind::ScrollLeft => "scroll-left",
                MouseButtonKind::ScrollRight => "scroll-right",
                MouseButtonKind::Left => "left",
            }
            .into(),
        ),
        Action::Script { script } if script.kind == neuron::action::ScriptKind::Python => {
            ("macro", script.id.clone())
        }
        // a recorded/typed key sequence renders back to its grammar (edit-shows-current); a richer
        // sequence (nested scripts, audio steps…) is config-only and presets to noop as before.
        Action::Sequence { steps } => match keyseq_to_param(steps) {
            Some(p) => ("keys", p),
            None => ("noop", String::new()),
        },
        Action::Turbo { action, cps } => match action.as_ref() {
            Action::Key { key } => ("turbo", format!("{key} \u{00b7} {cps}")),
            // a turbo of anything richer than a key isn't GUI-authorable (config-only)
            _ => ("noop", String::new()),
        },
        Action::ScrollStageCycle { dir } => ("scroll-stage", dir.label().to_string()),
        Action::ProfileCycle { dir } => ("profile", dir.label().to_string()),
        Action::Teleport => ("teleport", String::new()),
        Action::Whiteboard => ("whiteboard", String::new()),
        Action::Knockback => ("knockback", String::new()),
        Action::Control => ("control", String::new()),
        Action::Glance { target } => ("glance", target.clone()),
        Action::Summon { window, mode } => (
            "summon",
            if *mode == neuron::action::SummonMode::Focus {
                window.clone()
            } else {
                format!("{window} \u{00b7} {}", mode.label())
            },
        ),
        Action::Banish { pick } => ("banish", pick.label().to_string()),
        Action::Pin { pick } => ("pin", pick.label().to_string()),
        Action::Kill { pick } => ("kill", pick.label().to_string()),
        Action::Echo => ("echo", String::new()),
        // CURTAIN — its own top-level entry (panic privacy overlay), no longer under `system`.
        Action::Curtain => ("curtain", String::new()),
        Action::Lock => ("system", "lock".to_string()),
        Action::Sleep => ("system", "sleep".to_string()),
        // → the consolidated `obs` entry: op word + argument, matching build_obs's grammar.
        Action::Obs { op, arg } => (
            "obs",
            {
                use neuron::action::ObsOp;
                let word = match op {
                    ObsOp::Stream => "stream",
                    ObsOp::Record => "record",
                    ObsOp::RecordPause => "pause",
                    ObsOp::Replay => "replay",
                    ObsOp::Scene => "scene",
                    ObsOp::Mute => "mute",
                };
                if arg.is_empty() {
                    // a bare stream toggle presets to blank (the palette's default)
                    if *op == ObsOp::Stream {
                        String::new()
                    } else {
                        word.to_string()
                    }
                } else {
                    format!("{word} {arg}")
                }
            },
        ),
        Action::Tether { slot, mode } => (
            "tether",
            match (mode, slot.is_empty()) {
                // Mark = the automagical default: just the (optional) name, no mode word.
                (neuron::action::TetherMode::Mark, _) => slot.clone(),
                (neuron::action::TetherMode::Wormhole, true) => "wormhole".to_string(),
                (neuron::action::TetherMode::Wormhole, false) => {
                    format!("{slot} \u{00b7} wormhole")
                }
            },
        ),
        Action::GhostPaste { speed } => ("ghost-paste", speed.label().to_string()),
        Action::MomentaryMic { device, mode } => (
            "momentary-mic",
            match device {
                Some(d) => format!("{} \u{00b7} {d}", mode.label()),
                None => mode.label().to_string(),
            },
        ),
        Action::OutputFlip { devices } => ("output-flip", devices.join(" \u{00b7} ")),
        // Dial → the consolidated `volume` (its slide submode); mic vs the default output.
        Action::Dial { target } => (
            "volume",
            match target {
                neuron::action::DialTarget::MicVolume => "mic dial".to_string(),
                neuron::action::DialTarget::OutputVolume => "dial".to_string(),
            },
        ),
        // DPI set/cycle → the consolidated `dpi` entry.
        Action::DpiSet { dpi } => ("dpi", dpi.to_string()),
        Action::DpiCycle { dir } => (
            "dpi",
            if *dir == Direction::Down {
                "down"
            } else {
                "up"
            }
            .into(),
        ),
        Action::ProfileSwitch { name } => ("profile", name.clone()),
        Action::Pocket { slot, persist } => (
            "pocket",
            if *persist {
                format!("{slot} \u{00b7} keep")
            } else {
                slot.clone()
            },
        ),
        _ => ("noop", String::new()),
    }
}

/// Map a CAPTURED press to the palette entry it means — the "we know what you mean" half of
/// press-to-fill: a mouse button retargets to the mouse action, a media key to the media action,
/// anything else lands as a key (chorded with whatever modifiers were held at the press).
/// `mods` are the modifier names held at capture time, in press order (e.g. `["ctrl","shift"]`).
pub fn vk_to_palette(vk: i32, mods: &[&str]) -> (&'static str, String) {
    match vk {
        0x01 => ("mouse", "left".into()),
        0x02 => ("mouse", "right".into()),
        0x04 => ("mouse", "middle".into()),
        0x05 => ("mouse", "back".into()),
        0x06 => ("mouse", "forward".into()),
        0xB3 => ("media", "play-pause".into()),
        0xB2 => ("media", "stop".into()),
        0xB0 => ("media", "next".into()),
        0xB1 => ("media", "prev".into()),
        0xAF => ("media", "vol-up".into()),
        0xAE => ("media", "vol-down".into()),
        0xAD => ("media", "mute".into()),
        v => {
            let key = neuron::action::key_param_for_vk(v as u16);
            let param = if mods.is_empty() {
                key
            } else {
                format!("{}+{key}", mods.join("+"))
            };
            ("key", param)
        }
    }
}

use neuron::engine::RuleDoc;

/// The path the GUI's authored binds live at — read by `controls::load_rule_sidecars`.
pub fn gui_rules_path() -> std::path::PathBuf {
    std::path::PathBuf::from("profiles").join("gui.rules.toml")
}

/// Load the GUI-authored spine rules (the removable, editable set the Bindings panel owns).
pub fn load_gui_rules() -> Vec<Rule> {
    match std::fs::read_to_string(gui_rules_path()) {
        Ok(s) => toml::from_str::<RuleDoc>(&s)
            .map(|d| d.rules)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Save the GUI-authored spine rules back to the sidecar.
pub fn save_gui_rules(rules: &[Rule]) -> Result<(), String> {
    std::fs::create_dir_all("profiles").map_err(|e| e.to_string())?;
    let doc = RuleDoc {
        rules: rules.to_vec(),
    };
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(gui_rules_path(), body).map_err(|e| e.to_string())?;
    Ok(())
}

/// How [`add_gui_rule`] landed: appended fresh, or replaced an existing rule for the same trigger.
#[derive(PartialEq, Debug)]
pub enum AddOutcome {
    /// Appended; carries the new total count.
    Added(usize),
    /// An existing same-trigger same-tier rule had its action replaced in place (re-bind).
    Replaced(usize),
}

/// Author one rule (a captured HID control trigger -> a chosen Action), optionally on the
/// HyperShift layer. Re-binding an already-bound trigger REPLACES its action in place (the
/// behaviour every keybind UI trains users to expect) instead of silently stacking a duplicate.
pub fn add_gui_rule(
    trigger: Trigger,
    action: Action,
    hypershift: bool,
) -> Result<AddOutcome, String> {
    let mut rules = load_gui_rules();
    let layer = hypershift.then(|| "hypershift".to_string());
    if let Some(existing) = rules
        .iter_mut()
        .find(|r| r.trigger == trigger && r.layer.is_some() == hypershift)
    {
        existing.action = action;
        let n = rules.len();
        save_gui_rules(&rules)?;
        return Ok(AddOutcome::Replaced(n));
    }
    let mut rule = Rule::new(trigger, action);
    rule.layer = layer;
    rules.push(rule);
    let n = rules.len();
    save_gui_rules(&rules)?;
    Ok(AddOutcome::Added(n))
}

/// Remove the GUI-authored rule at `idx` (an index into [`load_gui_rules`]).
pub fn remove_gui_rule(idx: usize) -> Result<(), String> {
    let mut rules = load_gui_rules();
    if idx >= rules.len() {
        return Err("index out of range".into());
    }
    rules.remove(idx);
    save_gui_rules(&rules)
}

/// Remove the `n`-th GUI-authored rule of a given layer tier (base vs hypershift). The Bindings panel
/// lists base + hypershift GUI rules in SEPARATE columns, so a row index in one column is the n-th
/// rule of that tier — not a flat index into the mixed [`load_gui_rules`] vec. This maps it back.
pub fn remove_gui_rule_in_tier(n: usize, hypershift: bool) -> Result<(), String> {
    let rules = load_gui_rules();
    // find the flat index of the n-th rule whose layer-tier matches.
    let mut seen = 0usize;
    for (i, r) in rules.iter().enumerate() {
        // the Noop activator on the hypershift layer is the HOLD KEY — surfaced + removed separately
        // (clear_hypershift_hold) and filtered OUT of the shifted list, so skip it to keep indices aligned.
        let is_hold_key = hypershift && r.layer.is_some() && r.action == Action::Noop;
        if r.layer.is_some() == hypershift && !is_hold_key {
            if seen == n {
                return remove_gui_rule(i);
            }
            seen += 1;
        }
    }
    Err("no such GUI rule in that tier".into())
}

/// Reorder: move the `n`-th GUI-authored rule of a tier by `dir` (-1 = up / earlier, +1 = down /
/// later) among its tier siblings, then save. Uses the SAME tier-index mapping as
/// [`remove_gui_rule_in_tier`] (the hold-key Noop is filtered out so `n` lines up with the row
/// index). A move past either end is a CLAMPED no-op (not an error). Order is the display order, so
/// this is purely how a user ARRANGES their binds — it never changes what fires (each trigger is its
/// own rule), it just rewrites the row sequence in `gui.rules.toml`.
pub fn move_gui_rule_in_tier(n: usize, hypershift: bool, dir: i32) -> Result<(), String> {
    let mut rules = load_gui_rules();
    // flat-vec indices of this tier's rows, in display order (hold-key excluded, exactly as the
    // remover/inverse do — so swapping among them matches what the list shows).
    let tier: Vec<usize> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            let is_hold_key = hypershift && r.layer.is_some() && r.action == Action::Noop;
            r.layer.is_some() == hypershift && !is_hold_key
        })
        .map(|(i, _)| i)
        .collect();
    if n >= tier.len() {
        return Err("no such GUI rule in that tier".into());
    }
    let target = n as i32 + dir;
    if target < 0 || target as usize >= tier.len() {
        return Ok(()); // clamped at the ends — a no-op
    }
    // swap the two tier rows' FLAT positions: other-tier rows keep their place, and because the
    // display follows flat order, the n-th and target-th tier rows exchange in the list.
    rules.swap(tier[n], tier[target as usize]);
    save_gui_rules(&rules)
}

/// Fetch a clone of the `n`-th GUI-authored rule of a tier (base vs hypershift) — the inverse lookup
/// of [`remove_gui_rule_in_tier`], so an editor opening on a row can read its real trigger + action.
/// Skips the hold-key Noop activator exactly as the remover does, so `n` lines up with the row index.
pub fn gui_rule_in_tier(n: usize, hypershift: bool) -> Option<Rule> {
    let rules = load_gui_rules();
    let mut seen = 0usize;
    for r in &rules {
        let is_hold_key = hypershift && r.layer.is_some() && r.action == Action::Noop;
        if r.layer.is_some() == hypershift && !is_hold_key {
            if seen == n {
                return Some(r.clone());
            }
            seen += 1;
        }
    }
    None
}

/// Edit the `n`-th GUI-authored rule of a tier IN PLACE — overwrite its trigger + action, then save.
/// Pairs with [`gui_rule_in_tier`] (same indexing) for inline rule editing: unlike [`add_gui_rule`]
/// (which keys off the trigger), this targets the exact row, so a TRIGGER change rebinds that one rule
/// without leaving the original behind or stacking a duplicate.
pub fn edit_gui_rule_in_tier(
    n: usize,
    hypershift: bool,
    trigger: Trigger,
    action: Action,
) -> Result<(), String> {
    let mut rules = load_gui_rules();
    let mut seen = 0usize;
    let mut target: Option<usize> = None;
    for (i, r) in rules.iter().enumerate() {
        let is_hold_key = hypershift && r.layer.is_some() && r.action == Action::Noop;
        if r.layer.is_some() == hypershift && !is_hold_key {
            if seen == n {
                target = Some(i);
                break;
            }
            seen += 1;
        }
    }
    let Some(i) = target else {
        return Err("no such GUI rule in that tier".into());
    };
    rules[i].trigger = trigger;
    rules[i].action = action;
    save_gui_rules(&rules)
}

/// The HyperShift HOLD KEY — the control you hold to REACH the second layer. Stored as a `Noop` rule
/// on the "hypershift" layer: the engine activates a layer for ANY input with a rule on it, so a Noop
/// rule is a pure activator (it holds the layer, does nothing itself). Exactly one hold key — setting
/// a new one replaces the old.
pub fn set_hypershift_hold(trigger: Trigger) -> Result<(), String> {
    let mut rules = load_gui_rules();
    rules.retain(|r| !(r.layer.as_deref() == Some("hypershift") && r.action == Action::Noop));
    let mut rule = Rule::new(trigger, Action::Noop);
    rule.layer = Some("hypershift".to_string());
    rules.push(rule);
    save_gui_rules(&rules)
}

/// Clear the HyperShift hold key (drop its Noop activator). Idempotent.
pub fn clear_hypershift_hold() -> Result<(), String> {
    let mut rules = load_gui_rules();
    let before = rules.len();
    rules.retain(|r| !(r.layer.as_deref() == Some("hypershift") && r.action == Action::Noop));
    if rules.len() == before {
        return Ok(());
    }
    save_gui_rules(&rules)
}

/// Serialize a [`CastConfig`] back to `cast.toml` (the radial wedges + glyph spells). `CastConfig`
/// has no `save()` in core, so the editor owns the write — the file format is plain serde TOML.
pub fn save_cast(cast: &CastConfig) -> Result<(), String> {
    let body = toml::to_string_pretty(cast).map_err(|e| e.to_string())?;
    std::fs::write(CastConfig::path(), body).map_err(|e| e.to_string())
}

/// Set sector `i`'s action in the base OR HyperShift radial (`hyper`), growing the vec as needed,
/// then save. The two sets share the editor — only the target Vec differs.
pub fn set_sector_action_on(
    cast: &mut CastConfig,
    i: usize,
    action: Action,
    hyper: bool,
) -> Result<(), String> {
    let vec = if hyper {
        &mut cast.hyper_radial
    } else {
        &mut cast.radial
    };
    if vec.len() <= i {
        vec.resize(i + 1, Action::Noop);
    }
    vec[i] = action;
    save_cast(cast)
}

/// Bind a recorded glyph name -> an action in the cast config, then save.
pub fn set_gesture_action(cast: &mut CastConfig, name: &str, action: Action) -> Result<(), String> {
    cast.gestures.insert(name.to_string(), action);
    save_cast(cast)
}

/// Bind a CAST RHYTHM (`taps` taps then hold) -> an action in the cast config (upsert by tap
/// count), then save. This is the rhythm-map twin of [`set_gesture_action`]: it writes the
/// `rhythm_actions` entry the engine folds into a first-class `Trigger::Cast { taps }` rule.
pub fn set_rhythm_action(cast: &mut CastConfig, taps: u8, action: Action) -> Result<(), String> {
    match cast.rhythm_actions.iter_mut().find(|rb| rb.taps == taps) {
        Some(rb) => rb.action = action,
        None => cast
            .rhythm_actions
            .push(neuron::cast::RhythmBind { taps, action }),
    }
    save_cast(cast)
}

/// Remove a CAST RHYTHM binding (by tap count) from the cast config, then save. A no-op if the
/// rhythm wasn't bound (still saves — harmless, keeps the on-disk shape current).
pub fn delete_rhythm_action(cast: &mut CastConfig, taps: u8) -> Result<(), String> {
    cast.rhythm_actions.retain(|rb| rb.taps != taps);
    save_cast(cast)
}

// ── SNIPER binding — a held Action on the shared rule spine (NOT a snowflake config) ──────────
// Sniper is `Trigger::Input -> Action::Sniper { dpi }` in `gui.rules.toml`, exactly like every
// other authored bind. These helpers keep it to AT MOST ONE rule and let the Device panel + the CLI
// read/write it through the one store. (Retired the old `sniper.toml` + `SniperConfig`.)

/// The current sniper binding, if any: `(hold trigger, precision dpi)`. The Device panel seeds its
/// readout from this — never a fictional default — and the CLI reads the same one rule.
pub fn sniper_binding() -> Option<(Trigger, u16)> {
    load_gui_rules().into_iter().find_map(|r| match r.action {
        Action::Sniper { dpi } => Some((r.trigger, dpi)),
        _ => None,
    })
}

/// Author (or RE-bind) the sniper hold button to `trigger` at `dpi`. Clears any existing sniper
/// rule first, so there is always at most one — a re-bind MOVES the button, never stacks a second.
pub fn set_sniper_button(trigger: Trigger, dpi: u16) -> Result<(), String> {
    clear_sniper()?;
    add_gui_rule(trigger, Action::Sniper { dpi }, false).map(|_| ())
}

/// Update the precision DPI of the existing sniper binding in place. `Ok(false)` if nothing is bound
/// yet (the caller then just remembers the pending dpi for the next bind).
pub fn set_sniper_dpi(dpi: u16) -> Result<bool, String> {
    let mut rules = load_gui_rules();
    let mut found = false;
    for r in rules.iter_mut() {
        if let Action::Sniper { dpi: d } = &mut r.action {
            *d = dpi;
            found = true;
        }
    }
    if found {
        save_gui_rules(&rules)?;
    }
    Ok(found)
}

/// UNBIND the sniper hold button — the FEEL tile's explicit "turn this off".
/// Ok even when nothing was bound (unbinding nothing isn't an error). The
/// caller reloads the live worker, whose release-all safety net restores any
/// currently-held precision DPI before the rule vanishes.
pub fn unbind_sniper() -> Result<(), String> {
    clear_sniper()
}

/// Remove every sniper rule (used before a re-bind so exactly one ever exists).
fn clear_sniper() -> Result<(), String> {
    let mut rules = load_gui_rules();
    let before = rules.len();
    rules.retain(|r| !matches!(r.action, Action::Sniper { .. }));
    if rules.len() != before {
        save_gui_rules(&rules)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_every_palette_action() {
        // every palette id resolves to a non-panicking Action (most non-Noop with a param).
        for (id, ..) in ACTION_PALETTE {
            let a = build_action(id, "f");
            // only "noop" maps to Noop; everything else is a real action.
            if *id == "noop" {
                assert_eq!(a, Action::Noop);
            } else {
                assert_ne!(a, Action::Noop, "id '{id}' should build a real action");
            }
        }
    }

    /// The consolidated `obs` grammar: op word + argument, round-tripping through
    /// `action_to_palette` (edit-shows-current), with the strict front door catching a
    /// scene switch to nowhere.
    #[test]
    fn obs_grammar_round_trips() {
        use neuron::action::ObsOp;
        let obs = |op: ObsOp, arg: &str| Action::Obs {
            op,
            arg: arg.into(),
        };
        // blank = the stream toggle (the palette's default), and it presets back to blank
        assert_eq!(build_action("obs", ""), obs(ObsOp::Stream, ""));
        assert_eq!(
            action_to_palette(&obs(ObsOp::Stream, "")),
            ("obs", String::new())
        );
        // op words parse, with arguments where they apply
        assert_eq!(build_action("obs", "stream stop"), obs(ObsOp::Stream, "stop"));
        assert_eq!(build_action("obs", "record"), obs(ObsOp::Record, ""));
        assert_eq!(build_action("obs", "pause"), obs(ObsOp::RecordPause, ""));
        assert_eq!(build_action("obs", "replay"), obs(ObsOp::Replay, ""));
        assert_eq!(
            build_action("obs", "scene Just Chatting"),
            obs(ObsOp::Scene, "Just Chatting")
        );
        assert_eq!(build_action("obs", "mute Mic/Aux"), obs(ObsOp::Mute, "Mic/Aux"));
        // and they render back to the same grammar (edit-shows-current)
        for p in ["stream stop", "record", "pause", "replay", "scene Just Chatting", "mute Mic/Aux"] {
            let a = build_action("obs", p);
            let (id, back) = action_to_palette(&a);
            assert_eq!(id, "obs");
            assert_eq!(build_action(id, &back), a, "'{p}' must round-trip");
        }
        // the strict front door: a scene with no name, or a junk op, never commits
        assert!(validate_action("obs", "scene").is_err());
        assert!(validate_action("obs", "twitch").is_err());
        assert!(validate_action("obs", "").is_ok());
        assert!(validate_action("obs", "scene Gameplay").is_ok());
    }

    /// The new palette entries build the real engine actions — the picker is never narrower
    /// than the spine: instruments, turbo, the cycle intents.
    #[test]
    fn palette_covers_the_new_spine() {
        assert_eq!(build_action("teleport", ""), Action::Teleport);
        assert_eq!(build_action("whiteboard", ""), Action::Whiteboard);
        assert_eq!(
            build_action("turbo", "f \u{00b7} 12"),
            Action::Turbo {
                action: Box::new(Action::Key { key: "f".into() }),
                cps: 12
            }
        );
        assert_eq!(
            build_action("turbo", "f"),
            Action::Turbo {
                action: Box::new(Action::Key { key: "f".into() }),
                cps: 10
            },
            "bare key defaults to 10 cps"
        );
        assert!(matches!(
            build_action("scroll-stage", "up"),
            Action::ScrollStageCycle { .. }
        ));
        assert!(matches!(
            build_action("profile", "down"),
            Action::ProfileCycle { .. }
        ));
        assert!(matches!(
            build_action("profile", "studio"),
            Action::ProfileSwitch { .. }
        ));
        // inverses round-trip so edit-shows-current holds for the new entries too
        assert_eq!(action_to_palette(&Action::Teleport).0, "teleport");
        assert_eq!(action_to_palette(&Action::Whiteboard).0, "whiteboard");
        // GLANCE: builds, round-trips, and demands a target (a peek at nothing is nothing)
        assert_eq!(
            build_action("glance", "obs"),
            Action::Glance {
                target: "obs".into()
            }
        );
        assert_eq!(
            action_to_palette(&Action::Glance {
                target: "obs".into()
            }),
            ("glance", "obs".to_string())
        );
        assert!(validate_action("glance", "").is_err());
        assert!(validate_action("glance", "obs").is_ok());

        // ── the WINDOW quick-actions: enums + grammars round-trip ──
        use neuron::action::{SummonMode, WindowPick};
        // summon: "window · mode"; bare window defaults to focus and inverses WITHOUT a mode tail
        assert_eq!(
            build_action("summon", "discord \u{00b7} here"),
            Action::Summon {
                window: "discord".into(),
                mode: SummonMode::Here
            }
        );
        assert_eq!(
            action_to_palette(&Action::Summon {
                window: "discord".into(),
                mode: SummonMode::Here
            }),
            ("summon", "discord \u{00b7} here".to_string())
        );
        assert_eq!(
            build_action("summon", "term"),
            Action::Summon {
                window: "term".into(),
                mode: SummonMode::Focus
            }
        );
        assert_eq!(
            action_to_palette(&Action::Summon {
                window: "term".into(),
                mode: SummonMode::Focus
            }),
            ("summon", "term".to_string())
        );
        assert!(
            validate_action("summon", "").is_err(),
            "summon needs a window"
        );
        assert!(validate_action("summon", "obs \u{00b7} toggle").is_ok());
        // banish: the pick enum; blank = focused (default), valid
        assert_eq!(
            build_action("banish", "hover"),
            Action::Banish {
                pick: WindowPick::Hover
            }
        );
        assert_eq!(
            build_action("banish", "behind"),
            Action::Banish {
                pick: WindowPick::Behind
            }
        );
        assert_eq!(
            build_action("banish", ""),
            Action::Banish {
                pick: WindowPick::Focused
            }
        );
        assert_eq!(
            action_to_palette(&Action::Banish {
                pick: WindowPick::Behind
            }),
            ("banish", "behind".to_string())
        );
        assert!(
            validate_action("banish", "").is_ok(),
            "banish defaults to focused"
        );
        // pin: behind degrades to focused (pin doesn't sweep)
        assert_eq!(
            build_action("pin", "behind"),
            Action::Pin {
                pick: WindowPick::Focused
            }
        );
        assert_eq!(
            build_action("pin", "hover"),
            Action::Pin {
                pick: WindowPick::Hover
            }
        );
        // tether: the param is an optional stone name, then an optional "· mode" (default mark);
        // wormhole makes the slot a two-anchor portal.
        use neuron::action::TetherMode;
        assert_eq!(
            build_action("tether", "home"),
            Action::Tether {
                slot: "home".into(),
                mode: TetherMode::Mark
            }
        );
        assert_eq!(
            build_action("tether", ""),
            Action::Tether {
                slot: String::new(),
                mode: TetherMode::Mark
            }
        );
        assert_eq!(
            build_action("tether", "warp \u{00b7} wormhole"),
            Action::Tether {
                slot: "warp".into(),
                mode: TetherMode::Wormhole
            }
        );
        assert_eq!(
            action_to_palette(&Action::Tether {
                slot: String::new(),
                mode: TetherMode::Mark
            }),
            ("tether", String::new())
        );
        assert_eq!(
            action_to_palette(&Action::Tether {
                slot: "a".into(),
                mode: TetherMode::Mark
            }),
            ("tether", "a".to_string())
        );
        assert_eq!(
            action_to_palette(&Action::Tether {
                slot: "warp".into(),
                mode: TetherMode::Wormhole
            }),
            ("tether", "warp \u{00b7} wormhole".to_string())
        );
        assert!(
            validate_action("tether", "").is_ok(),
            "tether needs no param"
        );
        // every one builds a real (non-Noop) action
        for (id, p) in [("summon", "x"), ("banish", ""), ("pin", ""), ("tether", "")] {
            assert_ne!(build_action(id, p), Action::Noop, "{id} must build");
        }

        // ── the prebuilt primitives: ghost-paste, momentary-mic, output-flip ──
        use neuron::action::{GhostSpeed, MomentaryMode};
        assert_eq!(
            build_action("ghost-paste", "fast"),
            Action::GhostPaste {
                speed: GhostSpeed::Fast
            }
        );
        assert_eq!(
            build_action("ghost-paste", ""),
            Action::GhostPaste {
                speed: GhostSpeed::Borderline
            },
            "blank ghost-paste = the borderline-instant default"
        );
        assert_eq!(
            action_to_palette(&Action::GhostPaste {
                speed: GhostSpeed::Normal
            })
            .1,
            "normal"
        );
        // momentary mic: "mode · device"
        assert_eq!(
            build_action("momentary-mic", "talk \u{00b7} seiren"),
            Action::MomentaryMic {
                device: Some("seiren".into()),
                mode: MomentaryMode::Talk
            }
        );
        assert_eq!(
            build_action("momentary-mic", ""),
            Action::MomentaryMic {
                device: None,
                mode: MomentaryMode::Flip
            }
        );
        assert_eq!(
            action_to_palette(&Action::MomentaryMic {
                device: None,
                mode: MomentaryMode::Mute
            }),
            ("momentary-mic", "mute".to_string())
        );
        // momentary mode's adaptive states: flip is the opposite of rest, talk/mute are fixed
        assert_eq!(
            MomentaryMode::Flip.states(true),
            (false, true),
            "muted rest → push-to-talk"
        );
        assert_eq!(
            MomentaryMode::Flip.states(false),
            (true, false),
            "live rest → push-to-mute"
        );
        assert_eq!(MomentaryMode::Talk.states(false), (false, true));
        assert_eq!(MomentaryMode::Mute.states(true), (true, false));
        // output flip: the device cycle set (· list), blank = cycle all
        assert_eq!(
            build_action("output-flip", "headset \u{00b7} speakers"),
            Action::OutputFlip {
                devices: vec!["headset".into(), "speakers".into()]
            }
        );
        assert_eq!(
            build_action("output-flip", ""),
            Action::OutputFlip { devices: vec![] }
        );
        assert_eq!(
            action_to_palette(&Action::OutputFlip {
                devices: vec!["a".into(), "b".into()]
            }),
            ("output-flip", "a \u{00b7} b".to_string())
        );
        // all three default-on-empty (their hints are non-empty but a blank is a valid default)
        for id in ["ghost-paste", "momentary-mic", "output-flip"] {
            assert!(
                validate_action(id, "").is_ok(),
                "{id} must accept a blank (default)"
            );
        }

        // VOLUME (consolidated): blank/`dial` = output slide, `mic dial` = mic slide; a ±number = step
        use neuron::action::DialTarget;
        assert_eq!(
            build_action("volume", "mic dial"),
            Action::Dial {
                target: DialTarget::MicVolume
            }
        );
        assert_eq!(
            build_action("volume", ""),
            Action::Dial {
                target: DialTarget::OutputVolume
            }
        );
        assert_eq!(
            build_action("volume", "dial"),
            Action::Dial {
                target: DialTarget::OutputVolume
            }
        );
        assert_eq!(
            action_to_palette(&Action::Dial {
                target: DialTarget::OutputVolume
            }),
            ("volume", "dial".to_string())
        );
        assert_eq!(
            build_action("volume", "+4"),
            Action::OutputGain {
                device: None,
                delta_pct: 4.0
            }
        );
        assert_eq!(
            build_action("volume", "mic -3"),
            Action::MicGain {
                device: None,
                delta_pct: -3.0
            }
        );
        assert!(validate_action("volume", "").is_ok());
        let (id, p) = action_to_palette(&Action::Turbo {
            action: Box::new(Action::Key { key: "q".into() }),
            cps: 25,
        });
        assert_eq!((id, p.as_str()), ("turbo", "q \u{00b7} 25"));
        // validation: turbo rejects silly rates, instruments need no parameter
        assert!(validate_action("turbo", "f \u{00b7} 999").is_err());
        assert!(validate_action("teleport", "").is_ok());
        assert!(validate_action("whiteboard", "").is_ok());
    }

    /// The sequence grammar: parse, inverse, and validation — the macro recorder's contract.
    #[test]
    fn keyseq_grammar_round_trips() {
        // "hold W 180, pause 90, tap A, tap space" — the canonical take.
        let a = build_action("keys", "w:180 ~90 a space");
        let Action::Sequence { steps } = &a else {
            panic!("keys builds a Sequence")
        };
        assert_eq!(steps.len(), 3);
        assert_eq!(*steps[0].action, Action::Key { key: "w".into() });
        assert_eq!(steps[0].hold_ms, 180);
        assert_eq!(steps[0].delay_ms, 90, "the pause rides the previous step");
        assert_eq!(
            *steps[2].action,
            Action::Key {
                key: "space".into()
            }
        );
        // inverse: edit-shows-current renders the grammar back.
        let (id, p) = action_to_palette(&a);
        assert_eq!(id, "keys");
        assert_eq!(p, "w:180 ~90 a space");
        // chords ride through; mouse tokens become real button steps.
        let b = build_action("keys", "ctrl+s lclick mouse4:200");
        let Action::Sequence { steps } = &b else {
            panic!()
        };
        assert_eq!(
            *steps[0].action,
            Action::Key {
                key: "ctrl+s".into()
            }
        );
        assert_eq!(
            *steps[1].action,
            Action::MouseButton {
                button: MouseButtonKind::Left
            }
        );
        assert_eq!(
            *steps[2].action,
            Action::MouseButton {
                button: MouseButtonKind::Back
            }
        );
        assert_eq!(steps[2].hold_ms, 200);
        let (id, p) = action_to_palette(&b);
        assert_eq!((id, p.as_str()), ("keys", "ctrl+s lclick mouse4:200"));
        // a leading pause gets a noop carrier and round-trips.
        let c = build_action("keys", "~250 f");
        let (_, p) = action_to_palette(&c);
        assert_eq!(p, "~250 f");
        // validation: junk keys rejected, pure-pause rejected, real takes pass.
        assert!(validate_action("keys", "w:180 ~90 a").is_ok());
        assert!(validate_action("keys", "notakey q").is_err());
        assert!(
            validate_action("keys", "~500").is_err(),
            "a macro of nothing"
        );
        // arrow keys stay keys — "left" the arrow, "lclick" the button.
        let d = build_action("keys", "left lclick");
        let Action::Sequence { steps } = &d else {
            panic!()
        };
        assert_eq!(*steps[0].action, Action::Key { key: "left".into() });
        assert_eq!(
            *steps[1].action,
            Action::MouseButton {
                button: MouseButtonKind::Left
            }
        );
    }

    /// Captured presses map to the palette entry they MEAN — mouse buttons retarget to the mouse
    /// action, media keys to media, everything else lands as a (possibly chorded) key.
    #[test]
    fn captures_map_to_the_right_palette_entry() {
        assert_eq!(vk_to_palette(0x05, &[]), ("mouse", "back".into()));
        assert_eq!(vk_to_palette(0xB3, &[]), ("media", "play-pause".into()));
        assert_eq!(vk_to_palette(0x53, &[]), ("key", "s".into()));
        assert_eq!(
            vk_to_palette(0x53, &["ctrl", "shift"]),
            ("key", "ctrl+shift+s".into())
        );
        assert_eq!(vk_to_palette(0x73, &[]), ("key", "f4".into()));
        // everything a capture emits passes the strict validation gate (the no-dead-binds law).
        for (vk, mods) in [(0x53, &["ctrl"][..]), (0x70, &[]), (0xBA, &[]), (0x67, &[])] {
            let (id, p) = vk_to_palette(vk, mods);
            assert!(
                validate_action(id, &p).is_ok(),
                "captured ({id}, {p}) must validate"
            );
        }
        // key validation rejects junk but accepts chords + the hex escape hatch.
        assert!(validate_action("key", "ctrl+shift+s").is_ok());
        assert!(validate_action("key", "0x73").is_ok());
        assert!(validate_action("key", "definitely-not-a-key").is_err());
    }

    #[test]
    fn build_action_parses_params() {
        assert_eq!(build_action("key", "f5"), Action::Key { key: "f5".into() });
        assert_eq!(
            build_action("run", "obs"),
            Action::Run { cmd: "obs".into() }
        );
        assert_eq!(build_action("dpi", "1600"), Action::DpiSet { dpi: 1600 });
        assert_eq!(
            build_action("volume", "mic +4"),
            Action::MicGain {
                device: None,
                delta_pct: 4.0
            }
        );
        assert_eq!(
            build_action("media", "vol-up"),
            Action::Media {
                key: MediaKind::VolumeUp
            }
        );
        assert_eq!(
            build_action("dpi", "down"),
            Action::DpiCycle {
                dir: Direction::Down
            }
        );
    }

    /// The OUTPUT audio palette entries build the new `OutputGain`/`OutputMute` actions, including
    /// the optional `· device` substring that targets a specific render endpoint (headset / sound
    /// card) vs the system default output.
    #[test]
    fn build_action_parses_output_audio() {
        // bare delta -> system default output (device None).
        assert_eq!(
            build_action("volume", "+4"),
            Action::OutputGain {
                device: None,
                delta_pct: 4.0
            }
        );
        assert_eq!(
            build_action("volume", "-6"),
            Action::OutputGain {
                device: None,
                delta_pct: -6.0
            }
        );
        // "<delta> · <device>" targets a named render endpoint.
        assert_eq!(
            build_action("volume", "+5 · razer"),
            Action::OutputGain {
                device: Some("razer".into()),
                delta_pct: 5.0
            }
        );
        // mute modes (blank/toggle default).
        assert_eq!(
            build_action("mute", "toggle"),
            Action::OutputMute {
                device: None,
                mode: "toggle".into()
            }
        );
        assert_eq!(
            build_action("mute", "on · headphones"),
            Action::OutputMute {
                device: Some("headphones".into()),
                mode: "on".into()
            }
        );
        // an unrecognized mute word falls back to toggle (the safe default).
        assert_eq!(
            build_action("mute", "wiggle"),
            Action::OutputMute {
                device: None,
                mode: "toggle".into()
            }
        );
    }

    /// The GUI rules sidecar serializes + parses losslessly (the on-disk contract `load_gui_rules`
    /// reads). Tested at the serde layer — NO cwd change — so it is parallel-safe and never touches
    /// the user's real config. The HyperShift layer tag survives the round-trip.
    #[test]
    fn gui_rules_doc_roundtrips() {
        let rules = vec![
            Rule::new(
                Trigger::Input {
                    page: 0x0C,
                    usage: 0xE9,
                    pid: Some(0x0529),
                },
                build_action("key", "f"),
            ),
            {
                let mut r = Rule::new(
                    Trigger::Input {
                        page: 0x0B,
                        usage: 0x2F,
                        pid: None,
                    },
                    build_action("mute", "mic"),
                );
                r.layer = Some("hypershift".into());
                r
            },
        ];
        let doc = RuleDoc {
            rules: rules.clone(),
        };
        let body = toml::to_string_pretty(&doc).unwrap();
        let back: RuleDoc = toml::from_str(&body).unwrap();
        assert_eq!(back.rules.len(), 2);
        assert_eq!(back.rules[0].action, Action::Key { key: "f".into() });
        assert_eq!(back.rules[1].layer.as_deref(), Some("hypershift"));
        assert_eq!(
            back.rules[1].action,
            Action::MicMute {
                device: None,
                mode: "toggle".into()
            }
        );
    }

    /// `add_gui_rule` appends + tags HyperShift correctly — tested in a scoped temp dir guarded by a
    /// process-wide mutex so the cwd change can't race other cwd-using tests.
    #[test]
    fn add_gui_rule_appends_and_tags_layer() {
        let _g = cwd_guard();
        assert!(load_gui_rules().is_empty());
        let n = add_gui_rule(
            Trigger::Input {
                page: 0x0C,
                usage: 0xE9,
                pid: Some(0x0529),
            },
            build_action("key", "f"),
            false,
        )
        .unwrap();
        assert_eq!(n, AddOutcome::Added(1));
        let n2 = add_gui_rule(
            Trigger::Input {
                page: 0x0B,
                usage: 0x2F,
                pid: None,
            },
            build_action("mute", "mic"),
            true,
        )
        .unwrap();
        assert_eq!(n2, AddOutcome::Added(2));
        // re-binding the same trigger on the same tier REPLACES, never stacks.
        let n3 = add_gui_rule(
            Trigger::Input {
                page: 0x0C,
                usage: 0xE9,
                pid: Some(0x0529),
            },
            build_action("key", "g"),
            false,
        )
        .unwrap();
        assert_eq!(n3, AddOutcome::Replaced(2));
        assert_eq!(load_gui_rules()[0].action, Action::Key { key: "g".into() });
        let loaded = load_gui_rules();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].layer.as_deref(), Some("hypershift"));
        remove_gui_rule(0).unwrap();
        assert_eq!(load_gui_rules().len(), 1);
    }

    /// `remove_gui_rule_in_tier` removes the n-th rule of the correct tier even when base + hyper
    /// rules are interleaved on disk — the mapping the Bindings panel's two columns rely on.
    #[test]
    fn remove_in_tier_maps_columns_correctly() {
        let _g = cwd_guard();
        // author: base0, hyper0, base1 (interleaved in the file)
        add_gui_rule(
            Trigger::Input {
                page: 1,
                usage: 1,
                pid: None,
            },
            build_action("key", "a"),
            false,
        )
        .unwrap();
        add_gui_rule(
            Trigger::Input {
                page: 2,
                usage: 2,
                pid: None,
            },
            build_action("key", "b"),
            true,
        )
        .unwrap();
        add_gui_rule(
            Trigger::Input {
                page: 3,
                usage: 3,
                pid: None,
            },
            build_action("key", "c"),
            false,
        )
        .unwrap();
        // base column row 1 = the SECOND base rule = the "c" key (not the hyper "b").
        remove_gui_rule_in_tier(1, false).unwrap();
        let rules = load_gui_rules();
        // remaining base rules: just "a"; hyper rule "b" untouched.
        let base: Vec<_> = rules.iter().filter(|r| r.layer.is_none()).collect();
        let hyper: Vec<_> = rules.iter().filter(|r| r.layer.is_some()).collect();
        assert_eq!(base.len(), 1);
        assert_eq!(base[0].action, Action::Key { key: "a".into() });
        assert_eq!(hyper.len(), 1);
        assert_eq!(hyper[0].action, Action::Key { key: "b".into() });
    }

    /// Every GUI-authorable action round-trips: build_action -> action_to_palette -> build_action
    /// is identity, so the edit-shows-current preset can never misrepresent what's on disk.
    #[test]
    fn action_palette_roundtrips() {
        let samples: &[(&str, &str)] = &[
            ("key", "f5"),
            ("run", "obs"),
            ("volume", "mic +4"),
            ("volume", "+5 · razer"),
            ("volume", "dial"),
            ("volume", "mic dial"),
            ("mute", "mic"),
            ("mute", "on · headphones"),
            ("mute", ""),
            ("media", "vol-up"),
            ("mouse", "middle"),
            ("dpi", "1600"),
            ("dpi", "down"),
            ("profile", "comfy"),
            ("profile", "up"),
            ("tether", "spot"),
            ("tether", "wormhole"),
            ("tether", "warp · wormhole"),
            ("system", ""),
            ("system", "lock"),
            ("system", "sleep"),
            ("noop", ""),
        ];
        for (id, param) in samples {
            let a = build_action(id, param);
            let (rid, rparam) = action_to_palette(&a);
            let b = build_action(rid, &rparam);
            assert_eq!(
                a, b,
                "({id}, {param}) failed to round-trip via ({rid}, {rparam})"
            );
        }
    }

    /// The strict validation gate rejects what build_action would silently mangle.
    #[test]
    fn validate_action_rejects_junk() {
        assert!(validate_action("key", "").is_err(), "empty key param");
        assert!(validate_action("dpi", "abc").is_err(), "non-numeric dpi");
        assert!(validate_action("dpi", "50").is_err(), "out-of-range dpi");
        assert!(
            validate_action("volume", "mic abc").is_err(),
            "non-numeric gain step"
        );
        assert!(
            validate_action("profile", "__no_such_profile__").is_err(),
            "missing profile"
        );
        assert!(validate_action("key", "f5").is_ok());
        assert!(
            validate_action("noop", "").is_ok(),
            "no-param action needs none"
        );
        assert!(validate_action("dpi", "up").is_ok());
        assert!(
            validate_action("volume", "").is_ok(),
            "blank volume = dial (default)"
        );
        assert!(
            validate_action("mute", "").is_ok(),
            "blank mute = output toggle (default)"
        );
        assert!(
            validate_action("tether", "").is_ok(),
            "blank tether = the automagical warpstone"
        );
    }

    /// The sniper binding lives as a held `Action::Sniper` rule on the shared spine: it round-trips,
    /// its DPI updates in place, and a re-bind MOVES it (never stacks a second sniper rule).
    #[test]
    fn sniper_binding_is_one_rule_on_the_spine() {
        let _g = cwd_guard();
        let alt = Trigger::Input {
            page: 0x07,
            usage: 0xE2,
            pid: None,
        }; // Left Alt
        set_sniper_button(alt.clone(), 400).unwrap();
        assert_eq!(sniper_binding(), Some((alt.clone(), 400)));

        // the precision DPI updates in place, keeping the same button
        assert_eq!(set_sniper_dpi(800), Ok(true));
        assert_eq!(sniper_binding(), Some((alt, 800)));

        // re-binding to a different control moves the sniper — still exactly ONE sniper rule
        let side = Trigger::Input {
            page: 0x09,
            usage: 5,
            pid: Some(0x1234),
        };
        set_sniper_button(side.clone(), 800).unwrap();
        assert_eq!(sniper_binding(), Some((side, 800)));
        let count = load_gui_rules()
            .into_iter()
            .filter(|r| matches!(r.action, Action::Sniper { .. }))
            .count();
        assert_eq!(count, 1, "a re-bind must not stack a second sniper rule");

        // unbind removes the rule outright; unbinding again stays Ok (not an error)
        unbind_sniper().unwrap();
        assert_eq!(sniper_binding(), None, "unbind must clear the binding");
        unbind_sniper().unwrap();
    }

    /// With nothing bound, setting the precision DPI is a no-op that reports `Ok(false)` — the panel
    /// then just remembers the fader value until a hold button is captured.
    #[test]
    fn sniper_dpi_without_a_binding_is_ok_false() {
        let _g = cwd_guard();
        assert_eq!(set_sniper_dpi(1200), Ok(false));
        assert_eq!(sniper_binding(), None);
    }

    // ── cwd test isolation ────────────────────────────────────────────────
    // cwd is a process-global; ALL cwd-mutating tests across the crate share ONE lock (see
    // `testsupport`) so they serialize regardless of module. This is the safe way to test
    // relative-path file IO.
    fn cwd_guard() -> crate::testsupport::CwdGuard {
        crate::testsupport::cwd_guard("editor_test")
    }
}
