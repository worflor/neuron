//! The NOTIFICATION ENGINE — the consumer end of [`neuron::confirm`].
//!
//! A dedicated thread drains the confirmation channel and, for each one, does TWO independent things
//! (the two-axis model): it plays an audio cue (if the audio axis is on) and shows a card (if the
//! placement axis isn't "off"). Either, both, or neither — "sound-only" is just audio-on with
//! placement off. Rapid repeats COALESCE the card in place AND walk the tone up the pentatonic, so
//! cycling DPI is one card that updates and a rising run of tones — never a stack of five, never a
//! jackhammer.
//!
//! The card draws in the beacon's Signal grammar (`draw_card`) on its own overlay; the sound is the
//! `neuron::tone` synth driven through [`crate::sound`]. Both are gated by the live prefs on every
//! confirmation, so toggles take effect immediately.

use crate::prefs::Prefs;
use crate::sound::SoundEngine;
use neuron::confirm::{Confirmation, Kind, Shape};
use neuron::tone::{pentatonic_hz, Timbre};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long a card holds at full strength before fading (refreshed by any new confirmation).
const HOLD: Duration = Duration::from_millis(1200);
/// The musical key: A4 + 3 semitones = C — a calm mid root for the pentatonic cues.
const ROOT: i32 = 3;
/// Cues within this window of the previous one form one rising phrase (the burst continuity); a
/// longer gap resets to each event's own leitmotif anchor.
const CUE_WINDOW: Duration = Duration::from_millis(1600);

