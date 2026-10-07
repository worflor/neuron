// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Render Neuron presets offline, frame by frame, through the real pattern engine.
//!
//!     promo-pattern-export script.json out_dir
//!
//! script.json: { "fps": 30, "frames": N,
//!                "grids": [{"name": "kb", "rows": 6, "cols": 22}, ...],
//!                "segments": [{"frame": 0, "preset": "off"}, {"frame": 412, "preset": "fire"},
//!                             {"frame": 600, "preset": "comet", "pairing": 0}, ...],
//!                "keys": [[frame, vk, down], ...] }
//! Writes <out_dir>/<grid>.bin: N frames of rows*cols RGB bytes, row-major.
//! A segment starts a fresh compositor, as applying a preset does; "off" renders black. `pairing` is
//! an index into `neuron::pairing::pairings_for(preset)`, the same chip the lighting page offers.
//! Keys reach the live-input patterns through `capture::script_key_reads`, never the OS.

use std::{env, fs, io::Write, path::Path};

use serde_json::Value;

/// The layer a segment applies: the preset, or the preset with its `pairing`-th suggested pairing.
fn layer_for(slug: &str, pairing: Option<usize>) -> Option<neuron::pattern::LayerDef> {
    match pairing {
        None => neuron::pattern::preset_layer(slug),
        Some(i) => {
            let p = neuron::pairing::pairings_for(slug).into_iter().nth(i)?;
            neuron::pairing::pairing_layer(slug, &p)
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let script_path = args.next().ok_or("usage: promo-pattern-export script.json out_dir")?;
    let out_dir = args.next().ok_or("usage: promo-pattern-export script.json out_dir")?;
    let script: Value = serde_json::from_str(&fs::read_to_string(script_path)?)?;
    let fps = script["fps"].as_f64().ok_or("fps")? as f32;
    let frames = script["frames"].as_u64().ok_or("frames")? as usize;

    let mut segments: Vec<(usize, String, Option<usize>)> = script["segments"].as_array().ok_or("segments")?.iter()
        .map(|s| (
            s["frame"].as_u64().unwrap_or(0) as usize,
            s["preset"].as_str().unwrap_or("off").to_string(),
            s["pairing"].as_u64().map(|p| p as usize),
        ))
        .collect();
    segments.sort_by_key(|s| s.0);
    for (_, slug, pairing) in &segments {
        if slug != "off" && layer_for(slug, *pairing).is_none() {
            return Err(format!("unknown preset or pairing {slug} {pairing:?}").into());
        }
    }
    let mut keys: Vec<(usize, i32, bool)> = script["keys"].as_array().ok_or("keys")?.iter()
        .map(|k| (k[0].as_u64().unwrap_or(0) as usize, k[1].as_i64().unwrap_or(0) as i32, k[2].as_bool().unwrap_or(false)))
        .collect();
    keys.sort_by_key(|k| k.0);

    let grids: Vec<(String, u8, u8)> = script["grids"].as_array().ok_or("grids")?.iter()
        .map(|g| (g["name"].as_str().unwrap_or("grid").to_string(),
                  g["rows"].as_u64().unwrap_or(1) as u8, g["cols"].as_u64().unwrap_or(1) as u8))
        .collect();

    fs::create_dir_all(&out_dir)?;
    let scripted = neuron::capture::script_key_reads();
    let mut outs: Vec<Vec<u8>> = grids.iter().map(|(_, r, c)| Vec::with_capacity(frames * *r as usize * *c as usize * 3)).collect();
    let mut comps: Vec<Option<neuron::pattern::Compositor>> = grids.iter().map(|_| None).collect();
    let mut seg_start = 0usize;
    let mut next_seg = 0usize;
    let mut next_key = 0usize;

    for f in 0..frames {
        while next_seg < segments.len() && segments[next_seg].0 <= f {
            let (slug, pairing) = (&segments[next_seg].1, segments[next_seg].2);
            seg_start = segments[next_seg].0;
            for comp in comps.iter_mut() {
                *comp = layer_for(slug, pairing).map(|l| neuron::pattern::Compositor::from_defs(&[l]));
            }
            next_seg += 1;
        }
        let mut changed = false;
        while next_key < keys.len() && keys[next_key].0 <= f {
            scripted.set(keys[next_key].1, keys[next_key].2);
            changed = true;
            next_key += 1;
        }
        if changed {
            neuron::capture::note_key_transition();
        }
        let t = (f - seg_start) as f32 / fps;
        for (gi, (_, rows, cols)) in grids.iter().enumerate() {
            let n = *rows as usize * *cols as usize;
            match comps[gi].as_mut() {
                Some(comp) => {
                    let px = comp.render(*rows, *cols, t);
                    for p in px.iter().take(n) {
                        outs[gi].extend_from_slice(&[p.r, p.g, p.b]);
                    }
                    for _ in px.len()..n {
                        outs[gi].extend_from_slice(&[0, 0, 0]);
                    }
                }
                None => outs[gi].extend(std::iter::repeat(0u8).take(n * 3)),
            }
        }
    }
    for (gi, (name, _, _)) in grids.iter().enumerate() {
        let mut file = fs::File::create(Path::new(&out_dir).join(format!("{name}.bin")))?;
        file.write_all(&outs[gi])?;
    }
    eprintln!("rendered {frames} frames x {} grids through the pattern engine", grids.len());
    Ok(())
}
