// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Press-to-bind capture glue — the GUI side of the project's strongest UX rule: the user PRESSES
//! the control they want; we NEVER hardcode a button. (Memory: "DONT HARD CODE IT TO FUCKING
//! THUMB 2".)
//!
//! `neuron::capture` is the headless primitive (baseline-then-detect-newly-pressed VK, ESC cancels,
//! friendly `vk_name`). It is a blocking poll loop, so this module runs it on a worker thread and
//! posts the captured result back to the UI thread via `invoke_from_event_loop`.
//!
//! Threading: `invoke_from_event_loop`'s closure must be `Send`, but the per-bind handler closures
//! (defined in `glue.rs`) capture `&AppWindow` and are NOT `Send`. So the worker posts only the
//! plain result (a VK / a `CapturedControl`) across the boundary, and the handler — stashed in a
//! UI-thread-local — runs on the UI thread where `!Send` is fine. A shared `AtomicBool` lets the UI
//! cancel an in-flight capture.
//!
//! Capture only READS input state (`GetAsyncKeyState` / Raw-Input decode) — it never synthesizes
//! input, so it is NOT gated by the input-arm gate and is always safe (even with the live loop off).

use crate::ui::{AppWindow, State};
use slint::ComponentHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The handler run on the UI thread when a HID control capture completes (None = cancelled).
type CtlHandler = Box<dyn Fn(&AppWindow, Option<CapturedControl>)>;
/// The handler for a chord-aware capture: `(app, vk, held-modifier-names, friendly-name)`.
type ChordHandler = Box<dyn Fn(&AppWindow, i32, &[&str], &str)>;
/// The handler for a finished key-sequence recording (`None` = nothing recorded).
type SeqHandler = Box<dyn Fn(&AppWindow, Option<String>)>;

thread_local! {
    /// The currently-active capture's cancel flag (a new capture supersedes the prior one).
    static CANCEL: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    /// Capture GENERATION token: each begin() bumps it, and a worker's posted finish_* carries the
    /// generation it was started under. A superseded worker's late completion (it only notices the
    /// stop flag on its next poll) must NOT tear down the prompt/handler of the capture that
    /// replaced it — the guard makes stale completions inert.
    static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The pending control-capture handler.
    static CTL_HANDLER: std::cell::RefCell<Option<CtlHandler>> = const { std::cell::RefCell::new(None) };
    /// The pending chord-capture handler.
    static CHORD_HANDLER: std::cell::RefCell<Option<ChordHandler>> = const { std::cell::RefCell::new(None) };
    /// The pending sequence-recorder handler.
    static SEQ_HANDLER: std::cell::RefCell<Option<SeqHandler>> = const { std::cell::RefCell::new(None) };
    /// The weak window handle the worker posts back into (UI-thread-local so it isn't sent).
    static CAP_WINDOW: std::cell::RefCell<Option<slint::Weak<AppWindow>>> = const { std::cell::RefCell::new(None) };
}

/// A captured HID control: the semantic `(page, usage, pid)` of the device control the user pressed.
#[derive(Clone, Copy)]
pub struct CapturedControl {
    pub page: u16,
    pub usage: u16,
    pub pid: Option<u16>,
}

/// Process-global "a press-to-bind capture is in flight" — read by the live key DISPATCHER (which
/// runs on its own thread) so a key pressed in order to BIND it isn't ALSO dispatched as whatever
/// it's currently bound to. Set by every `begin*`, cleared on cancel and on the (non-stale) finish.
pub static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// A capture finished with NO window to deliver it to (the `slint::Weak` no longer upgrades).
///
/// The latch still has to come down. [`CAPTURE_ACTIVE`] is read by the live DISPATCHER on its own
/// thread to suppress dispatch while a key is being bound; every `finish_*` used to bail here
/// BEFORE clearing it, so a capture that outlived its window left the flag stuck true and the
/// dispatcher swallowing every input edge — binds dead process-wide with no visible cause and no
/// way back but a restart. The handler cells are dropped too: they hold a closure over the dead
/// window and can never run.
fn end_capture_without_window() {
    CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    neuron::intercept::set_paused(false);
    RECORDING.store(false, Ordering::Relaxed);
    // the stop flag belongs to the worker that just finished — drop it with everything else so no
    // stale cell survives into the next capture.
    CANCEL.with(|c| *c.borrow_mut() = None);
    CHORD_HANDLER.with(|h| *h.borrow_mut() = None);
    CTL_HANDLER.with(|h| *h.borrow_mut() = None);
    SEQ_HANDLER.with(|h| *h.borrow_mut() = None);
    CAP_WINDOW.with(|c| *c.borrow_mut() = None);
}

