// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A device's badge: the emblem and name every surface shows for it in place of its pid.
//!
//! Each part is the user's choice when they made one (`badges.toml` in the run root), else what
//! the device says about itself: its product string, and an emblem read from its top-level HID
//! collections (a gamepad collection is a pad, a pointer a mouse) or from its name. Lookups are a
//! map read; a device not yet seen is learned by one background enumeration, and [`generation`]
//! moves whenever a badge changes so views know to redraw.

use crate::registry::CanonicalPid;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The shapes a device can wear. Keys are stable: they are written to `badges.toml` and read by
/// the GUI's emblem renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Emblem {
    Mouse,
    Keyboard,
    Keypad,
    Pad,
    Stick,
    Headset,
    Mic,
    Dial,
    Device,
}

impl Emblem {
    pub const ALL: [Emblem; 9] = [
        Emblem::Mouse,
        Emblem::Keyboard,
        Emblem::Keypad,
        Emblem::Pad,
        Emblem::Stick,
        Emblem::Headset,
        Emblem::Mic,
        Emblem::Dial,
        Emblem::Device,
    ];

    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Emblem::Mouse => "mouse",
            Emblem::Keyboard => "keyboard",
            Emblem::Keypad => "keypad",
            Emblem::Pad => "pad",
            Emblem::Stick => "stick",
            Emblem::Headset => "headset",
            Emblem::Mic => "mic",
            Emblem::Dial => "dial",
            Emblem::Device => "device",
        }
    }

    #[must_use]
    pub fn parse(key: &str) -> Option<Emblem> {
        Emblem::ALL.into_iter().find(|g| g.key() == key)
    }

    /// How strongly a HID collection claims a device, when it exposes several: a pad's keyboard
    /// collection must not make it a keyboard.
    fn rank(self) -> u8 {
        match self {
            Emblem::Pad | Emblem::Stick | Emblem::Keypad | Emblem::Headset | Emblem::Mic => 5,
            Emblem::Mouse => 4,
            Emblem::Keyboard => 3,
            Emblem::Dial => 2,
            Emblem::Device => 0,
        }
    }
}

/// What a surface shows for one device.
#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub emblem: Emblem,
    pub name: String,
    /// The name the device gives itself (or its pid), shown as the hint behind a custom name.
    pub own_name: String,
    pub custom_emblem: bool,
    pub custom_name: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emblem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    device: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, Default)]
struct Learned {
    emblem: Option<Emblem>,
    name: String,
}

#[derive(Default)]
struct Book {
    user: BTreeMap<u16, Entry>,
    loaded: Option<PathBuf>,
    learned: HashMap<u16, Learned>,
    last_scan: Option<Instant>,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SCANNING: AtomicBool = AtomicBool::new(false);
const RESCAN: Duration = Duration::from_secs(3);

fn path() -> PathBuf {
    crate::runroot::run_root().join("badges.toml")
}

fn with_book<T>(f: impl FnOnce(&mut Book) -> T) -> T {
    let mut guard = BOOK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let book = guard.get_or_insert_with(Book::default);
    let p = path();
    if book.loaded.as_ref() != Some(&p) {
        book.user = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| toml::from_str::<File>(&s).ok())
            .map(|f| f.device.into_iter().filter_map(|(k, v)| Some((u16::from_str_radix(&k, 16).ok()?, v))).collect())
            .unwrap_or_default();
        book.loaded = Some(p);
    }
    f(book)
}

/// Views redraw badges when this changes.
#[must_use]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The badge for a device.
#[must_use]
pub fn of(pid: CanonicalPid) -> Badge {
    let (user, learned, stale) = with_book(|b| {
        let learned = b.learned.get(&pid.get()).cloned();
        let stale = learned.is_none() && b.last_scan.is_none_or(|t| t.elapsed() > RESCAN);
        (b.user.get(&pid.get()).cloned().unwrap_or_default(), learned.unwrap_or_default(), stale)
    });
    if stale {
        scan_in_background();
    }
    let custom_emblem = user.emblem.as_deref().and_then(Emblem::parse);
    let custom_name = user.name.filter(|n| !n.trim().is_empty());
    let own_name = if learned.name.is_empty() { pid.to_string() } else { learned.name };
    Badge {
        emblem: custom_emblem.or(learned.emblem).unwrap_or(Emblem::Device),
        name: custom_name.clone().unwrap_or_else(|| own_name.clone()),
        own_name,
        custom_emblem: custom_emblem.is_some(),
        custom_name: custom_name.is_some(),
    }
}

/// The device's display name.
#[must_use]
pub fn name(pid: CanonicalPid) -> String {
    of(pid).name
}

