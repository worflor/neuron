// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Audition harness for the tone synth — renders the palettes, a "spam" burst, and the semantic
//! notification cues to WAV files so the sound can be judged by ear (and inspected) before any
//! real-time output exists. Pure offline render; no audio device touched.
//!
//!   cargo run -p neuron --example tone_demo
//!
//! Writes _audio/*.wav in the run directory and prints a peak/RMS sanity line per file.

use neuron::tone::{pentatonic_hz, render, Hit, Timbre};
use std::io::Write;

const SR: f32 = 48_000.0;
const MASTER: f32 = 0.55;
const ROOT: i32 = 3; // A4 + 3 semitones = C5 (~523 Hz): a pleasant mid key.

fn main() {
    let _ = std::fs::create_dir_all("_audio");

    // ── the four palettes, each playing the same short rising pentatonic phrase ──
    for (name, timbre) in [
        ("pulse", Timbre::PULSE),
        ("warm", Timbre::WARM),
        ("glass", Timbre::GLASS),
    ] {
        let hits = phrase(&[0, 1, 2, 4], ROOT, 150.0, 0.85, timbre, 0.0);
        let buf = render(&hits, samples_ms(1400.0), SR, MASTER);
        save(&format!("_audio/tone_{name}.wav"), &buf);
    }

    // ── the SPAM test: ten cues in ~1.4 s, each stepping up the pentatonic so a burst reads as a
    // rising wind-chime run instead of a jackhammer. This is the "not annoying when spammed" proof,
    // and the "consecutive ones play into each other" continuity. ──
    let mut burst = Vec::new();
    for i in 0..10 {
        burst.extend(phrase(
            &[i % 7],
            ROOT,
            0.0,
            0.8,
            Timbre::PULSE,
            i as f32 * 140.0,
        ));
    }
    let buf = render(&burst, samples_ms(2200.0), SR, MASTER);
    save("_audio/tone_burst.wav", &buf);

    // contrast: the SAME ten, all on one note (what a naive system does). Deliberately included so
    // the difference is audible — this one should feel naggy; the burst above should not.
    let mut nag = Vec::new();
    for i in 0..10 {
        nag.extend(phrase(&[0], ROOT, 0.0, 0.8, Timbre::PULSE, i as f32 * 140.0));
    }
    let buf = render(&nag, samples_ms(2200.0), SR, MASTER);
    save("_audio/tone_burst_naive.wav", &buf);

    // ── the semantic cues, spaced so each reads on its own (what a real notification sounds like) ──
    // direction encodes meaning: ranged-up rises, ranged-down falls; a profile switch is a little
    // major arpeggio; a layer engage is a two-note "lock".
    let mut cues = Vec::new();
    let mut t = 0.0;
    let mut add = |degs: &[i32], timbre: Timbre, ioi: f32, t: &mut f32| {
        cues.extend(phrase(degs, ROOT, ioi, 0.85, timbre, *t));
        *t += ioi * degs.len() as f32 + 650.0; // a gap between cues
    };
    add(&[2, 4], Timbre::PULSE, 95.0, &mut t); // DPI up  (rises)
    add(&[4, 2], Timbre::PULSE, 95.0, &mut t); // DPI down (falls)
    add(&[1, 3], Timbre::PULSE, 95.0, &mut t); // brightness up
    add(&[0, 2, 4], Timbre::GLASS, 105.0, &mut t); // profile switch (arpeggio)
    add(&[4, 0], Timbre::PULSE, 70.0, &mut t); // layer engage (two-note lock)
    let total = t + 800.0;
    let buf = render(&cues, samples_ms(total), SR, MASTER);
    save("_audio/tone_cues.wav", &buf);

    // ── the CHOSEN voice (PULSE): the burst + the cues you liked, now in the timbre you liked ──
    let mut pb = Vec::new();
    for i in 0..10 {
        pb.extend(phrase(&[i % 7], ROOT, 0.0, 0.8, Timbre::PULSE, i as f32 * 140.0));
    }
    save("_audio/pulse_burst.wav", &render(&pb, samples_ms(2200.0), SR, MASTER));

    let mut pc = Vec::new();
    let mut tt = 0.0f32;
    for (degs, ioi) in [
        (vec![2, 4], 95.0),    // DPI up
        (vec![4, 2], 95.0),    // DPI down
        (vec![1, 3], 95.0),    // brightness up
        (vec![0, 2, 4], 110.0), // profile switch (arpeggio)
        (vec![4, 0], 70.0),    // layer engage
    ] {
        pc.extend(phrase(&degs, ROOT, ioi, 0.85, Timbre::PULSE, tt));
        tt += ioi * degs.len() as f32 + 650.0;
    }
    save("_audio/pulse_cues.wav", &render(&pc, samples_ms(tt + 800.0), SR, MASTER));

    println!("\nwrote _audio/*.wav — palettes, bursts, and semantic cues (incl. pulse_*).");
}

/// Build a phrase: each degree struck `ioi_ms` apart, starting at `start_ms`.
fn phrase(degrees: &[i32], root: i32, ioi_ms: f32, vel: f32, timbre: Timbre, start_ms: f32) -> Vec<Hit> {
    degrees
        .iter()
        .enumerate()
        .map(|(i, &d)| Hit {
            at: samples_ms(start_ms + i as f32 * ioi_ms),
            freq: pentatonic_hz(d, root),
            vel,
            timbre,
        })
        .collect()
}

fn samples_ms(ms: f32) -> usize {
    (ms * SR / 1000.0) as usize
}

fn save(path: &str, buf: &[f32]) {
    let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    let rms = (buf.iter().map(|&s| s * s).sum::<f32>() / buf.len().max(1) as f32).sqrt();
    match write_wav(path, buf) {
        Ok(()) => println!("{path:<28} peak {peak:.3}  rms {rms:.3}  {:.2}s", buf.len() as f32 / SR),
        Err(e) => eprintln!("{path}: {e}"),
    }
}

/// Minimal 16-bit mono PCM WAV writer (no dependency).
fn write_wav(path: &str, samples: &[f32]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let sr = SR as u32;
    let data_len = (samples.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&(sr * 2).to_le_bytes())?; // byte rate
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits per sample
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    Ok(())
}