/// Cancel any in-flight press-to-bind capture (the dialog closed / a new capture started).
pub fn cancel() {
    CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    neuron::intercept::set_paused(false);
    CANCEL.with(|c| {
        if let Some(flag) = c.borrow().as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
        *c.borrow_mut() = None;
    });
}

/// Begin a CHORD-aware press-to-bind capture: the handler also receives the
/// modifier names held at the instant of the press (`["ctrl","shift"]`, press order ctrl→shift→
/// alt→win), read in the WORKER at capture time — by the time a UI-thread handler ran, the user
/// would already have let go. A pressed modifier captures as itself, never as its own chord.
pub fn begin_chord(app: &AppWindow, on_done: impl Fn(&AppWindow, i32, &[&str], &str) + 'static) {
    cancel();
    let gen = GENERATION.with(|g| {
        let v = g.get() + 1;
        g.set(v);
        v
    });
    let stop = Arc::new(AtomicBool::new(false));
    CANCEL.with(|c| *c.borrow_mut() = Some(stop.clone()));
    CHORD_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(on_done)));
    CAP_WINDOW.with(|c| *c.borrow_mut() = Some(app.as_weak()));

    let st = app.global::<State>();
    st.set_capture_active(true);
    CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
    neuron::intercept::set_paused(true);
    st.set_capture_prompt("press the key / button / chord you want — ESC to cancel".into());

    // Same "done clears the latch on every path" contract as `begin` above.
    crate::worker::spawn_notify(
        "neuron-chord-capture",
        move || {
            let vk = neuron::capture::capture_keypress_until(&stop);
            match vk {
                Some(v) => (v, held_modifiers(v), neuron::capture::vk_name(v)),
                None => (0, Vec::new(), "cancelled".to_string()),
            }
        },
        move |result| {
            let (code, mods, name) =
                result.unwrap_or_else(|| (0, Vec::new(), "cancelled".to_string()));
            let _ = slint::invoke_from_event_loop(move || finish_chord(gen, code, mods, name));
        },
    );
}

/// The modifier names held *right now*, excluding `pressed` itself (so a captured Ctrl press is
/// "ctrl", not "ctrl+ctrl"). Side-agnostic — chords serialize with the generic names.
#[cfg(windows)]
fn held_modifiers(pressed: i32) -> Vec<&'static str> {
    use neuron::capture::key_down;
    let mut mods = Vec::new();
    for (vks, name) in [
        (&[0x11, 0xA2, 0xA3][..], "ctrl"),
        (&[0x10, 0xA0, 0xA1][..], "shift"),
        (&[0x12, 0xA4, 0xA5][..], "alt"),
        (&[0x5B, 0x5C][..], "win"),
    ] {
        if vks.contains(&pressed) {
            continue;
        }
        if vks.iter().any(|&vk| key_down(vk)) {
            mods.push(name);
        }
    }
    mods
}

#[cfg(not(windows))]
fn held_modifiers(_pressed: i32) -> Vec<&'static str> {
    Vec::new()
}

fn finish_chord(gen: u64, code: i32, mods: Vec<&'static str>, name: String) {
    if GENERATION.with(|g| g.get()) != gen {
        return;
    }
    let weak = CAP_WINDOW.with(|c| c.borrow().clone());
    let Some(app) = weak.and_then(|w| w.upgrade()) else {
        end_capture_without_window();
        return;
    };
    let st = app.global::<State>();
    st.set_capture_active(false);
    CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    neuron::intercept::set_paused(false);
    st.set_capture_prompt("".into());
    let handler = CHORD_HANDLER.with(|h| h.borrow_mut().take());
    if let Some(h) = handler {
        h(&app, code, &mods, &name);
    }
}

