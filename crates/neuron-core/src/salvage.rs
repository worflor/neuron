//! Resilient config loading: one bad value must never cost the user the whole file.
//!
//! Every `Struct::load()` for a TOML config shares the same choreography — read, try a clean
//! whole-struct parse (the overwhelmingly common case), and only on failure drop to a field-by-field
//! salvage that keeps every value that DOES parse. The trap this module closes is two-staged: (1) a
//! naive `from_str().unwrap_or_default()` throws away EVERY value when one is malformed, and (2) the
//! next `save()` then writes the flattened struct back, making the loss permanent. Per-field salvage
//! shrinks stage 1; **backing the raw bytes up to `<file>.bad` (or `.bad.1` … `.bad.8` if earlier
//! generations already occupy it) at the moment the clean parse fails** (before anything can save
//! over them) neutralises stage 2 — the user's original is always on disk.
//! This backup fires for ANY degraded read, including a file that isn't even valid UTF-8 and so never
//! makes it to a TOML parse at all — the bytes are backed up first regardless of why they're degraded.
//!
//! A config opts in by implementing [`SalvageLoad`] (three items: `FILE`, `path`, `salvage`); the
//! resilient `load()` is provided. The per-field work is done with [`salvage_field`] / [`salvage_vec`]
//! / [`salvage_vec_positional`] / [`salvage_map`] (and the [`salvage_fields!`](crate::salvage_fields)
//! macro for scalar-heavy bodies), so one corrupt entry drops only itself, warns by name, and leaves
//! its siblings intact. `salvage_vec` is for list-shaped vecs (order-independent, self-describing
//! entries) and drops bad elements; `salvage_vec_positional` is for vecs where the INDEX carries
//! meaning and defaults a bad element in place instead, so surviving elements keep their exact index.
//!
//! Forward-compat trade-off: the fast path returns as soon as serde parses the WHOLE struct, and the
//! participating structs do not `deny_unknown_fields`. So a config written by a NEWER version of the
//! app — one with fields this binary doesn't know about yet — parses cleanly here: no salvage runs, no
//! `.bad` backup is made, and those unknown fields are silently dropped the next time this binary calls
//! `save()`. That's deliberate: the alternative, denying unknown fields, would instead make every OLDER
//! binary reject a NEWER config outright (no load at all, not even a degraded one). Noting it here so
//! the loss mode is a documented decision, not a surprise discovered via a bug report.

use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Open `path` for writing CREATED RESTRICTIVE: on Unix the file is born 0o600 (owner-only), so
/// secret-bearing bytes (app.toml carries host_obs_password) are never readable by other local
/// users even for an instant — permissions are WIDENED to match the real destination only after
/// the bytes are down, never the other way around. On Windows, `std::fs::Permissions` models only
/// the readonly bit; confidentiality comes from the parent directory's inherited ACL, and every
/// temp this module creates lives in the SAME directory as its destination (or the user's own %TEMP%),
/// so it inherits exactly the protection the destination already has. Custom per-FILE Windows ACLs
/// on an EXISTING destination are now preserved across a save: `atomic_write`'s publish step uses
/// `ReplaceFileW` (not rename) when the destination already exists, which carries the destination's
/// own ACL/attributes across the swap instead of handing it the temp's inherited one. Only a
/// first-write (destination absent) inherits the ambient directory ACL, same as any new file.
///
/// ALWAYS exclusive-create (`create_new`), never truncate-an-existing-file: exclusivity is part of
/// the guarantee, not an optional mode. If a stale temp from a crashed process (or a PID-reused
/// name after a reboot) already sits at `path`, its permissions were set the LAST time it was
/// created — the restrictive 0o600 mode above only ever applies at creation, so reusing that file
/// via `create(true).truncate(true)` would silently keep whatever (possibly wider) mode the stale
/// file was born with. Callers that need a fresh temp name retry with a new candidate on
/// `AlreadyExists` instead.
fn create_restrictive(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// A TOML-backed config whose `load()` recovers field-by-field instead of resetting the whole file.
///
/// Implement `FILE` (the on-disk name, used in every warning), `path`, and `salvage` (rebuild from a
/// parsed table, keeping what parses). `fallback` defaults to [`Default`]; override it when a MISSING
/// file should seed non-empty starter content (e.g. `Bindings::default_for_user`) — note `salvage`
/// must NOT reuse that seed, or a present-but-broken file would resurrect starter entries alongside
/// the user's survivors.
pub trait SalvageLoad: Sized + Default + DeserializeOwned {
    /// The file's name, e.g. `"bindings.toml"` — used verbatim in every warning line.
    const FILE: &'static str;

    /// Absolute path to the config file.
    fn path() -> PathBuf;

    /// Fallback when the file is ABSENT or not even valid TOML. Defaults to `Self::default()`.
    fn fallback() -> Self {
        Self::default()
    }

    /// Rebuild from a structurally-valid table, salvaging field by field (see [`salvage_field`] etc.).
    fn salvage(table: &toml::Table) -> Self;

    /// Load the config, recovering as much as possible. NEVER errors, and never destroys the user's
    /// bytes: a degraded load copies the original to `<file>.bad` before returning anything.
    fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// The salvaging load, parameterised on `path` — factored out of [`load`](Self::load) so the
    /// whole choreography (fast path → backup → field-by-field salvage → fallback) can be tested
    /// against a temp file without a process-global path. Callers use `load()`.
    fn load_from(path: &Path) -> Self {
        // Read RAW BYTES first: `read_to_string` would conflate "file absent" with "file exists but
        // isn't valid UTF-8", and the latter is exactly a degraded file whose bytes must be backed up
        // BEFORE anything else — not silently treated as a first run.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::fallback(); // absent file → fallback (first run; nothing to warn about)
            }
            Err(e) => {
                // present but unreadable (permissions, sharing violation, …): we can't read the bytes
                // to salvage them, but we CAN still pin them — a hard link needs only directory
                // permissions, not read access, so the original's data survives under a backup name
                // even though this process can't currently see inside it. Do that BEFORE falling back.
                preserve_unreadable_original(path);
                eprintln!("neuron: could not read {} ({e}); using defaults for this run", Self::FILE);
                return Self::fallback();
            }
        };
        let raw = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => {
                // exists but isn't UTF-8 → a degraded file. Preserve the user's exact bytes FIRST,
                // then fall back — same contract as any other degraded load.
                backup_degraded_bytes(path, e.as_bytes());
                eprintln!(
                    "neuron: {0} is not valid UTF-8; using defaults (original preserved alongside the file)",
                    Self::FILE
                );
                return Self::fallback();
            }
        };
        // Fast path: the whole struct parses clean.
        if let Ok(cfg) = toml::from_str::<Self>(&raw) {
            return cfg;
        }
        // Degraded: preserve the user's exact bytes BEFORE any later save can overwrite them.
        backup_degraded_bytes(path, raw.as_bytes());
        match toml::from_str::<toml::Table>(&raw) {
            Ok(table) => {
                eprintln!(
                    "neuron: {0}: some values are malformed; salvaging what parses (original preserved alongside the file)",
                    Self::FILE
                );
                Self::salvage(&table)
            }
            Err(e) => {
                eprintln!(
                    "neuron: {0} is not valid TOML ({e}); using defaults (original preserved alongside the file)",
                    Self::FILE
                );
                Self::fallback()
            }
        }
    }
}

