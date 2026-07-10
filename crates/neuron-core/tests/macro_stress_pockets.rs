//! NON-DESTRUCTIVE stress tests for the POCKET subsystem (`neuron::pocket`) — the macro spine's
//! "portable clipboard" register. The 21 stress cases below cover the gaps the audit flagged: the
//! arm/disarm gate, the full `activate()` move table (stash / restore / swap / nothing), durable vs
//! ephemeral disk lifecycle, the generation counter, slot identity/sharing, the public read APIs
//! (`views`/`view_of`/`sigil_of`/`sigil_svg_of`), uncarryable refusal, malformed-disk recovery,
//! multi-format + large + many-format fidelity, concurrent slot access, and FNV-1a slot hashing.
//!
//! STRICTLY NON-DESTRUCTIVE — the real OS clipboard is NEVER touched. `activate()` normally reads
//! and writes the OS clipboard; here we install an IN-MEMORY clipboard via the `pocket::testclip`
//! seam (off in production), so every move runs against a fake cell. Durable state is isolated to a
//! private temp cwd (`runtime/pockets/` is cwd-relative) and cleaned up. The process arm gate is
//! restored on exit.
//!
//! ONE monolithic test on purpose: the pocket store, the generation counter, the fake clipboard, and
//! the process cwd are all PROCESS-GLOBAL, so the phases run serially (no intra-process races), the
//! way the project's other sidecar/e2e tests are structured.

use neuron::pocket::{
    self, activate, generation, sigil_of, sigil_svg_of, view_of, views, ClipFormat, Pocket,
};
use neuron::pocket::testclip;
use neuron::safety;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DISK_DIR: &str = "runtime/pockets";

/// UTF-16LE bytes (NUL-terminated) — a CF_UNICODETEXT payload, like the OS clipboard holds.
fn utf16le(s: &str) -> Vec<u8> {
    let mut b: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    b.extend_from_slice(&[0, 0]);
    b
}

/// The text a pocket previews (its CF_UNICODETEXT first line), or None.
fn ptext(p: &Pocket) -> Option<String> {
    p.view().text
}

/// The `.pocket` files currently on disk (cwd-relative, isolated to the temp dir).
fn pocket_files() -> Vec<PathBuf> {
    std::fs::read_dir(DISK_DIR)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pocket"))
                .collect()
        })
        .unwrap_or_default()
}