/// Drain confirmations forever. Returns only when the channel's sender is dropped (app shutdown).
pub fn run(rx: Receiver<Confirmation>) {
    // The card overlay is created lazily; the audio engine opens its output stream once up front
    // (silent until struck). Either may be absent — no audio device, or notifications off — and both
    // paths then degrade to no-ops.
    let mut overlay: Option<crate::overlay::SpellOverlay> = None;
    let mut sound = SoundEngine::new(crate::prefs::notif_volume());
    let mut music = Music::new();

    loop {
        let first = match rx.recv() {
            Ok(c) => c,
            Err(_) => return,
        };
        let Some(act) = decide(&first) else { continue };
        if act.audio {
            music.play(&first, &mut sound, act.vol, act.tid);
        }
        let Some(card) = act.card else { continue }; // audio-only (or gated): no card, no hold loop.
        let ov = overlay.get_or_insert_with(crate::overlay::SpellOverlay::spawn);
        ov.begin(card);

        // a card is up: hold it, coalescing new confirmations in place (and sounding their tones)
        // until quiet.
        let mut deadline = Instant::now() + HOLD;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(c) => {
                    if let Some(act) = decide(&c) {
                        if act.audio {
                            music.play(&c, &mut sound, act.vol, act.tid);
                        }
                        if let Some(card) = act.card {
                            ov.begin(card); // coalesce: replace in place, reset the hold.
                            deadline = Instant::now() + HOLD;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    ov.end(); // hold lapsed — fade it out.
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// What a confirmation should do, after the live config: play audio? show a card? + the audio params.
struct Act {
    audio: bool,
    card: Option<crate::overlay::WeaveMode>,
    vol: f32,
    tid: u8,
}

/// Apply the live prefs to a confirmation. `None` = master off or this kind muted (do nothing).
fn decide(c: &Confirmation) -> Option<Act> {
    let p = Prefs::load();
    if !p.notif_enabled || !p.notif_kind_on(c.kind) {
        return None;
    }
    let card = p.notif_place_code().map(|place| {
        let (title, body) = format_card(c);
        crate::overlay::WeaveMode::Notify {
            title,
            body,
            place,
            panel: p.notif_panel,
        }
    });
    Some(Act {
        audio: p.notif_audio,
        card,
        vol: p.notif_volume,
        tid: Timbre::id_of(&p.notif_sound),
    })
}

/// The running musical state — turns a stream of confirmations into a never-clashing, emergent line.
struct Music {
    deg: i32,
    at: Instant,
}

impl Music {
    fn new() -> Music {
        Music {
            deg: 0,
            at: Instant::now() - Duration::from_secs(60),
        }
    }

    /// Strike the cue for `c` through `sound`, advancing the melodic state.
    fn play(&mut self, c: &Confirmation, sound: &mut Option<SoundEngine>, vol: f32, tid: u8) {
        let Some(eng) = sound.as_mut() else { return };
        eng.set_volume(vol);
        let now = Instant::now();
        for (deg, vel, delay) in self.cue(c, now) {
            eng.strike(pentatonic_hz(deg, ROOT), vel, tid, delay);
        }
    }

    /// Build the note gesture. The START degree rises within a burst (continuity) or resets to the
    /// event's leitmotif anchor; the SHAPE picks the gesture — ranged values rise/fall by direction,
    /// a profile switch arpeggiates, a layer "locks" with a two-note leap. Every degree is
    /// pentatonic, so nothing this can produce ever clashes.
    fn cue(&mut self, c: &Confirmation, now: Instant) -> Vec<(i32, f32, u16)> {
        let recent = now.duration_since(self.at) < CUE_WINDOW;
        let base = if recent {
            let d = self.deg + 1;
            if d > 8 { 0 } else { d } // rise a while, then fall home so a long burst stays in range
        } else {
            kind_anchor(c.kind)
        };
        let notes = match &c.shape {
            Shape::Ranged { value, .. } => match direction(c.prev.as_deref(), *value) {
                Dir::Up => vec![(base, 0.78, 0), (base + 1, 0.85, 95)],
                Dir::Down => vec![(base + 1, 0.85, 0), (base, 0.78, 95)],
                Dir::Flat => vec![(base, 0.85, 0)],
            },
            Shape::Discrete { .. } => match c.kind {
                Kind::Profile => vec![(base, 0.76, 0), (base + 1, 0.82, 90), (base + 2, 0.86, 180)],
                Kind::Layer => vec![(base, 0.85, 0), (base + 2, 0.8, 80)],
                _ => vec![(base, 0.85, 0)],
            },
        };
        self.deg = notes.last().map(|n| n.0).unwrap_or(base);
        self.at = now;
        notes
    }
}

enum Dir {
    Up,
    Down,
    Flat,
}

/// Direction of a ranged change from its `prev` string to the new `value` (drives the rise/fall cue).
fn direction(prev: Option<&str>, value: f64) -> Dir {
    match prev.and_then(|p| p.parse::<f64>().ok()) {
        Some(p) if value > p => Dir::Up,
        Some(p) if value < p => Dir::Down,
        _ => Dir::Flat,
    }
}

/// Each event kind's home pentatonic degree — a recognizable pitch identity (leitmotif).
fn kind_anchor(k: Kind) -> i32 {
    match k {
        Kind::Profile => 0,
        Kind::Brightness | Kind::Scroll => 1,
        Kind::Dpi | Kind::Macro => 2,
        Kind::Polling => 3,
        Kind::Layer => 4,
    }
}

/// Turn a confirmation into the card's two lines: the noun on top, the value (with old→new) beneath.
fn format_card(c: &Confirmation) -> (String, String) {
    let body = match &c.shape {
        Shape::Ranged { value, unit, .. } => {
            let now = format_num(*value, unit);
            match &c.prev {
                Some(prev) => format!("{now}   was {prev}"),
                None => now,
            }
        }
        Shape::Discrete { label } => match &c.prev {
            Some(prev) if !prev.is_empty() => format!("{label}   \u{2190} {prev}"),
            _ => label.clone(),
        },
    };
    (c.title.clone(), body)
}

/// Format a ranged value with only its symbol unit — the title already carries the noun (so a DPI
/// card reads "1600", not "1600 DPI").
fn format_num(v: f64, unit: &str) -> String {
    let n = v.round() as i64;
    match unit {
        "%" => format!("{n}%"),
        "Hz" => format!("{n} Hz"),
        _ => format!("{n}"),
    }
}

/// Fire a TEST notification of a random currently-allowed kind, through the exact same emission path
/// a real change uses (so it genuinely exercises audio + card). Returns a status line for the UI.
pub fn fire_test() -> String {
    let p = Prefs::load();
    if !p.notif_enabled {
        return "notifications are off — enable them first".into();
    }
    if p.notif_place_code().is_none() && !p.notif_audio {
        return "placement and audio are both off — turn one on to test".into();
    }
    // the kinds a user can act on (macro is opt-in per binding, never a global test).
    let candidates = [
        Kind::Dpi,
        Kind::Scroll,
        Kind::Polling,
        Kind::Brightness,
        Kind::Profile,
        Kind::Layer,
    ];
    let allowed: Vec<Kind> = candidates
        .into_iter()
        .filter(|k| p.notif_kind_on(*k))
        .collect();
    if allowed.is_empty() {
        return "every event is muted — enable one to test".into();
    }
    let pick = allowed[pseudo_rand() % allowed.len()];
    emit_sample(pick);
    format!("test notification fired — {}", pick.slug())
}

/// Emit a representative confirmation for `kind` via the real typed constructors.
fn emit_sample(kind: Kind) {
    use neuron::confirm;
    match kind {
        Kind::Dpi => confirm::dpi(1600, Some(800)),
        Kind::Scroll => confirm::scroll(3, 5, Some(2)),
        Kind::Polling => confirm::polling(1000, Some(500)),
        Kind::Brightness => confirm::brightness(70, Some(40)),
        Kind::Profile => confirm::profile("gaming", Some("chill")),
        Kind::Layer => confirm::layer("sniper", true),
        Kind::Macro => confirm::macro_fired("test macro"),
    }
}

/// A throwaway index source for the test's random pick — no `rand` dependency, just the clock's
/// sub-second jitter. Good enough to vary which sample fires.
fn pseudo_rand() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0)
}