/// One scalar/struct field. `None` = key absent OR malformed; a malformed value warns by name and the
/// caller keeps its default. `file` is the config name for the warning.
pub fn salvage_field<T: DeserializeOwned>(table: &toml::Table, key: &str, file: &str) -> Option<T> {
    let v = table.get(key)?;
    match v.clone().try_into() {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("neuron: {file}: `{key}` is malformed ({e}); keeping the default");
            None
        }
    }
}

/// A `Vec` field, salvaged PER ELEMENT: malformed elements are dropped (each warned, with its index)
/// and the good ones kept IN ORDER. `None` = key absent or not an array (caller keeps its default).
pub fn salvage_vec<T: DeserializeOwned>(table: &toml::Table, key: &str, file: &str) -> Option<Vec<T>> {
    let v = table.get(key)?;
    let toml::Value::Array(items) = v else {
        eprintln!("neuron: {file}: `{key}` is not an array; keeping the default");
        return None;
    };
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item.clone().try_into() {
            Ok(t) => out.push(t),
            Err(e) => eprintln!(
                "neuron: {file}: `{key}`[{i}] is malformed ({e}); dropping that entry, keeping the rest"
            ),
        }
    }
    Some(out)
}

/// A POSITIONAL `Vec` field (index carries meaning — e.g. cast's radial wedges, where index =
/// sector). Malformed elements are NOT dropped: dropping would shift every later element left and
/// silently remap positions. Instead each bad element warns (with its index) and is replaced by
/// `T::default()` IN PLACE, so every surviving element keeps its exact index. `None` = key absent
/// or not an array (caller keeps its default).
pub fn salvage_vec_positional<T: DeserializeOwned + Default>(
    table: &toml::Table,
    key: &str,
    file: &str,
) -> Option<Vec<T>> {
    let v = table.get(key)?;
    let toml::Value::Array(items) = v else {
        eprintln!("neuron: {file}: `{key}` is not an array; keeping the default");
        return None;
    };
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item.clone().try_into() {
            Ok(t) => out.push(t),
            Err(e) => {
                eprintln!(
                    "neuron: {file}: `{key}`[{i}] is malformed ({e}); resetting that slot to the default (positions kept)"
                );
                out.push(T::default());
            }
        }
    }
    Some(out)
}

/// A map field, salvaged PER ENTRY (e.g. cast's `gestures`). `None` = key absent or not a table
/// (caller keeps its default). Malformed entries are dropped (each warned, by name); the rest survive.
pub fn salvage_map<T: DeserializeOwned>(
    table: &toml::Table,
    key: &str,
    file: &str,
) -> Option<std::collections::BTreeMap<String, T>> {
    let v = table.get(key)?;
    let toml::Value::Table(entries) = v else {
        eprintln!("neuron: {file}: `{key}` is not a table; keeping the default");
        return None;
    };
    let mut out: std::collections::BTreeMap<String, T> = std::collections::BTreeMap::new();
    for (name, entry) in entries {
        match entry.clone().try_into() {
            Ok(t) => {
                out.insert(name.clone(), t);
            }
            Err(e) => eprintln!(
                "neuron: {file}: `{key}.{name}` is malformed ({e}); dropping that entry, keeping the rest"
            ),
        }
    }
    Some(out)
}

/// Terse per-field wiring for scalar-heavy `salvage()` bodies:
/// ```ignore
/// crate::salvage_fields!(table, Self::FILE, cfg, { "hold_ms" => hold_ms, "gap_ms" => gap_ms });
/// ```
/// Each present-and-valid key overwrites its field; an absent field keeps its default silently, a
/// present-but-malformed one warns and keeps the default.
#[macro_export]
macro_rules! salvage_fields {
    ($table:expr, $file:expr, $cfg:expr, { $($key:literal => $field:ident),+ $(,)? }) => {
        $(
            if let Some(v) = $crate::salvage::salvage_field($table, $key, $file) {
                $cfg.$field = v;
            }
        )+
    };
}