/// The set of `.pocket` file NAMES currently on disk. Durable writes happen on a detached worker
/// thread, so to stay robust against a straggler write from an EARLIER phase we track a phase's own
/// file by set-difference from a baseline rather than asserting global emptiness.
fn file_names() -> HashSet<String> {
    pocket_files()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// Poll `pred` until true or `dur` elapses (durable persistence is on a background worker thread).
fn wait_until(pred: impl Fn() -> bool, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The durable flag the store reports for a slot via the public `views()` API.
fn durable_of(slot: &str) -> Option<bool> {
    views().into_iter().find(|(s, _, _)| s == slot).map(|(_, d, _)| d)
}

#[test]
fn pocket_stress() {
    // Isolate everything cwd-relative (durable pockets, the slot store's disk load) into a private
    // temp dir so the user's real `runtime/pockets/` is never read or written.
    let tmp = std::env::temp_dir().join(format!("neuron_pocket_stress_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    // Remember the real arm state and force a known one; restored at the end.
    let prev_armed = safety::input_armed();

    // Route every clipboard read/write to the in-memory seam — the OS clipboard is now untouchable.
    testclip::install_empty();
    testclip::reset_store();
    let _ = std::fs::remove_dir_all(DISK_DIR);

    // Run the phases; capture a panic so we always restore cwd/arm/seam before failing.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_phases));

    // ── teardown (always) ────────────────────────────────────────────────────────────────────────
    testclip::uninstall();
    safety::set_input_armed(prev_armed);
    std::env::set_current_dir(&prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_phases() {
    // ── 1. disarmed_stash_noop ─────────────────────────────────────────────────────────────────
    // activate() while disarmed must change NOTHING and report [disarmed].
    {
        safety::set_input_armed(false);
        testclip::reset_store();
        testclip::install_text("clipdata");
        let g0 = generation();
        let r = activate("dis", false);
        assert!(r.contains("[disarmed]"), "disarmed activate must report it: {r}");
        assert_eq!(generation(), g0, "disarmed move must not bump the generation");
        assert!(view_of("dis").is_empty(), "disarmed move must not fill the pocket");
        assert_eq!(
            testclip::current().as_ref().and_then(ptext).as_deref(),
            Some("clipdata"),
            "disarmed move must leave the clipboard exactly as found"
        );
        eprintln!("[pocket 1] disarmed_stash_noop OK");
    }

    // ── 2. stash_clipboard_to_pocket ───────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        testclip::install_text("hello");
        let g0 = generation();
        let r = activate("s", false);
        assert!(r.contains("stashed"), "clipboard->empty pocket is a stash: {r}");
        assert_eq!(generation(), g0 + 1, "a real move bumps the generation");
        assert_eq!(view_of("s").text.as_deref(), Some("hello"), "pocket now holds the content");
        assert!(testclip::current().is_none(), "stash clears the clipboard");
        eprintln!("[pocket 2] stash_clipboard_to_pocket OK");
    }

    // ── 3. restore_pocket_to_clipboard (continues from #2: pocket 's' holds 'hello') ───────────
    {
        safety::set_input_armed(true);
        let g1 = generation();
        let r = activate("s", false);
        assert!(
            r.contains("clipboard") || r.contains('\u{2192}'),
            "empty clipboard + full pocket is a restore: {r}"
        );
        assert_eq!(generation(), g1 + 1);
        assert_eq!(
            testclip::current().as_ref().and_then(ptext).as_deref(),
            Some("hello"),
            "restore puts the content back on the clipboard"
        );
        assert!(view_of("s").is_empty(), "restore empties the pocket");
        eprintln!("[pocket 3] restore_pocket_to_clipboard OK");
    }

    // ── 4. swap_exchange_both_full ─────────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        testclip::install_text("P");
        activate("sw", false); // stash P: pocket=P, clipboard empty
        testclip::install_text("C"); // clipboard=C, pocket=P -> both full
        let g0 = generation();
        let r = activate("sw", false);
        assert!(
            r.contains("swapped") || r.contains('\u{21c4}'),
            "both-full is a swap: {r}"
        );
        assert_eq!(generation(), g0 + 1);
        assert_eq!(
            testclip::current().as_ref().and_then(ptext).as_deref(),
            Some("P"),
            "swap: the pocket's old content is now on the clipboard"
        );
        assert_eq!(view_of("sw").text.as_deref(), Some("C"), "swap: clipboard's content now pocketed");
        eprintln!("[pocket 4] swap_exchange_both_full OK");
    }

    // ── 5. noop_both_empty ─────────────────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        testclip::install_empty();
        let g0 = generation();
        let r = activate("e", false);
        assert!(r.contains("nothing to move"), "both empty is a no-op: {r}");
        assert_eq!(generation(), g0, "a no-op must not bump the generation");
        assert!(view_of("e").is_empty());
        eprintln!("[pocket 5] noop_both_empty OK");
    }

    // ── 6. persist_flag_enables_durability ─────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        testclip::install_text("durable!");
        let r = activate("dur", true);
        assert!(r.contains("stashed"), "{r}");
        assert_eq!(durable_of("dur"), Some(true), "persist=true marks the slot durable in-memory");
        assert!(
            wait_until(|| !pocket_files().is_empty(), Duration::from_secs(3)),
            "a durable pocket must write a file under {DISK_DIR}/"
        );
        eprintln!("[pocket 6] persist_flag_enables_durability OK");
    }

    // ── 7. ephemeral_no_disk_write ─────────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        testclip::install_text("ephemeral");
        let r = activate("eph", false);
        assert!(r.contains("stashed"), "{r}");
        assert_eq!(durable_of("eph"), Some(false), "persist=false slots are ephemeral");
        std::thread::sleep(Duration::from_millis(150)); // give any (wrongful) write a chance
        assert!(pocket_files().is_empty(), "an ephemeral pocket must never touch disk");
        eprintln!("[pocket 7] ephemeral_no_disk_write OK");
    }

    // ── 8. generation_counter_increments ───────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let g0 = generation();
        testclip::install_text("a");
        activate("gc", false); // stash
        let g1 = generation();
        assert_eq!(g1, g0 + 1, "stash increments");
        activate("gc", false); // restore
        let g2 = generation();
        assert_eq!(g2, g1 + 1, "restore increments");
        testclip::install_text("b");
        activate("gc", false); // stash again
        assert_eq!(generation(), g2 + 1, "every successful move increments exactly once");
        eprintln!("[pocket 8] generation_counter_increments OK");
    }

    // ── 9. slot_identity_and_sharing ───────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        // same slot name shares one register (a stash then a restore on it round-trips the content).
        testclip::reset_store();
        testclip::install_text("A1");
        activate("slotA", false); // slotA <- A1
        activate("slotA", false); // restore on the SAME register -> empties it
        assert!(view_of("slotA").is_empty(), "two activates on one slot share its register");
        assert_eq!(testclip::current().as_ref().and_then(ptext).as_deref(), Some("A1"));

        // different slots are independent.
        testclip::reset_store();
        testclip::install_text("AA");
        activate("slotA", false);
        testclip::install_text("BB");
        activate("slotB", false);
        assert_eq!(view_of("slotA").text.as_deref(), Some("AA"));
        assert_eq!(view_of("slotB").text.as_deref(), Some("BB"), "slots don't bleed into each other");

        // the empty name is the single default pocket.
        testclip::install_text("DEF");
        activate("", false);
        assert_eq!(view_of("").text.as_deref(), Some("DEF"), "empty slot name = default pocket");
        eprintln!("[pocket 9] slot_identity_and_sharing OK");
    }

    // ── 10. views_api_public_query ─────────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        testclip::install_text("aa");
        activate("a", false);
        testclip::install_text("bb");
        activate("b", true); // durable
        testclip::install_text("cc");
        activate("c", false);
        // settle the durable "b" write so its detached worker can't recreate a file in a later phase.
        assert!(wait_until(|| !pocket_files().is_empty(), Duration::from_secs(3)));
        let v = views();
        let names: Vec<&str> = v.iter().map(|(s, _, _)| s.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"], "views() is sorted by slot name");
        for (slot, dur, view) in &v {
            assert!(!view.summary.is_empty(), "slot {slot} view summary must be non-empty");
            let expected = slot == "b";
            assert_eq!(*dur, expected, "slot {slot} durable flag");
        }
        eprintln!("[pocket 10] views_api_public_query OK");
    }

    // ── 11. view_of_single_slot_query ──────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        testclip::install_text("xx");
        activate("ex", false);
        assert_eq!(view_of("ex").text.as_deref(), Some("xx"));
        assert!(view_of("does_not_exist").is_empty(), "absent slot view is empty");
        eprintln!("[pocket 11] view_of_single_slot_query OK");
    }

    // ── 12. sigil_deterministic_per_slot ───────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        testclip::install_text("sigil-content");
        activate("sg", false);
        let s1 = sigil_of("sg", 256);
        let s2 = sigil_of("sg", 256);
        assert!(!s1.path.is_empty(), "a non-empty pocket has a sigil");
        assert_eq!(s1.path, s2.path, "same content -> identical sigil (deterministic)");
        // change the slot's content (swap a different payload in) -> the sigil must change.
        testclip::install_text("totally-different");
        activate("sg", false); // swap: slot now holds the new payload
        let s3 = sigil_of("sg", 256);
        assert_ne!(s1.path, s3.path, "different content -> different sigil");
        assert!(sigil_of("no_such_slot", 256).path.is_empty(), "absent slot -> empty sigil path");
        let svg = sigil_svg_of("sg", 64.0);
        assert!(svg.contains("svg"), "a present pocket renders an SVG sigil");
        assert!(sigil_svg_of("no_such_slot", 64.0).is_empty(), "absent slot -> empty SVG");
        eprintln!("[pocket 12] sigil_deterministic_per_slot OK");
    }

    // ── 13. empty_durable_pocket_file_deletion ─────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        let baseline = file_names(); // tolerate any straggler write from an earlier phase
        testclip::install_text("keepme");
        activate("del", true); // stash durable -> del's OWN file appears
        assert!(
            wait_until(|| file_names().difference(&baseline).next().is_some(), Duration::from_secs(3)),
            "durable stash must create del's file"
        );
        let del_file = file_names()
            .difference(&baseline)
            .next()
            .expect("del's file")
            .clone();
        activate("del", true); // restore -> pocket emptied -> del's durable file is deleted
        assert!(
            wait_until(|| !file_names().contains(&del_file), Duration::from_secs(3)),
            "emptying a durable pocket must remove its file ({del_file})"
        );
        eprintln!("[pocket 13] empty_durable_pocket_file_deletion OK");
    }

    // ── 14. multi_format_fidelity_in_activate (text + image + files in one pocket) ──────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let p = Pocket {
            formats: vec![
                ClipFormat { id: 13, bytes: utf16le("multi") },       // CF_UNICODETEXT
                ClipFormat { id: 8, bytes: vec![40, 0, 0, 0, 9, 9, 9, 9] }, // CF_DIB-ish bytes
                ClipFormat { id: 15, bytes: vec![1, 2, 3, 4, 5] },    // CF_HDROP-ish bytes
            ],
        };
        testclip::install_pocket(p.clone());
        activate("mf", false); // stash
        activate("mf", false); // restore -> all formats land back on the clipboard
        let back = testclip::current().expect("multi-format pocket restored to clipboard");
        assert_eq!(back, p, "all three formats survive a stash+restore byte-for-byte");
        eprintln!("[pocket 14] multi_format_fidelity_in_activate OK");
    }

    // ── 15. parse_disk_malformed_data_graceful ─────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        // write a VALID durable pocket first.
        testclip::install_text("valid");
        activate("good", true);
        assert!(wait_until(|| !pocket_files().is_empty(), Duration::from_secs(3)));
        // now litter the dir with malformed files: bad magic, truncated, empty.
        std::fs::create_dir_all(DISK_DIR).unwrap();
        std::fs::write(format!("{DISK_DIR}/zz_badmagic.pocket"), b"not a pocket file").unwrap();
        std::fs::write(format!("{DISK_DIR}/zz_truncated.pocket"), b"NPKT").unwrap();
        std::fs::write(format!("{DISK_DIR}/zz_empty.pocket"), b"").unwrap();
        // reload: must not panic; the malformed files are skipped, the valid one survives.
        testclip::reload_disk();
        assert_eq!(
            view_of("good").text.as_deref(),
            Some("valid"),
            "load_all skips malformed files and keeps the valid pocket"
        );
        eprintln!("[pocket 15] parse_disk_malformed_data_graceful OK");
    }

    // ── 16. uncarryable_clipboard_content_refused ──────────────────────────────────────────────
    {
        safety::set_input_armed(true); // even armed, an uncarryable clipboard is refused
        testclip::reset_store();
        testclip::install_uncarryable();
        let g0 = generation();
        let r = activate("unc", false);
        assert!(
            r.contains("can't carry") && r.contains("left it"),
            "uncarryable content must be refused, not clobbered: {r}"
        );
        assert_eq!(generation(), g0, "a refusal is not a move");
        assert!(view_of("unc").is_empty(), "the pocket is untouched on refusal");
        eprintln!("[pocket 16] uncarryable_clipboard_content_refused OK");
    }

    // ── 17. large_payload_stress (5 MB) ────────────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let big = vec![0xABu8; 5 * 1024 * 1024];
        let p = Pocket { formats: vec![ClipFormat { id: 0xC100, bytes: big }] };
        testclip::install_pocket(p.clone());
        activate("big", false); // stash
        activate("big", false); // restore
        let back = testclip::current().expect("5MB pocket restored");
        assert_eq!(back.formats.len(), 1);
        assert_eq!(back.formats[0].bytes.len(), 5 * 1024 * 1024, "no truncation");
        assert_eq!(back, p, "5MB payload round-trips byte-for-byte");
        eprintln!("[pocket 17] large_payload_stress OK");
    }

    // ── 18. many_formats_fidelity (10 formats, through disk) ────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        let mut formats = vec![
            ClipFormat { id: 13, bytes: utf16le("text") },
            ClipFormat { id: 8, bytes: vec![1u8; 32] },
            ClipFormat { id: 15, bytes: vec![2u8; 24] },
        ];
        for i in 0..7u32 {
            formats.push(ClipFormat { id: 0xC000 + i, bytes: vec![i as u8; 10 + i as usize] });
        }
        let p = Pocket { formats };
        assert_eq!(p.formats.len(), 10);
        testclip::install_pocket(p.clone());
        activate("many", true); // stash durable -> all 10 formats written to disk
        assert!(wait_until(|| !pocket_files().is_empty(), Duration::from_secs(3)));
        // reload from disk, then restore to read the loaded bytes back verbatim.
        testclip::reload_disk();
        testclip::install_empty();
        activate("many", false); // restore the disk-loaded pocket onto the clipboard
        let back = testclip::current().expect("10-format pocket restored from disk");
        assert_eq!(back, p, "all 10 formats survive the disk round-trip in order");
        eprintln!("[pocket 18] many_formats_fidelity OK");
    }

    // ── 19. concurrent_slot_access_isolation ───────────────────────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        // seed 5 slots with distinct content sequentially (deterministic).
        for i in 0..5 {
            testclip::install_text(&format!("slot-{i}-payload"));
            activate(&format!("c{i}"), false);
        }
        // concurrent READS: each thread hammers its OWN slot; another slot's content must never leak
        // in, and the global read locks (view_of / sigil_of / views) must not panic or deadlock.
        let mut handles = Vec::new();
        for i in 0..5 {
            handles.push(std::thread::spawn(move || {
                let slot = format!("c{i}");
                let want = format!("slot-{i}-payload");
                for _ in 0..200 {
                    assert_eq!(view_of(&slot).text.as_deref(), Some(want.as_str()), "slot isolation");
                    let _ = sigil_of(&slot, 64);
                    let _ = views();
                }
            }));
        }
        for h in handles {
            h.join().expect("a concurrent reader thread panicked");
        }
        // concurrent WRITES: each thread activates its OWN slot repeatedly. The in-memory clipboard
        // is a single shared cell (exactly like the real OS clipboard), so the GUARANTEE under test
        // is "no corruption / no panic / no deadlock", which we prove by completing + a still-queryable
        // store, not by predicting a per-slot end state that the shared clipboard makes racy.
        let g_before = generation();
        let mut handles = Vec::new();
        for i in 0..5 {
            handles.push(std::thread::spawn(move || {
                let slot = format!("w{i}");
                for _ in 0..20 {
                    testclip::install_text("shared");
                    let _ = activate(&slot, false);
                }
            }));
        }
        for h in handles {
            h.join().expect("a concurrent writer thread panicked");
        }
        let _ = views(); // store still consistent + queryable after the storm
        assert!(generation() >= g_before, "concurrent moves only ever advance the generation");
        eprintln!("[pocket 19] concurrent_slot_access_isolation OK");
    }

    // ── 20. (audit case fnv1a) hash slot-name collision zero ───────────────────────────────────
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        let _ = std::fs::remove_dir_all(DISK_DIR);
        let baseline = file_names(); // tolerate any straggler write from an earlier phase
        for name in ["", "a", "b"] {
            testclip::install_text(&format!("payload-{name}"));
            activate(name, true); // durable -> one file per distinct slot hash
        }
        // the THREE slots must produce THREE new (distinct) files relative to the baseline.
        assert!(
            wait_until(
                || file_names().difference(&baseline).count() == 3,
                Duration::from_secs(3)
            ),
            "three durable slots must produce three files (new files: {:?})",
            file_names().difference(&baseline).cloned().collect::<Vec<_>>()
        );
        let new: HashSet<_> = file_names().difference(&baseline).cloned().collect();
        assert_eq!(new.len(), 3, "empty/'a'/'b' must hash to 3 DISTINCT files (no collision)");
        eprintln!("[pocket 20] fnv1a_hash_slot_name_collision_zero OK");
    }

    // ── 21. refused_clipboard_write_moves_nothing (review finding, 2026-07-09) ─────────────────
    // The ordinary FALLIBLE path, not the panic path: a foreign process grabs the clipboard
    // between activate()'s read and its write (`set_clipboard` returns false). The exchange must
    // move NOTHING — the pocketed payload stays in the slot (not silently consumed), the
    // clipboard is untouched, the generation doesn't bump, and the status says so honestly.
    {
        safety::set_input_armed(true);
        testclip::reset_store();
        // seed the slot through a WORKING clipboard first: pocket <- "precious".
        testclip::install_text("precious");
        activate("ref", false);
        assert_eq!(view_of("ref").text.as_deref(), Some("precious"));
        // now the clipboard turns hostile: reads show "live-stuff", every write is refused.
        testclip::install_refusing_text("live-stuff");
        let g0 = generation();
        let r = activate("ref", false);
        assert!(
            r.contains("nothing moved"),
            "a refused clipboard write must report an honest no-move: {r}"
        );
        assert_eq!(
            view_of("ref").text.as_deref(),
            Some("precious"),
            "the pocketed payload must SURVIVE a refused exchange — never silently consumed"
        );
        assert_eq!(generation(), g0, "a refused exchange is not a move");
        eprintln!("[pocket 21] refused_clipboard_write_moves_nothing OK");
    }

    // a final clean state for the public API after the storm.
    let _ = pocket::generation();
    eprintln!("[pocket] all 21 phases passed");
}