/// Begin a HID **control** capture (knob / mute / media / mic-tap — the controls `controls::listen`
/// decodes). `on_done(app, Some/None)` runs on the UI thread. The press-to-bind path for
/// `Trigger::Input` rules (distinct from VK capture, which is for the cast/sniper buttons).
pub fn begin_control(
    app: &AppWindow,
    on_done: impl Fn(&AppWindow, Option<CapturedControl>) + 'static,
) {
    cancel();
    let gen = GENERATION.with(|g| {
        let v = g.get() + 1;
        g.set(v);
        v
    });
    let stop = Arc::new(AtomicBool::new(false));
    CANCEL.with(|c| *c.borrow_mut() = Some(stop.clone()));
    CTL_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(on_done)));
    CAP_WINDOW.with(|c| *c.borrow_mut() = Some(app.as_weak()));

    let st = app.global::<State>();
    st.set_capture_active(true);
    CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
    neuron::intercept::set_paused(true);
    st.set_capture_prompt(
        "press the device control (knob / mute / media / macro key / mic-tap) — ESC to cancel".into(),
    );

    // Same "done clears the latch on every path" contract as `begin` above.
    crate::worker::spawn_notify(
        "neuron-control-capture",
        move || {
            let captured = capture_control_until(&stop);
            captured.map(|c| (c.page, c.usage, c.pid))
        },
        move |pkt| {
            let _ = slint::invoke_from_event_loop(move || finish_ctl(gen, pkt.flatten()));
        },
    );
}

/// Run the stashed control handler on the UI thread (stale generations are inert; see finish_vk).
fn finish_ctl(gen: u64, pkt: Option<(u16, u16, Option<u16>)>) {
    if GENERATION.with(|g| g.get()) != gen {
        return;
    }
    let weak = CAP_WINDOW.with(|c| c.borrow().clone());
    let Some(app) = weak.and_then(|w| w.upgrade()) else {
        end_capture_without_window();
        return;
    };
    let st = app.global::<State>();
    st.set_capture_active(false);
    CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    neuron::intercept::set_paused(false);
    st.set_capture_prompt("".into());
    let captured = pkt.map(|(page, usage, pid)| CapturedControl { page, usage, pid });
    let handler = CTL_HANDLER.with(|h| h.borrow_mut().take());
    if let Some(h) = handler {
        h(&app, captured);
    }
}

/// Listen for the first HID control press (or ESC/stop/timeout). Returns its semantic identity.
#[cfg(windows)]
fn capture_control_until(stop: &AtomicBool) -> Option<CapturedControl> {
    use std::cell::Cell;
    let found: Cell<Option<CapturedControl>> = Cell::new(None);
    neuron::controls::listen_until(
        Some(30), // hard 30s cap so a forgotten capture can't run forever
        stop,
        true, // interactive press-to-bind — ESC cancels, as the panel says
        |ev| {
            if let Some(&(page, usage)) = ev.hits.first() {
                // The LEFT mouse button operates this dialog (the "cancel" button + the scrim-click
                // dismiss). It is not a bindable "device control", so never capture it here —
                // otherwise clicking cancel binds mouse-1 instead of cancelling. Side buttons (2-5),
                // the knob, media keys, mic-tap and macro keys all still capture normally.
                if (page, usage) == (0x09, 1) {
                    return;
                }
                // Macro keys are LOGICAL controls (M5 is M5 on any board) riding a synthetic
                // edge-bucket pid; bind them DEVICE-ANY so the label stays clean ("Macro M5", no
                // @pid) and a replug/keyboard-swap keeps the binding. Every other control keeps its
                // real device pid (a knob on headset A is not the knob on headset B).
                let pid = if page == neuron::controls::RAZER_MACRO_PAGE {
                    None
                } else {
                    u16::from_str_radix(&ev.pid, 16).ok()
                };
                found.set(Some(CapturedControl { page, usage, pid }));
                stop.store(true, Ordering::Relaxed);
            }
        },
        // on_tick: nothing to poll during a one-shot interactive capture. The returned Duration
        // is a pump-cadence hint for the future blocking-wait rewrite (ignored today).
        || std::time::Duration::from_millis(5),
    );
    found.get()
}