/// Set (or, with `None`, return to automatic) a device's emblem and name, and persist them.
///
/// # Errors
/// The badge file could not be written; the in-memory badge is unchanged.
pub fn set(pid: CanonicalPid, emblem: Option<Emblem>, name: Option<&str>) -> std::io::Result<()> {
    with_book(|b| {
        let entry = Entry {
            emblem: emblem.map(|g| g.key().to_string()),
            name: name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string),
        };
        let mut user = b.user.clone();
        if entry == Entry::default() {
            user.remove(&pid.get());
        } else {
            user.insert(pid.get(), entry);
        }
        let file = File { device: user.iter().map(|(k, v)| (format!("{k:04x}"), v.clone())).collect() };
        let text = toml::to_string_pretty(&file).map_err(std::io::Error::other)?;
        crate::salvage::atomic_write(&path(), text.as_bytes())?;
        b.user = user;
        GENERATION.fetch_add(1, Ordering::AcqRel);
        Ok(())
    })
}

/// Record what a source knows about a device: an emblem it is sure of (a gamepad API saw sticks)
/// and the name it reports. A stronger emblem replaces a weaker one; the first name stays.
pub fn learn(pid: CanonicalPid, emblem: Option<Emblem>, name: &str) {
    let changed = with_book(|b| {
        let slot = b.learned.entry(pid.get()).or_default();
        let before = (slot.emblem, slot.name.clone());
        if let Some(g) = emblem {
            if slot.emblem.is_none_or(|old| g.rank() > old.rank()) {
                slot.emblem = Some(g);
            }
        }
        if slot.name.is_empty() {
            slot.name = name.trim().to_string();
        }
        before != (slot.emblem, slot.name.clone())
    });
    if changed {
        GENERATION.fetch_add(1, Ordering::AcqRel);
    }
}

/// Learn every device in an enumeration.
///
/// A composite device's first interface is its primary function (a mouse's side keys are a
/// keyboard collection on a later interface, a keyboard's mouse keys a mouse collection on one),
/// so the lowest-numbered interface with a telling collection decides; strength only breaks ties
/// within it. A product word, when one matches, says it outright.
pub fn learn_from(infos: &[crate::transport::HidDeviceInfo]) {
    let mut by_pid: BTreeMap<u16, Vec<&crate::transport::HidDeviceInfo>> = BTreeMap::new();
    for i in infos {
        by_pid.entry(CanonicalPid::of(i.pid).get()).or_default().push(i);
    }
    for (pid, list) in by_pid {
        let product = list.iter().map(|i| i.product.trim()).find(|p| !p.is_empty()).unwrap_or("");
        let emblem = emblem_from_name(product).or_else(|| {
            list.iter()
                .filter_map(|i| Some((interface(&i.path.as_os_str().to_string_lossy()), emblem_from_usage(i.usage_page, i.usage)?)))
                .min_by_key(|(iface, e)| (*iface, std::cmp::Reverse(e.rank())))
                .map(|(_, e)| e)
        });
        learn(CanonicalPid::of(pid), emblem, product);
    }
    with_book(|b| b.last_scan = Some(Instant::now()));
}

/// The USB interface number a HID path names (`&mi_02`), 0 when it names none (a single-function
/// device, or a platform path).
fn interface(path: &str) -> u8 {
    let p = path.to_ascii_lowercase();
    p.split("&mi_").nth(1).and_then(|s| u8::from_str_radix(s.get(..2)?, 16).ok()).unwrap_or(0)
}

fn scan_in_background() {
    if cfg!(test) || SCANNING.swap(true, Ordering::AcqRel) {
        return;
    }
    with_book(|b| b.last_scan = Some(Instant::now()));
    let spawned = crate::worker::spawn_detached("neuron-badges", || {
        if let Ok(infos) = crate::transport::enumerate() {
            learn_from(&infos);
        }
        SCANNING.store(false, Ordering::Release);
    });
    if !spawned {
        SCANNING.store(false, Ordering::Release);
    }
}

/// A top-level HID collection's emblem (HID Usage Tables: Generic Desktop 0x01, Consumer 0x0C).
fn emblem_from_usage(page: u16, usage: u16) -> Option<Emblem> {
    match (page, usage) {
        (0x01, 0x05) => Some(Emblem::Pad),
        (0x01, 0x04) => Some(Emblem::Stick),
        (0x01, 0x01 | 0x02) => Some(Emblem::Mouse),
        (0x01, 0x06 | 0x07) => Some(Emblem::Keyboard),
        (0x0C, 0x01) => Some(Emblem::Dial),
        _ => None,
    }
}