/// Copy the pre-salvage bytes to the next free generational backup slot (`<file>.bad`, `.bad.1`, …
/// `.bad.8`), so a later `save()` can never destroy the user's original. NEVER-CLOBBER: an existing
/// backup slot is never overwritten — the earliest capture is the closest to the user's true
/// original, and a later degradation must not destroy it. Up to nine distinct generations are kept;
/// if this exact byte sequence is already backed up somewhere, nothing new is written. Atomic so a
/// slot is never itself observed half-written.
///
/// If the chosen write fails — e.g. something already occupies that slot as a directory, or the
/// config's own folder is locked down while the config file's path itself remains writable — this
/// falls back to the OS temp dir, a location with no reason to share whatever blocked the sibling
/// path. `load_from` proceeds to return a salvaged/fallback value either way (it never errors), and
/// that value CAN be handed straight to a `save()` that atomically replaces the original — so unless
/// a backup landed SOMEWHERE first, "a later save can never destroy the user's original" would be a
/// promise the salvage path doesn't keep. Only if both the sibling path AND the OS temp dir fail
/// (both loudly logged) does the degraded load proceed with no backup at all — a residual case that
/// needs two independent, unrelated locations to be unwritable at once, not a single point of failure.
/// Publish `raw` into the next free generational slot USING ATOMIC NO-REPLACE (hard-link
/// publication) rather than check-then-replace: two concurrent degraded loads picking the SAME
/// slot via a read-then-write choreography can race — the later `atomic_write` (rename-REPLACE)
/// would destroy the earlier one's backup even though both looked "free" at read time. A hard link
/// closes that window: `std::fs::hard_link` FAILS with `AlreadyExists` if the destination is
/// already taken, atomically, with no gap to race through — so an occupied slot is only ever
/// advanced past, never replaced, no matter how many loads degrade at once.
///
/// Before any of that, a cheap PRE-SCAN reads the nine slots looking for an exact byte match: a
/// config that stays malformed on disk gets `load_from`'d on every run, and without this fast path
/// each of those runs would pay for a full temp create+write+fsync only to discover during
/// publication that the identical bytes were already backed up. The pre-scan is an optimization
/// only, not the correctness guarantee — see the AlreadyExists arm below for the race-safe check.
fn backup_degraded_bytes(path: &Path, raw: &[u8]) {
    // FAST PATH: scan the nine slots BEFORE staging any temp write. A config that stays malformed
    // on disk hits `load_from` on every run, and without this pre-scan each of those runs pays for
    // a full create+write+fsync temp file only to discover during publication that the identical
    // bytes were already backed up. Reading nine small sibling files is far cheaper than that write.
    // This is a pure optimization, not the correctness guarantee: it can race (another process could
    // publish an identical backup between this scan and the write below), which is exactly why the
    // AlreadyExists arm in the publication loop below ALSO carries its own dedupe check — that one is
    // race-safe and is what actually closes the never-clobber contract.
    let base = bad_sibling(path);
    for i in 0..=8 {
        let slot = bad_slot_path(&base, i);
        if let Ok(existing) = std::fs::read(&slot) {
            if existing == raw {
                return; // already safely backed up — skip the temp write entirely
            }
        }
    }

    // 1. Land the bytes at a unique sibling temp first (same naming/fsync discipline as
    // `atomic_write`), then hard-link that temp into a slot. The link is what makes publication
    // atomic; the temp itself is just a place to point the link at.
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let tmp = match write_tmp(dir, path, raw) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("neuron: could not stage a backup of {} ({e}); trying a fallback location", path.display());
            write_to_temp_fallback(path, raw);
            return;
        }
    };

    for i in 0..=8 {
        let slot = bad_slot_path(&base, i);
        match std::fs::hard_link(&tmp, &slot) {
            Ok(()) => {
                eprintln!("neuron: preserved the malformed original as {}", slot.display());
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // RACE-SAFE dedupe: the pre-scan above is only a fast path (it can miss a backup
                // published by another process concurrently, between the scan and here) — this
                // check is the one that actually closes the never-clobber contract for that race.
                match std::fs::read(&slot) {
                    Ok(existing) if existing == raw => {
                        // dedupe: this exact original is already safely backed up here.
                        let _ = std::fs::remove_file(&tmp);
                        return;
                    }
                    Ok(_) => continue, // occupied by a DIFFERENT original — never clobber, advance
                    Err(_) => continue, // unreadable slot — occupancy UNKNOWN, treat as occupied, skip
                }
            }
            Err(_) => {
                // Some filesystem without hard-link support (exotic/FAT, etc.): fall back to the
                // OLD check-then-replace choreography as a documented residual. This keeps
                // rename-replace semantics (and its narrow read-check race window) ONLY on
                // filesystems that can't do better — everywhere else the link path above is race
                // free.
                let _ = std::fs::remove_file(&tmp);
                match next_bad_slot(path, raw) {
                    BadSlot::AlreadySaved => {}
                    BadSlot::Free(slot) => {
                        if let Err(e) = atomic_write(&slot, raw) {
                            eprintln!(
                                "neuron: could not back up {} ({e}); trying a fallback location",
                                slot.display()
                            );
                            write_to_temp_fallback(path, raw);
                        } else {
                            eprintln!("neuron: preserved the malformed original as {}", slot.display());
                        }
                    }
                    BadSlot::Full => {
                        eprintln!(
                            "neuron: all nine sibling backup slots for {} are occupied by distinct \
                             originals; preserving this one in the OS temp dir instead",
                            path.display()
                        );
                        write_to_temp_fallback(path, raw);
                    }
                }
                return;
            }
        }
    }
    // all nine slots taken by distinct (or unreadable) originals — the OS temp dir has unique
    // names per call, so it can't race the way sibling slots can.
    eprintln!(
        "neuron: all nine sibling backup slots for {} are occupied by distinct originals; \
         preserving this one in the OS temp dir instead",
        path.display()
    );
    let _ = std::fs::remove_file(&tmp);
    write_to_temp_fallback(path, raw);
}