#[cfg(not(windows))]
fn capture_control_until(_stop: &AtomicBool) -> Option<CapturedControl> {
    None
}

// ── the SEQUENCE RECORDER ──────────────────────────────────────────────────────────────────────
// Records your real keys with your real timing and writes the sequence GRAMMAR (`w:180 ~90 a`) —
// the macro stays readable text you can trim and retime by hand, never a black box. Keyboard only
// (the mouse keeps working the UI, and clicking STOP must not record itself). ESC or STOP ends
// the take; whatever was played stays.

/// Whether a sequence recording is currently running (drives the REC/STOP toggle).
static RECORDING: AtomicBool = AtomicBool::new(false);

/// True while the key-sequence recorder is rolling.
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// Toggle-off for an in-flight recording: the worker notices and posts the take.
pub fn stop_recording() {
    cancel(); // the shared stop flag doubles as "take is done"
}

/// Begin recording a key sequence. `on_done(app, Some(grammar))` runs on the UI thread when the
/// take ends (ESC / [`stop_recording`]); `None` if nothing was played.
pub fn begin_keyseq(app: &AppWindow, on_done: impl Fn(&AppWindow, Option<String>) + 'static) {
    cancel();
    let gen = GENERATION.with(|g| {
        let v = g.get() + 1;
        g.set(v);
        v
    });
    let stop = Arc::new(AtomicBool::new(false));
    CANCEL.with(|c| *c.borrow_mut() = Some(stop.clone()));
    SEQ_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(on_done)));
    CAP_WINDOW.with(|c| *c.borrow_mut() = Some(app.as_weak()));
    RECORDING.store(true, Ordering::Relaxed);

    let st = app.global::<State>();
    st.set_capture_active(true);
    CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
    neuron::intercept::set_paused(true);
    st.set_capture_prompt(
        "RECORDING — play the keys with your real timing · STOP or ESC ends the take".into(),
    );

    // RECORDING (set above) must clear even if the worker never runs or panics mid-take, or the
    // REC/STOP toggle would wedge in "recording" forever.
    crate::worker::spawn_notify(
        "neuron-seq-recorder",
        move || record_keyseq_until(&stop),
        move |grammar| {
            RECORDING.store(false, Ordering::Relaxed);
            let _ = slint::invoke_from_event_loop(move || finish_seq(gen, grammar.flatten()));
        },
    );
}

fn finish_seq(gen: u64, grammar: Option<String>) {
    if GENERATION.with(|g| g.get()) != gen {
        return;
    }
    let weak = CAP_WINDOW.with(|c| c.borrow().clone());
    let Some(app) = weak.and_then(|w| w.upgrade()) else {
        end_capture_without_window();
        return;
    };
    let st = app.global::<State>();
    st.set_capture_active(false);
    CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    neuron::intercept::set_paused(false);
    st.set_capture_prompt("".into());
    let handler = SEQ_HANDLER.with(|h| h.borrow_mut().take());
    if let Some(h) = handler {
        h(&app, grammar);
    }
}