/// Product words that say what a device is where its collections can't (`hints/emblems.toml`).
fn emblem_from_name(product: &str) -> Option<Emblem> {
    #[derive(Deserialize)]
    struct Word {
        #[serde(rename = "match")]
        word: String,
        emblem: String,
    }
    #[derive(Deserialize)]
    struct Hints {
        #[serde(default)]
        word: Vec<Word>,
    }
    static WORDS: std::sync::OnceLock<Vec<(String, Emblem)>> = std::sync::OnceLock::new();
    let words = WORDS.get_or_init(|| {
        let user = std::fs::read_to_string(crate::runroot::run_root().join("emblems.toml")).unwrap_or_default();
        [user.as_str(), include_str!("../hints/emblems.toml")]
            .into_iter()
            .filter_map(|src| toml::from_str::<Hints>(src).ok())
            .flat_map(|h| h.word)
            .filter_map(|w| Some((w.word.to_ascii_lowercase(), Emblem::parse(&w.emblem)?)))
            .collect()
    });
    let lower = product.to_ascii_lowercase();
    words.iter().find(|(w, _)| !w.is_empty() && lower.contains(w.as_str())).map(|(_, e)| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u16, page: u16, usage: u16, product: &str) -> crate::transport::HidDeviceInfo {
        crate::transport::HidDeviceInfo {
            vid: 0x1234,
            pid,
            usage_page: page,
            usage,
            feature_len: 0,
            input_len: 0,
            output_len: 0,
            path: crate::transport::DevicePath::from_str_for_tests(&format!("badge-test-{pid:04x}-{page:x}-{usage:x}")),
            product: product.into(),
        }
    }

    #[test]
    fn collections_and_names_pick_the_emblem_and_the_strongest_claim_wins() {
        learn_from(&[
            info(0xB001, 0x01, 0x06, "Acme Thing"),
            info(0xB001, 0x01, 0x05, "Acme Thing"),
            info(0xB002, 0x0C, 0x01, "Razer Kraken V3"),
            info(0xB003, 0x01, 0x06, "Razer Tartarus Pro"),
        ]);
        let pad = of(CanonicalPid::of(0xB001));
        assert_eq!((pad.emblem, pad.name.as_str()), (Emblem::Pad, "Acme Thing"));
        assert_eq!(of(CanonicalPid::of(0xB002)).emblem, Emblem::Headset);
        assert_eq!(of(CanonicalPid::of(0xB003)).emblem, Emblem::Keypad);
    }

    #[test]
    fn the_first_interface_decides_what_a_composite_device_is() {
        let with = |pid: u16, mi: u8, usage: u16, product: &str| {
            let mut i = info(pid, 0x01, usage, product);
            i.path = crate::transport::DevicePath::from_str_for_tests(&format!(r"\?\HID#VID_1532&PID_{pid:04X}&MI_{mi:02X}#x"));
            i
        };
        // A keyboard whose macro interface also exposes a mouse collection, and a mouse whose
        // side keys are a keyboard collection.
        learn_from(&[with(0xB021, 2, 0x02, "Acme Board"), with(0xB021, 0, 0x06, "Acme Board")]);
        learn_from(&[with(0xB0A7, 1, 0x06, "Acme Rodent"), with(0xB0A7, 0, 0x02, "Acme Rodent")]);
        assert_eq!(of(CanonicalPid::of(0xB021)).emblem, Emblem::Keyboard);
        assert_eq!(of(CanonicalPid::of(0xB0A7)).emblem, Emblem::Mouse);
    }

    #[test]
    fn an_unknown_device_is_its_pid_and_a_generic_emblem() {
        let b = of(CanonicalPid::of(0xB0FF));
        assert_eq!((b.emblem, b.name.as_str()), (Emblem::Device, "b0ff"));
        assert!(!b.custom_emblem && !b.custom_name);
    }

    #[test]
    fn a_users_badge_persists_overrides_and_clears() {
        let _env = crate::runroot::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = std::env::temp_dir().join(format!("neuron_badge_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        {
            let _pin = crate::runroot::RunDirPin::to(&tmp);
            let pid = CanonicalPid::of(0xB010);
            learn(pid, Some(Emblem::Pad), "Acme Pad");
            let before = generation();
            set(pid, Some(Emblem::Stick), Some("  couch  ")).unwrap();
            assert!(generation() > before);
            let b = of(pid);
            assert_eq!((b.emblem, b.name.as_str(), b.own_name.as_str()), (Emblem::Stick, "couch", "Acme Pad"));
            assert!(std::fs::read_to_string(tmp.join("badges.toml")).unwrap().contains("b010"));

            // Survives a reload from disk.
            with_book(|b| b.loaded = None);
            assert_eq!(of(pid).name, "couch");

            set(pid, None, None).unwrap();
            let b = of(pid);
            assert_eq!((b.emblem, b.name.as_str()), (Emblem::Pad, "Acme Pad"));
            assert!(!b.custom_emblem && !b.custom_name);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn emblem_keys_round_trip() {
        for g in Emblem::ALL {
            assert_eq!(Emblem::parse(g.key()), Some(g));
        }
    }
}