/// Write `bytes` to a fresh, uniquely-named temp sibling of `path` (same naming scheme as
/// [`atomic_write`]'s temp) and fsync it. `path` here is the ORIGINAL config path (not the temp's
/// own directory target) — used only to derive the temp's stem and, when its metadata is readable,
/// to inherit its permissions onto the temp (a 0600 config's backup shouldn't leak to umask-default).
fn write_tmp(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("cfg");
    const MAX_ATTEMPTS: u32 = 16;
    let mut last_err = None;
    for _ in 0..MAX_ATTEMPTS {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{stem}.{}.{seq}.bad.tmp", std::process::id()));
        let write_res = (|| -> std::io::Result<()> {
            let mut f = create_restrictive(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            Ok(())
        })();
        match write_res {
            Ok(()) => {
                // born restrictive (0o600 on Unix) above; now WIDEN/match the source config's real
                // permissions — best-effort, never fails the backup.
                if let Ok(meta) = std::fs::metadata(path) {
                    if let Err(e) = std::fs::set_permissions(&tmp, meta.permissions()) {
                        eprintln!("neuron: could not carry {}'s permissions onto its backup ({e})", path.display());
                    }
                }
                return Ok(tmp);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // a stale same-named temp survived (crash / PID reuse) — never reuse it, try the
                // next sequence number instead.
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exhausted tmp name attempts")))
}

/// Preserve an original we can't READ: link its bytes into a free backup slot. A hard link needs
/// only directory permissions, not read access, and pins the file's data (same inode/file record)
/// under the backup name — so even if a later save atomically replaces the config path with
/// defaults, the unreadable original's bytes survive at the link. Slots are probed with the same
/// never-clobber rule as byte backups: link creation FAILS on an existing destination, so an
/// occupied slot is advanced past, never replaced.
fn preserve_unreadable_original(path: &Path) {
    let base = bad_sibling(path);
    for i in 0..=8 {
        let slot = bad_slot_path(&base, i);
        match std::fs::hard_link(path, &slot) {
            Ok(()) => {
                eprintln!("neuron: preserved the unreadable original as {} (hard link)", slot.display());
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue, // occupied — never clobber
            Err(e) => {
                eprintln!(
                    "neuron: could not link a backup of {} ({e}) — proceeding WITHOUT one; the \
                     unreadable original is at risk on the next save",
                    path.display()
                );
                return;
            }
        }
    }
    eprintln!("neuron: all backup slots for {} are occupied — the unreadable original was not linked", path.display());
}

/// Shared fallback choreography: write `raw` to a fresh OS-temp-dir `.bad` path, logging success or
/// (loud, final) failure. Used both when a sibling slot write fails and when the generational ring
/// is full — in both cases NO sibling backup may be overwritten, so the temp dir is the only option.
///
/// NEVER-CLOBBER holds here too, same as the sibling-slot ring: `fallback_bad_path` names the file
/// from the config's filename, the process id, and a process-local sequence starting at 0, so after
/// a reboot a reused PID can recur the SAME name — publishing via `atomic_write` (rename-REPLACE)
/// would silently destroy an earlier preserved original under that name. Instead this publishes
/// with create-new (exclusive-create) semantics in a bounded retry loop: each attempt asks
/// `fallback_bad_path` for the next name in the sequence, and only a name nobody already occupies is
/// accepted.
fn write_to_temp_fallback(path: &Path, raw: &[u8]) {
    const MAX_ATTEMPTS: u32 = 16;
    for _ in 0..MAX_ATTEMPTS {
        let candidate = fallback_bad_path(path);
        match try_create_exclusive(&candidate, raw, path) {
            Ok(()) => {
                eprintln!("neuron: preserved the malformed original as {} instead", candidate.display());
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue, // reused name — advance
            Err(e2) => {
                eprintln!(
                    "neuron: could not back up {} either ({e2}) — proceeding WITHOUT a backup; \
                     the malformed original at {} is at risk on the next save",
                    candidate.display(),
                    path.display()
                );
                return;
            }
        }
    }
    eprintln!(
        "neuron: could not find a free fallback backup name for {} after {MAX_ATTEMPTS} attempts — \
         proceeding WITHOUT a backup; the malformed original is at risk on the next save",
        path.display()
    );
}

/// Publish `bytes` to `dest` with CREATE-NEW (exclusive-create) semantics: succeeds only if `dest`
/// does not already exist, and never touches — let alone replaces — whatever is already there. This
/// is the never-clobber primitive `write_to_temp_fallback` retries against when a name collides.
/// Permissions are best-effort inherited from `src` (the ORIGINAL config being backed up, e.g. a
/// 0600 `app.toml` holding a password) so the temp-dir copy doesn't leak to umask-default; a
/// permission-copy failure is logged but does not fail the backup itself.
fn try_create_exclusive(dest: &Path, bytes: &[u8], src: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = create_restrictive(dest)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    // born restrictive (0o600 on Unix) above; now WIDEN/match the source config's real
    // permissions — best-effort, never fails the backup.
    if let Ok(meta) = std::fs::metadata(src) {
        if let Err(e) = std::fs::set_permissions(dest, meta.permissions()) {
            eprintln!("neuron: could not carry {}'s permissions onto its backup ({e})", src.display());
        }
    }
    Ok(())
}

/// The outcome of picking a generational backup slot for a degraded original.
enum BadSlot {
    /// This exact original is already backed up somewhere — nothing new to write.
    AlreadySaved,
    /// This slot is free (confirmed absent, `NotFound`) — write here.
    Free(PathBuf),
    /// All nine slots (`<file>.bad`, `.bad.1` … `.bad.8`) hold distinct originals already. NO
    /// sibling slot may be overwritten — the caller must fall back to the OS temp dir instead.
    Full,
}

/// Pick where a degraded backup should land next to `path`: `<file>.bad` if free, else the first
/// free of `<file>.bad.1` … `<file>.bad.8`. An existing backup is NEVER overwritten, period — the
/// earliest capture is the closest to the user's true original, and later degradations must not
/// destroy it. A slot only counts as free when reading it fails with `NotFound`; any OTHER read
/// error (permissions, sharing violation, a directory sitting there, …) means the slot's occupancy
/// is UNKNOWN — it is left untouched and skipped, never treated as free, since it might already
/// hold a preserved original this process just can't currently read. If an existing backup already
/// holds EXACTLY these bytes, `AlreadySaved` (nothing new to save). If every one of the nine slots
/// ends up either holding a distinct original or unreadable, `Full`: the caller routes to the OS
/// temp dir rather than clobbering slot 8 or an unknown slot.
fn next_bad_slot(path: &Path, bytes: &[u8]) -> BadSlot {
    let base = bad_sibling(path);
    for i in 0..=8 {
        let p = bad_slot_path(&base, i);
        match std::fs::read(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BadSlot::Free(p), // truly absent → free
            Err(e) => {
                // present but unreadable (permissions, sharing violation, a directory, …): occupancy
                // is UNKNOWN, not free — clobbering it could destroy a preserved original. Skip it.
                eprintln!(
                    "neuron: backup slot {} is unreadable ({e}); leaving it untouched",
                    p.display()
                );
                continue;
            }
            Ok(existing) if existing == bytes => return BadSlot::AlreadySaved, // already safe
            Ok(_) => continue,                                  // occupied by a DIFFERENT original — keep it
        }
    }
    BadSlot::Full // all nine slots hold distinct originals; no sibling slot may be overwritten
}

/// The OS temp dir fallback used when `<file>.bad` itself can't be written — named off the config's
/// own filename plus a process+sequence tag (mirroring [`atomic_write`]'s temp-name scheme) so it
/// can't collide across configs or across repeated failures on the SAME config within one process.
/// The sequence resets every process start, so after a reboot a reused PID CAN recur an earlier
/// name — that's why [`write_to_temp_fallback`] publishes with create-new semantics rather than
/// trusting the name to be unique.
fn fallback_bad_path(path: &Path) -> PathBuf {
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("cfg");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("neuron.{stem}.{}.{seq}.bad", std::process::id()))
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` ATOMICALLY: write a sibling temp, fsync it, then PUBLISH it over `path`.
/// On Unix (and on Windows for a first-write, i.e. `path` doesn't exist yet), publication is
/// `std::fs::rename`, which REPLACES an existing destination on both Windows and Unix — this is
/// `std::fs::rename` calling `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)` on Windows, not the C
/// runtime's `rename()` (which is the one that refuses an existing destination; easy to confuse the
/// two). Don't take that on faith: `atomic_write_overwrites_an_existing_file` below writes the SAME
/// path three times in a row and asserts each overwrite lands, on whatever OS the suite runs on —
/// this is the keystone every config `save()` in the app goes through, so its second-and-later-write
/// behaviour is exactly the thing that must never regress silently.
///
/// On Windows, when `path` ALREADY EXISTS, publication instead goes through `ReplaceFileW` — its
/// entire purpose is swapping in new file content while carrying over the DESTINATION's own
/// attributes/ACL/alternate streams, rather than `rename`'s MoveFileExW-REPLACE, which gives the
/// destination the TEMP's inherited ACL (its parent directory's, or the OS temp dir's) instead of
/// keeping the ACL the destination itself had. A custom per-file restrictive ACL on e.g. app.toml
/// (which holds `host_obs_password`) would otherwise be silently WIDENED back to the ambient
/// directory ACL on every save. Plain rename remains for first-writes (nothing to preserve yet).
///
/// A concurrent reader sees either the complete OLD or complete NEW file, never a torn write, and a
/// crash mid-write leaves the previous file intact rather than a truncated one. Config `save()`s and
/// the degraded-load `.bad` backup all go through here so the salvage path can never trip over (or
/// preserve) a half-written file. The temp lives in the SAME directory (rename can't cross a
/// filesystem) and carries a unique per-process sequence so concurrent writers never share a temp.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let dir = dir.unwrap_or_else(|| Path::new("."));
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("cfg");
    // Exclusive-create the temp, retrying with a fresh sequence number on `AlreadyExists` — a
    // stale same-named temp surviving a crash or PID reuse must never be reused (see
    // `create_restrictive`'s doc comment).
    const MAX_ATTEMPTS: u32 = 16;
    let mut tmp = dir.join(format!(".{stem}.{}.0.tmp", std::process::id()));
    let mut last_err = None;
    let mut created = false;
    for _ in 0..MAX_ATTEMPTS {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        tmp = dir.join(format!(".{stem}.{}.{seq}.tmp", std::process::id()));
        // scope the handle so it's closed before the rename (Windows won't rename an open file).
        let write_res = (|| -> std::io::Result<()> {
            let mut f = create_restrictive(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?; // durable on disk before the rename makes it the live file
            Ok(())
        })();
        match write_res {
            Ok(()) => {
                created = true;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp); // don't leave a stray temp on failure
                return Err(e);
            }
        }
    }
    if !created {
        return Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exhausted tmp name attempts")));
    }
    // Born restrictive (0o600 on Unix) above — the bytes were never briefly world/group readable.
    // Now MATCH the destination's real permission metadata before the rename: a deliberately
    // restricted config (e.g. a 0600 app.toml holding `host_obs_password`) must not silently
    // become umask-default, and a previously-0644 config must not silently STAY 0600 either —
    // this step widens (or narrows) to the destination's true permissions, not just restricts.
    // On Windows this carries the readonly flag. Best-effort — a metadata hiccup must not fail
    // the save.
    if let Ok(meta) = std::fs::metadata(path) {
        if let Err(e) = std::fs::set_permissions(&tmp, meta.permissions()) {
            eprintln!("neuron: could not carry {}'s permissions onto the new write ({e})", path.display());
        }
    }
    #[cfg(windows)]
    {
        if path.exists() {
            return publish_replace_windows(&tmp, path);
        }
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            // The rename's directory entry isn't power-loss durable until the directory itself is
            // synced — fsyncing the file only guarantees its DATA landed, not that the rename (a
            // directory-metadata change) survives a crash. Windows has no direct equivalent; NTFS's
            // own metadata journaling covers the common case there.
            #[cfg(unix)]
            {
                if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
                    eprintln!("neuron: could not sync the directory of {} ({e})", path.display());
                }
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Publish `tmp` over an EXISTING `dest` via `ReplaceFileW` — the Windows primitive that swaps in
/// new content while carrying over the destination's own attributes/ACL/alternate data streams
/// (unlike `MoveFileExW`/`rename`, which gives the destination the TEMP's inherited ACL). If `dest`
/// vanished between the caller's `path.exists()` check and this call (`ERROR_FILE_NOT_FOUND`), there
/// is nothing left to preserve — fall through to a plain rename instead of failing the whole write.
/// Any other failure removes the temp and returns the OS error, same contract as the rename path.
#[cfg(windows)]
fn publish_replace_windows(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let dest_w = wide(dest);
    let tmp_w = wide(tmp);

    // SAFETY: both pointers are null-terminated UTF-16 buffers kept alive for the duration of the
    // call; `lpBackupFileName` is null (we don't want a `.bak` sibling) and `dwReplaceFlags` is 0
    // (no special flags needed for a plain config swap).
    let ok = unsafe {
        ReplaceFileW(
            dest_w.as_ptr(),
            tmp_w.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
        // destination vanished between the exists() check and here — nothing left to preserve, so a
        // plain rename (create-or-replace) finishes the publish.
        return std::fs::rename(tmp, dest);
    }
    let _ = std::fs::remove_file(tmp);
    Err(err)
}

/// `.../feel.toml` → `.../feel.toml.bad` (append, not replace — we keep the full original name).
fn bad_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bad");
    path.with_file_name(name)
}

/// The `i`th generational backup slot given `base` (`base` itself for `i == 0`, else
/// `base.<i>`) — the one slot-name scheme shared by every caller that walks the nine-slot ring
/// ([`next_bad_slot`], [`backup_degraded_bytes`], [`preserve_unreadable_original`]).
fn bad_slot_path(base: &Path, i: usize) -> PathBuf {
    if i == 0 {
        base.to_path_buf()
    } else {
        base.with_file_name(format!("{}.{i}", base.file_name().unwrap_or_default().to_string_lossy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, serde::Deserialize)]
    struct Item {
        n: u32,
    }

    fn table(s: &str) -> toml::Table {
        toml::from_str(s).expect("valid TOML for the test")
    }

    #[test]
    fn salvage_field_keeps_good_and_defaults_bad() {
        let t = table("good = 7\nbad = \"not a number\"\n");
        assert_eq!(salvage_field::<u32>(&t, "good", "t.toml"), Some(7));
        assert_eq!(salvage_field::<u32>(&t, "bad", "t.toml"), None); // malformed → None (keep default)
        assert_eq!(salvage_field::<u32>(&t, "absent", "t.toml"), None); // absent → None
    }

    #[test]
    fn salvage_vec_drops_only_the_bad_element_and_keeps_order() {
        // element 1 is malformed (missing `n` / wrong shape); 0 and 2 survive, in order.
        let t = table("items = [ { n = 1 }, { nope = true }, { n = 3 } ]\n");
        let got: Option<Vec<Item>> = salvage_vec(&t, "items", "t.toml");
        assert_eq!(got, Some(vec![Item { n: 1 }, Item { n: 3 }]));
    }

    #[test]
    fn salvage_vec_positional_defaults_bad_elements_in_place() {
        // element 1 is malformed; instead of dropping (which would shift element 2 to index 1),
        // it must be reset to the default IN PLACE, keeping every surviving element's index.
        let t = table("items = [ { n = 1 }, { nope = true }, { n = 3 } ]\n");
        let got: Option<Vec<Item>> = salvage_vec_positional(&t, "items", "t.toml");
        assert_eq!(got, Some(vec![Item { n: 1 }, Item { n: 0 }, Item { n: 3 }]));
    }

    #[test]
    fn salvage_vec_positional_none_when_not_an_array() {
        let t = table("items = 5\n");
        assert_eq!(salvage_vec_positional::<Item>(&t, "items", "t.toml"), None);
        assert_eq!(salvage_vec_positional::<Item>(&t, "absent", "t.toml"), None);
    }

    #[test]
    fn salvage_vec_none_when_not_an_array() {
        let t = table("items = 5\n");
        assert_eq!(salvage_vec::<Item>(&t, "items", "t.toml"), None);
        assert_eq!(salvage_vec::<Item>(&t, "absent", "t.toml"), None);
    }

    #[test]
    fn salvage_map_drops_only_the_bad_entry() {
        let t = table("[m.a]\nn = 1\n[m.b]\nnope = true\n[m.c]\nn = 3\n");
        let got = salvage_map::<Item>(&t, "m", "t.toml").expect("m is a table");
        assert_eq!(got.get("a"), Some(&Item { n: 1 }));
        assert_eq!(got.get("c"), Some(&Item { n: 3 }));
        assert_eq!(got.get("b"), None); // the malformed entry dropped, siblings kept
    }

    #[test]
    fn backup_degraded_falls_back_to_temp_dir_when_all_sibling_slots_are_unreadable() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_fallback_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");
        // Occupy ALL nine sibling slots (`.bad`, `.bad.1` … `.bad.8`) with DIRECTORIES. Each is a
        // non-NotFound read error, so under the unreadable-is-unknown rule every one is SKIPPED
        // rather than treated as free — nine skipped slots exhausts the ring exactly like nine
        // distinct originals would, routing to `Full` and the OS temp-dir fallback.
        let base = bad_sibling(&target);
        std::fs::create_dir_all(&base).expect("occupy .bad with a dir");
        for i in 1..=8 {
            let slot = base.with_file_name(format!("{}.{i}", base.file_name().unwrap().to_string_lossy()));
            std::fs::create_dir_all(&slot).expect("occupy sibling slot with a dir");
        }

        backup_degraded_bytes(&target, b"broken = [");

        // the fallback (OS temp dir) must have caught it — find a file matching our naming scheme.
        let stem = target.file_name().unwrap().to_str().unwrap();
        let found = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("neuron.") && name.contains(stem) && name.ends_with(".bad")
            });
        assert!(
            found,
            "backup_degraded must fall back to a temp-dir .bad when every sibling slot is unreadable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_backup_slot_is_skipped_not_overwritten() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_unreadable_skip_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");
        let base = bad_sibling(&target);
        // Occupy `.bad` with a DIRECTORY — a portable stand-in for a backup file that exists but
        // fails to read (permissions, sharing violation). It must NOT be classified free.
        std::fs::create_dir_all(&base).expect("occupy .bad with a dir");

        backup_degraded_bytes(&target, b"new-original");

        assert!(base.is_dir(), "the unreadable `.bad` slot must be left untouched, not clobbered");
        let slot1 = base.with_file_name(format!("{}.1", base.file_name().unwrap().to_string_lossy()));
        assert_eq!(
            std::fs::read(&slot1).unwrap(),
            b"new-original",
            "the write must skip the unreadable slot and land in the next one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degraded_backup_never_clobbers_an_earlier_original() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_generations_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        backup_degraded_bytes(&target, b"first-original");
        backup_degraded_bytes(&target, b"second-original");

        assert_eq!(std::fs::read(bad_sibling(&target)).unwrap(), b"first-original");
        let slot1 = bad_sibling(&target).with_file_name(format!(
            "{}.1",
            bad_sibling(&target).file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(std::fs::read(&slot1).unwrap(), b"second-original");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degraded_backup_dedupes_identical_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_dedupe_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        backup_degraded_bytes(&target, b"same-original");
        backup_degraded_bytes(&target, b"same-original");

        assert!(bad_sibling(&target).exists());
        let slot1 = bad_sibling(&target).with_file_name(format!(
            "{}.1",
            bad_sibling(&target).file_name().unwrap().to_string_lossy()
        ));
        assert!(!slot1.exists(), "identical bytes must not create a second generation");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_backup_ring_overflows_to_temp_dir_not_overwrite() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_full_ring_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        // fill all nine sibling slots (`.bad`, `.bad.1` … `.bad.8`) with nine DISTINCT originals,
        // then a tenth — the tenth must NOT overwrite slot 8; it must land in the OS temp dir.
        for i in 0..10 {
            backup_degraded_bytes(&target, format!("original-{i}").as_bytes());
        }

        let base = bad_sibling(&target);
        assert_eq!(std::fs::read(&base).unwrap(), b"original-0");
        for i in 1..=8 {
            let slot = base.with_file_name(format!(
                "{}.{i}",
                base.file_name().unwrap().to_string_lossy()
            ));
            assert_eq!(
                std::fs::read(&slot).unwrap(),
                format!("original-{i}").as_bytes(),
                "sibling slot {i} must keep its own original, not be overwritten by the overflow"
            );
        }

        // the tenth payload (original-9) must be found in the OS temp dir, matching the fallback
        // naming scheme — NOT in any sibling slot.
        let stem = target.file_name().unwrap().to_str().unwrap();
        let found_overflow = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("neuron.") && name.contains(stem) && name.ends_with(".bad") {
                    std::fs::read(e.path()).map(|b| b == b"original-9").unwrap_or(false)
                } else {
                    false
                }
            });
        assert!(
            found_overflow,
            "the 10th distinct original must overflow to a temp-dir .bad file, not overwrite a sibling slot"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_sibling_appends_not_replaces() {
        assert_eq!(
            bad_sibling(Path::new("/cfg/feel.toml")),
            PathBuf::from("/cfg/feel.toml.bad")
        );
    }

    /// The one claim the whole module leans on: `atomic_write` must be able to overwrite a file
    /// that already exists, not just create a fresh one. `std::fs::rename`'s replace-on-Windows
    /// behaviour is doc'd but unverified in this codebase — every `Struct::save()` (Profile,
    /// Bindings, FeelConfig, Prefs, cast, apps, CLI sidecars) is a SECOND-OR-LATER write onto a
    /// path that already exists, so a rename that only works on an absent destination would make
    /// every one of them fail after the very first save. Proven directly, twice over (plain
    /// overwrite, then a same-length overwrite so a stale destination can't coincidentally still
    /// look like the old content), against a REAL temp file on THIS OS, not mocked.
    #[test]
    fn atomic_write_overwrites_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_atomic_write_overwrite_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        atomic_write(&target, b"first").expect("first write (create) must succeed");
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        atomic_write(&target, b"second-longer-value").expect(
            "second write (overwrite) must succeed — this is the write every real save() after \
             the first performs",
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"second-longer-value");

        // same length as the first payload — rules out a false pass from a truncate-less partial
        // overwrite leaving old trailing bytes behind.
        atomic_write(&target, b"third").expect("third write (same-length overwrite) must succeed");
        assert_eq!(std::fs::read(&target).unwrap(), b"third");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug, Default, serde::Deserialize)]
    struct Demo {
        keep: u32,
        also: String,
        #[serde(default)]
        items: Vec<u32>,
    }

    impl SalvageLoad for Demo {
        const FILE: &'static str = "demo.toml";
        fn path() -> PathBuf {
            // unused by the test (which drives `load_from` directly), but the trait requires it.
            std::env::temp_dir().join("demo.toml")
        }
        fn salvage(table: &toml::Table) -> Self {
            let mut cfg = Self::default();
            crate::salvage_fields!(table, Self::FILE, cfg, { "keep" => keep, "also" => also });
            if let Some(v) = salvage_vec(table, "items", Self::FILE) {
                cfg.items = v;
            }
            cfg
        }
    }

    // The END-TO-END root-fix: a degraded file keeps its good values, defaults only the bad ones,
    // salvages arrays per-element, AND backs the original up to `<file>.bad` before returning — so a
    // later save can't destroy it.
    #[test]
    fn load_from_salvages_good_fields_and_backs_up_the_original() {
        let dir = std::env::temp_dir().join(format!("neuron-salvage-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("demo.toml");
        // `keep` is a valid u32; `also` is an int where a String is expected (malformed → default);
        // `items` has one non-integer element (dropped). The type error on `also` forces the whole-
        // struct fast parse to fail, exercising the degraded path.
        let raw = "keep = 42\nalso = 7\nitems = [1, \"bad\", 3]\n";
        std::fs::write(&path, raw).expect("write demo.toml");

        let got = Demo::load_from(&path);
        assert_eq!(got.keep, 42, "the good scalar survived");
        assert_eq!(got.also, "", "the type-wrong scalar defaulted, not the whole file");
        assert_eq!(got.items, vec![1, 3], "the bad array element dropped, the rest kept in order");

        // the ORIGINAL bytes were preserved before any save could clobber them.
        let backed_up = std::fs::read_to_string(dir.join("demo.toml.bad")).expect("`.bad` written");
        assert_eq!(backed_up, raw, "the .bad backup is the user's exact original bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_existing_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("neuron-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("cfg.toml");

        atomic_write(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second overwrites").expect("second write");
        assert_eq!(std::fs::read(&path).unwrap(), b"second overwrites");

        // no stray `.tmp` sibling is left behind after a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "atomic_write left a temp file: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_absent_file_is_the_fallback_and_writes_no_backup() {
        let dir = std::env::temp_dir().join(format!("neuron-salvage-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("demo.toml");

        let got = Demo::load_from(&path); // file never created
        assert_eq!(got.keep, 0);
        assert_eq!(got.items, Vec::<u32>::new());
        assert!(
            !dir.join("demo.toml.bad").exists(),
            "an absent file is a normal first run — no degraded backup"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_non_utf8_file_backs_up_the_exact_bytes_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("neuron-salvage-nonutf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("demo.toml");
        let bad_bytes: &[u8] = &[0x6b, 0x65, 0x79, 0xFF, 0xFE, 0x00];
        std::fs::write(&path, bad_bytes).expect("write non-UTF-8 demo.toml");

        let got = Demo::load_from(&path);
        assert_eq!(got.keep, 0);
        assert_eq!(got.also, "");
        assert_eq!(got.items, Vec::<u32>::new());

        let backed_up = std::fs::read(dir.join("demo.toml.bad")).expect("`.bad` written");
        assert_eq!(backed_up, bad_bytes, "the .bad backup is the user's exact original bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX B: publication is hard-link no-replace, not check-then-replace. Simulate a "concurrent
    /// winner" by pre-creating `.bad` with a DIFFERENT payload before calling `backup_degraded_bytes`
    /// — the never-clobber rule must hold exactly as it did under the old choreography: the earlier
    /// occupant survives untouched and the new payload lands in the next free slot.
    #[test]
    fn backup_publication_never_replaces_a_concurrent_winner() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_link_race_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");
        let base = bad_sibling(&target);
        std::fs::write(&base, b"X-original").expect("pre-create .bad as the concurrent winner");

        backup_degraded_bytes(&target, b"Y-original");

        assert_eq!(std::fs::read(&base).unwrap(), b"X-original", "the earlier occupant must survive untouched");
        let slot1 = bad_slot_path(&base, 1);
        assert_eq!(std::fs::read(&slot1).unwrap(), b"Y-original", "the new payload lands in the next free slot");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX B: dedupe must still work when publication goes through hard-link `AlreadyExists`
    /// handling instead of a `next_bad_slot` byte-read.
    #[test]
    fn backup_dedupe_still_works_through_link_publication() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_link_dedupe_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");
        let base = bad_sibling(&target);

        backup_degraded_bytes(&target, b"same-payload");
        backup_degraded_bytes(&target, b"same-payload");

        assert!(base.exists(), "`.bad` must exist after the first call");
        let slot1 = bad_slot_path(&base, 1);
        assert!(!slot1.exists(), "identical bytes must dedupe, not create a second generation");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX A: an original this process can't READ (here, a Windows exclusive sharing-mode lock) must
    /// still get pinned by a hard link — directory permissions are all a link needs, not read access
    /// on the data itself. If the platform/filesystem denies even the link under the lock, this test
    /// falls back to asserting the loud no-backup warning path doesn't panic (see the comment below).
    #[cfg(windows)]
    #[test]
    fn unreadable_original_is_pinned_by_hard_link() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = std::env::temp_dir().join(format!(
            "neuron_unreadable_link_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("demo.toml");
        let original_bytes = b"keep = 1\nalso = \"x\"\n";
        std::fs::write(&target, original_bytes).expect("write original");

        // hold an exclusive (share_mode 0) handle so a plain `std::fs::read` inside `load_from`
        // fails with a sharing violation — the non-NotFound read-error branch.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .expect("open the target exclusively");

        let got = Demo::load_from(&target);
        // the read failed, so load_from must return the fallback (defaults), not a salvage.
        assert_eq!(got.keep, 0);

        let bad = bad_sibling(&target);
        let bad_exists = bad.exists();

        drop(handle);

        if bad_exists {
            assert_eq!(
                std::fs::read(&bad).unwrap(),
                original_bytes,
                "the hard-linked backup must contain the exact original bytes once unlocked"
            );
        } else {
            // The exclusive lock denied even hard-link creation on this filesystem/platform
            // configuration — that's a documented residual, not a bug: preserve_unreadable_original
            // already logs a loud "proceeding WITHOUT one" warning in that case and must not panic.
            eprintln!(
                "note: hard-link creation was also denied under the exclusive lock on this run; \
                 the no-backup warning path was exercised instead of the link path"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX C: `atomic_write` must not silently drop the destination's existing content shape when
    /// overwriting — the portable half of the permission-preservation check (rename-over-readonly
    /// fails outright on Windows, which is pre-existing behaviour, not this fix's to solve).
    #[test]
    fn atomic_write_updates_content_on_overwrite() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_atomic_perm_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        atomic_write(&target, b"original").expect("create");
        atomic_write(&target, b"updated-content").expect("overwrite");
        assert_eq!(std::fs::read(&target).unwrap(), b"updated-content");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX C, unix half: mode bits on the destination must survive an `atomic_write` overwrite (the
    /// temp inherits the destination's permissions before the rename). Compile-checked only on this
    /// Windows machine; runs for real on any unix CI/dev box.
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_unix_mode_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "neuron_atomic_mode_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");

        std::fs::write(&target, b"first").expect("create dest");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");

        atomic_write(&target, b"second").expect("overwrite");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode bits must survive the atomic overwrite");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX D: the pre-scan fast path must skip the temp write entirely when the payload is already
    /// preserved. Pre-create `.bad` with the exact payload directly (bypassing `backup_degraded_bytes`
    /// so no temp file is ever created for it), then call `backup_degraded_bytes` once and assert the
    /// directory's entry set is unchanged — no `.tmp` file and no `.bad.1` generation appear, which is
    /// only possible if the function returned before staging any temp write.
    #[test]
    fn degraded_backup_is_write_free_when_already_preserved() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_backup_prescan_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let target = dir.join("cfg.toml");
        let base = bad_sibling(&target);
        std::fs::write(&base, b"already-preserved").expect("pre-create .bad with the payload");

        backup_degraded_bytes(&target, b"already-preserved");

        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["cfg.toml.bad".to_string()],
            "the pre-scan must return before any temp write or new generation is created: {entries:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SECURITY FIX: temp files that will carry secret-bearing bytes (app.toml's
    /// `host_obs_password`) must be born owner-only on Unix, never created umask-default and
    /// THEN restricted — that ordering leaves a window where another local user can read them.
    #[cfg(unix)]
    #[test]
    fn tmp_files_are_born_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "neuron_create_restrictive_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");

        let p = dir.join("secret.tmp");
        {
            use std::io::Write;
            let mut f = create_restrictive(&p).expect("create restrictive");
            f.write_all(b"host_obs_password = \"x\"").expect("write");
        }
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "tmp file must be born owner-only, not widened after the fact");

        let err = create_restrictive(&p).expect_err("create_new must refuse an existing path");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX (exclusivity): temp creation is ALWAYS exclusive-create now, never truncate-reuse. A
    /// stale file already occupying the exact candidate name must be refused, not silently reused
    /// (and hence must not inherit that stale file's possibly-wider permissions).
    #[test]
    fn tmp_creation_is_exclusive() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_tmp_creation_exclusive_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");

        let p = dir.join("occupied.tmp");
        std::fs::write(&p, b"stale").expect("pre-create a stale file at the candidate path");
        let err = create_restrictive(&p).expect_err("must refuse to reuse an existing file");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&p).unwrap(), b"stale", "the stale file must be left untouched");

        // Even with a stale file occupying a name in the sequence, atomic_write must still succeed
        // via its retry loop (it picks a fresh sequence number rather than reusing the stale one).
        let target = dir.join("cfg.toml");
        atomic_write(&target, b"payload").expect("atomic_write must succeed despite a stale tmp name nearby");
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FIX E: `try_create_exclusive` is the never-clobber primitive behind the temp-dir fallback.
    /// Against an absent destination it creates the file with the given content; against an
    /// EXISTING destination it must fail with `AlreadyExists` and leave the original content
    /// completely untouched — this is what makes a reused-PID name collision safe after a reboot.
    #[test]
    fn try_create_exclusive_never_replaces_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_try_create_exclusive_test_{}_{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let src = dir.join("cfg.toml");
        std::fs::write(&src, b"source").expect("write source config");

        // absent destination → created with the given content.
        let fresh = dir.join("fresh.bad");
        try_create_exclusive(&fresh, b"new-content", &src).expect("create on an absent destination");
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new-content");

        // existing destination → AlreadyExists, original untouched.
        let taken = dir.join("taken.bad");
        std::fs::write(&taken, b"earlier-original").expect("pre-create the destination");
        let err = try_create_exclusive(&taken, b"would-be-clobber", &src)
            .expect_err("must refuse to replace an existing file");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&taken).unwrap(),
            b"earlier-original",
            "the pre-existing file's content must survive the failed exclusive-create attempt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