/// The recording loop: poll keyboard edges, time holds and gaps, render the grammar. Holds under
/// 150 ms read as taps and gaps under 120 ms as "as fast as you can" — the grammar stays clean for
/// normal playing and exact where your timing was deliberate (rounded to 10 ms).
#[cfg(windows)]
fn record_keyseq_until(stop: &AtomicBool) -> Option<String> {
    use neuron::capture::{is_mouse_vk, key_down, VK_ESCAPE};
    use std::time::Instant;
    const HOLD_MIN_MS: u128 = 150;
    const GAP_MIN_MS: u128 = 120;
    let t0 = Instant::now();
    let mut down_at: [Option<Instant>; 256] = [None; 256];
    let mut base = [false; 256];
    for (vk, b) in base.iter_mut().enumerate() {
        *b = key_down(vk as i32);
    }
    // (press-time, vk, hold-ms) — assembled into tokens in press order at the end.
    let mut takes: Vec<(u128, i32, u128)> = Vec::new();
    loop {
        let done = key_down(VK_ESCAPE) || stop.load(Ordering::Relaxed);
        for vk in 1..256 {
            if vk == VK_ESCAPE || is_mouse_vk(vk) {
                continue;
            }
            let now = key_down(vk);
            match (down_at[vk as usize], now) {
                (None, true) if !base[vk as usize] => down_at[vk as usize] = Some(Instant::now()),
                (Some(t), false) => {
                    takes.push((
                        t.duration_since(t0).as_millis(),
                        vk,
                        t.elapsed().as_millis(),
                    ));
                    down_at[vk as usize] = None;
                }
                _ => {}
            }
            if !now {
                base[vk as usize] = false;
            }
        }
        if done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    // still-held keys at STOP count with their hold so far (the user is mid-gesture, keep it).
    for (vk, t) in down_at.iter().enumerate() {
        if let Some(t) = t {
            takes.push((
                t.duration_since(t0).as_millis(),
                vk as i32,
                t.elapsed().as_millis(),
            ));
        }
    }
    if takes.is_empty() {
        return None;
    }
    takes.sort_by_key(|(at, _, _)| *at);
    // CHORD FOLD: a held modifier whose span covers another key's press is part of THAT key
    // ("ctrl+s"), not a step of its own — sequential playback of "ctrl:300 s" would never chord.
    // A modifier pressed and released alone stays an honest key step.
    fn mod_name(vk: i32) -> Option<&'static str> {
        match vk {
            0x11 | 0xA2 | 0xA3 => Some("ctrl"),
            0x10 | 0xA0 | 0xA1 => Some("shift"),
            0x12 | 0xA4 | 0xA5 => Some("alt"),
            0x5B | 0x5C => Some("win"),
            _ => None,
        }
    }
    let mut folded: Vec<bool> = vec![false; takes.len()];
    let mut names: Vec<String> = takes
        .iter()
        .map(|(_, vk, _)| neuron::action::key_param_for_vk(*vk as u16))
        .collect();
    for i in 0..takes.len() {
        let (at, vk, hold) = takes[i];
        let Some(m) = mod_name(vk) else { continue };
        let mut wrapped = false;
        for j in 0..takes.len() {
            if i != j && mod_name(takes[j].1).is_none() && (at..=at + hold).contains(&takes[j].0) {
                names[j] = format!("{m}+{}", names[j]);
                wrapped = true;
            }
        }
        folded[i] = wrapped;
    }
    let mut out: Vec<String> = Vec::new();
    let mut prev_end: Option<u128> = None;
    for (i, (at, _, hold)) in takes.iter().enumerate() {
        if folded[i] {
            continue;
        }
        if let Some(end) = prev_end {
            let gap = at.saturating_sub(end);
            if gap >= GAP_MIN_MS {
                out.push(format!("~{}", (gap / 10) * 10));
            }
        }
        out.push(if *hold >= HOLD_MIN_MS {
            format!("{}:{}", names[i], (hold / 10) * 10)
        } else {
            names[i].clone()
        });
        prev_end = Some(at + hold);
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join(" "))
}

