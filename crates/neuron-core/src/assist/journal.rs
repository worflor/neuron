// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A local record of colour picking, in Luma's session log format (`luma.session-log.v1`), for
//! someone who wants to give a colour model their own picks to learn from.
//!
//! One JSON object per line, appended to a file the caller names. A record holds numbers and a
//! few fixed words: a random session id, a sequence number, milliseconds since the session began,
//! colours, stop positions, the stop being moved, which kind of control moved it, and what was
//! suggested and taken. It has no field for a name, a path, a device, an application or the time
//! of day, and nothing here opens a connection: the file stays where it is written until its
//! owner moves it.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value};

use crate::lighting::Rgb;
use crate::spectrum::Stop;

const SCHEMA: &str = "luma.session-log.v1";
/// The most palettes a `suggestion_shown` record may list.
const MAX_SLATE: usize = 32;

/// The kind of control a colour was moved with, in the format's own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// A two-axis colour area.
    Plane,
    /// A one-axis strip.
    Slider,
    /// Anything else: a typed value, a swatch.
    Other,
}

impl Control {
    fn word(self) -> &'static str {
        match self {
            Control::Plane => "sv-square",
            Control::Slider => "slider",
            Control::Other => "other",
        }
    }
}

/// One picking session being written.
pub struct Journal {
    file: File,
    session: String,
    began: Instant,
    seq: u64,
}

fn colour(c: Rgb) -> Value {
    let unit = |v: u8| (f64::from(v) / 255.0 * 1e5).round() / 1e5;
    json!([unit(c.r), unit(c.g), unit(c.b)])
}

fn colours(stops: &[Stop]) -> Value {
    stops.iter().map(|s| colour(s.col)).collect()
}

impl Journal {
    /// Begin a session on `palette`, appending to `path`. The session id is random and says
    /// nothing about the machine.
    pub fn start(path: &Path, palette: &[Stop]) -> io::Result<Journal> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let mut journal = Journal { file, session: format!("{random:016x}"), began: Instant::now(), seq: 0 };
        let positions: Value = palette.iter().map(|s| f64::from(s.at.clamp(0.0, 1.0))).collect();
        journal.write("session_start", json!({ "schema": SCHEMA, "palette": colours(palette), "positions": positions }))?;
        Ok(journal)
    }

    fn write(&mut self, event: &str, mut record: Value) -> io::Result<()> {
        let ms = (self.began.elapsed().as_secs_f64() * 1e5).round() / 100.0;
        record["session"] = json!(self.session);
        record["seq"] = json!(self.seq);
        record["t_ms"] = json!(ms);
        record["event"] = json!(event);
        self.seq += 1;
        writeln!(self.file, "{record}")?;
        self.file.flush()
    }

    /// Stop `active` now shows `rgb`, moved with `control`.
    pub fn drag(&mut self, active: usize, rgb: Rgb, control: Control) -> io::Result<()> {
        self.write("drag", json!({ "active": active, "rgb": colour(rgb), "control": control.word() }))
    }

    /// The palette was put back to `palette`.
    pub fn undo(&mut self, palette: &[Stop]) -> io::Result<()> {
        self.write("undo", json!({ "palette": colours(palette) }))
    }

    /// These palettes were suggested, in this order. An empty slate writes nothing.
    pub fn shown(&mut self, slate: &[&[Stop]]) -> io::Result<()> {
        if slate.is_empty() {
            return Ok(());
        }
        let options: Value = slate.iter().take(MAX_SLATE).map(|stops| colours(stops)).collect();
        self.write("suggestion_shown", json!({ "slate": options }))
    }

    /// Suggestion `choice` of the last slate shown was taken.
    pub fn accepted(&mut self, choice: usize) -> io::Result<()> {
        self.write("suggestion_accepted", json!({ "choice": choice.min(MAX_SLATE - 1) }))
    }

    /// The last slate shown went away with nothing taken.
    pub fn dismissed(&mut self) -> io::Result<()> {
        self.write("suggestion_dismissed", json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_written_in_the_fields_the_format_allows() {
        let path = std::env::temp_dir().join(format!("neuron-picker-journal-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let palette = [Stop::new(Rgb::new(255, 0, 0), 0.0), Stop::new(Rgb::new(0, 0, 255), 1.0)];
        let mut journal = Journal::start(&path, &palette).unwrap();
        journal.drag(1, Rgb::new(0, 128, 255), Control::Plane).unwrap();
        journal.shown(&[]).unwrap();
        journal.shown(&[&palette, &palette]).unwrap();
        journal.accepted(1).unwrap();
        journal.undo(&palette).unwrap();
        journal.dismissed().unwrap();
        let lines: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        drop(journal);
        let _ = std::fs::remove_file(&path);
        let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
        assert_eq!(events, ["session_start", "drag", "suggestion_shown", "suggestion_accepted", "undo", "suggestion_dismissed"]);
        let allowed = ["schema", "session", "seq", "t_ms", "event", "active", "rgb", "pointer_x", "pointer_y", "control", "palette", "positions", "slate", "slate_positions", "choice"];
        for (i, line) in lines.iter().enumerate() {
            assert!(line.as_object().unwrap().keys().all(|key| allowed.contains(&key.as_str())), "{line}");
            assert_eq!(line["seq"], i as u64);
            let session = line["session"].as_str().unwrap();
            assert!(session.len() == 16 && session.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{session}");
            assert!(line["t_ms"].as_f64().unwrap() >= 0.0);
        }
        assert_eq!(lines[0]["schema"], SCHEMA);
        assert_eq!(lines[0]["positions"], json!([0.0, 1.0]));
        assert_eq!(lines[1]["rgb"], json!([0.0, 0.50196, 1.0]));
        assert_eq!(lines[1]["control"], "sv-square");
        assert_eq!(lines[2]["slate"].as_array().unwrap().len(), 2);
    }
}