#[cfg(not(windows))]
fn record_keyseq_until(_stop: &AtomicBool) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every capture flow shares one process-global latch, [`CAPTURE_ACTIVE`], which the live
    /// dispatcher reads to suppress dispatch while a control is being bound. If a capture ever
    /// ends WITHOUT lowering it, the dispatcher swallows every input edge from then on: no binds
    /// fire, nothing on screen says why, and only a restart clears it. That is the most severe
    /// failure this module can produce, and until now the file had no tests at all.
    ///
    /// The dead-window path is the one that can reach it: the worker completes after the window
    /// is gone (close-to-tray hides rather than drops the handle, so this needs a genuinely
    /// dropped window — process teardown, or any future path that releases it), the `Weak` fails
    /// to upgrade, and each `finish_*` returned early. Every one of the four now routes through
    /// `end_capture_without_window`, which is what this pins.
    #[test]
    fn a_capture_that_outlives_its_window_never_leaves_the_dispatcher_gated() {
        // Simulate the state a live capture leaves behind, then the dead-window completion.
        CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
        neuron::intercept::set_paused(true);
        RECORDING.store(true, Ordering::Relaxed);
        CAP_WINDOW.with(|c| *c.borrow_mut() = None); // a Weak that cannot upgrade

        end_capture_without_window();

        assert!(
            !CAPTURE_ACTIVE.load(Ordering::Relaxed),
            "the dispatcher gate MUST come down — a stuck latch kills every binding process-wide"
        );
        assert!(
            !RECORDING.load(Ordering::Relaxed),
            "the REC/STOP toggle must not wedge in 'recording' either"
        );
        assert!(
            !neuron::intercept::paused(),
            "the interceptor pause must release too — a leaked pause silently kills every \
             device remap AND the cast-trigger swallow until the next capture completes"
        );
        CHORD_HANDLER.with(|h| assert!(h.borrow().is_none(), "handler cells hold closures over the dead window"));
        CTL_HANDLER.with(|h| assert!(h.borrow().is_none()));
        SEQ_HANDLER.with(|h| assert!(h.borrow().is_none()));
    }

    /// `cancel` is the ESC / dialog-closed path. It must lower the latch and ARM the stop flag,
    /// but it deliberately does NOT bump the generation: the in-flight worker still owns its
    /// generation and delivers a `(0, "cancelled")` completion, which the handler interprets as
    /// "no bind was made". Pinning that here so a future "tidy up" doesn't turn a cancel into a
    /// silent bind of VK 0.
    #[test]
    fn cancel_lowers_the_gate_and_arms_the_stop_flag() {
        let stop = Arc::new(AtomicBool::new(false));
        CANCEL.with(|c| *c.borrow_mut() = Some(stop.clone()));
        CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
        neuron::intercept::set_paused(true);

        cancel();

        assert!(!CAPTURE_ACTIVE.load(Ordering::Relaxed), "cancel lowers the dispatcher gate");
        assert!(!neuron::intercept::paused(), "cancel releases the interceptor pause");
        assert!(stop.load(Ordering::Relaxed), "the capture worker is told to stop");
        CANCEL.with(|c| assert!(c.borrow().is_none(), "the stop cell is cleared for the next capture"));
    }

    /// A STALE completion (its capture was superseded by a newer one, so the generation moved) is
    /// inert: it must not lower a gate the NEW capture raised, and must not consume the new
    /// capture's handler. This is the supersession contract every `finish_*` opens with, and it is
    /// what keeps "click bind, change your mind, click bind again" from eating the second bind.
    #[test]
    fn a_stale_completion_cannot_disturb_the_capture_that_replaced_it() {
        let stale_gen = GENERATION.with(|g| {
            let v = g.get() + 1;
            g.set(v);
            v
        });
        // the NEWER capture bumps the generation and raises the gate
        GENERATION.with(|g| g.set(g.get() + 1));
        CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
        neuron::intercept::set_paused(true);
        CAP_WINDOW.with(|c| *c.borrow_mut() = None);

        // the older worker lands late — with no window, so it would otherwise take the
        // dead-window path and clear everything the new capture just set up.
        finish_ctl(stale_gen, None);

        assert!(
            CAPTURE_ACTIVE.load(Ordering::Relaxed),
            "a superseded completion must not lower the live capture's gate"
        );
        CAPTURE_ACTIVE.store(false, Ordering::Relaxed); // leave the global clean for other tests
        neuron::intercept::set_paused(false);
    }
}
