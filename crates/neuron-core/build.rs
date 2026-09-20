// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../LICENSE.md.

//! Build script: BUNDLE a private CPython into `neuron`.
//!
//! It maps the Cargo build TARGET to a `python-build-standalone` (PBS) release triple, makes sure a
//! verified copy of that triple's `install_only_stripped` tarball lives in `<repo>/vendor/pbs-cache/`
//! (downloading + sha256-checking it on a cache miss), and exports the cached tarball's path so the
//! crate can `include_bytes!` it (see `src/macros/pyruntime.rs`). The interpreter is then unpacked
//! into the user's data dir at runtime — no system Python, no env-var hacks.
//!
//! GROUND TRUTH (pinned): PBS release tag `20260610`, CPython `3.12.13`, variant
//! `install_only_stripped`.
//! Asset:  cpython-<PYVER>+<TAG>-<TRIPLE>-install_only_stripped.tar.gz
//! URL:    https://github.com/astral-sh/python-build-standalone/releases/download/<TAG>/<asset>
//!         (the literal `+` in the asset name is `%2B` in the URL).
//! Integrity: this release ships ONE `SHA256SUMS` manifest (NOT per-asset `.sha256` siblings —
//! verified against the real release). We fetch it, find the line for our asset, and compare.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Pinned PBS release tag.
const TAG: &str = "20260610";
/// Pinned CPython version.
const PYVER: &str = "3.12.13";

fn main() {
    // build.rs only needs to re-run when itself or the selected target changes; the cache makes the
    // download a one-time cost. (Cargo reruns build scripts when build-dep inputs change anyway.)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=NEURON_PBS_OFFLINE");

    let target = std::env::var("TARGET").expect("cargo always sets TARGET for build scripts");
    let triple = match target_to_triple(&target) {
        Some(t) => t,
        None => panic!(
            "neuron bundles a private CPython, but the build target `{target}` has no \
             python-build-standalone mapping. Supported targets: {}. \
             Add a mapping in crates/neuron-core/build.rs::target_to_triple if PBS ships this triple.",
            supported_targets().join(", ")
        ),
    };

    // `install_only_stripped`, not `install_only`: the same tree with debug symbols removed,
    // published for every triple we map. Unix carries its debug info inside the ELF binaries
    // (`libpython3.12.so.1.0` is 218 MB unstripped, 32 MB stripped), where no path-based prune
    // list can reach it; Windows parks it in sibling `.pdb` files the prune list drops.
    let asset = format!("cpython-{PYVER}+{TAG}-{triple}-install_only_stripped.tar.gz");
    let cache_dir = repo_vendor_cache();
    std::fs::create_dir_all(&cache_dir)
        .unwrap_or_else(|e| panic!("create pbs cache dir {}: {e}", cache_dir.display()));
    let tarball = cache_dir.join(&asset);
    // Declare the cached tarball as a build input so deleting it ACTUALLY forces a re-fetch — cargo
    // only re-runs a build script when a declared input changes, so without this the "delete to
    // re-fetch" note below was a no-op (a deleted cache surfaced as an include_bytes! file-not-found
    // needing `cargo clean`, not an automatic re-download).
    println!("cargo:rerun-if-changed={}", tarball.display());

    if !tarball.exists() {
        // Cache miss: download the tarball + the release SHA256SUMS manifest, verify, then commit.
        ensure_tarball(&tarball, triple, &asset);
    }

    // SLIM the verified upstream tarball into a sibling `<asset>.slim.tar.gz` (stream-filter; see
    // `slim_tarball`). The crate embeds THIS one — dropping ~100 MB uncompressed of runtime-useless
    // weight (debug symbols, pip/ensurepip/venv, test suite, __pycache__) with zero feature loss.
    let slim = cache_dir.join(format!(
        "{}.slim.tar.gz",
        asset.strip_suffix(".tar.gz").unwrap_or(&asset)
    ));
    // Deleting the slim forces a re-slim on the next build (cargo only re-runs a build script when a
    // declared input changes).
    println!("cargo:rerun-if-changed={}", slim.display());
    ensure_slim(&tarball, &slim, &manifest_build_rs());

    // Hand the crate the absolute SLIM tarball path + the identity it was built for.
    println!("cargo:rustc-env=NEURON_PY_TARBALL={}", slim.display());
    println!("cargo:rustc-env=NEURON_PY_VER={PYVER}");
    println!("cargo:rustc-env=NEURON_PY_TRIPLE={triple}");
}

/// Absolute path to THIS build script — the slim cache's freshness anchor (editing the prune list
/// edits build.rs, which re-slims).
fn manifest_build_rs() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"))
        .join("build.rs")
}

/// Ensure a SLIMMED copy of `src` exists at `slim`, (re)building it only on a cache miss: when
/// `slim` is MISSING or OLDER than `build_rs` (so editing the prune list in build.rs re-slims, while
/// a warm cache is a no-op — keeping incremental builds fast and the embedded blob/link small).
fn ensure_slim(src: &Path, slim: &Path, build_rs: &Path) {
    if !needs_reslim(slim, build_rs) {
        return;
    }
    println!(
        "cargo:warning=neuron: slimming CPython tarball (one-time) → {}",
        slim.display()
    );
    slim_tarball(src, slim);
}

/// A re-slim is needed iff the slim file is absent, or it predates `build_rs` (the prune list moved).
/// If either mtime can't be read we err toward NOT re-slimming a present cache (avoid churn); a
/// missing slim always re-slims.
fn needs_reslim(slim: &Path, build_rs: &Path) -> bool {
    let Ok(slim_m) = slim.metadata().and_then(|m| m.modified()) else {
        return true; // missing/unreadable slim → (re)build it
    };
    match build_rs.metadata().and_then(|m| m.modified()) {
        Ok(build_m) => slim_m < build_m, // re-slim if the prune list is newer than the cache
        Err(_) => false,                 // can't compare → trust the present cache
    }
}

/// Stream-filter `src` (a gzip'd tar) into `dest`, KEEPING every entry [`should_prune`] doesn't
/// reject. Reads via `flate2::read::GzDecoder` + `tar::Archive::entries()` and writes the kept
/// entries to a fresh `tar::Builder` over `flate2::write::GzEncoder`, copying each entry's UPSTREAM
/// header so mode/mtime are preserved (so unix `python/bin/python3` stays executable). Written
/// ATOMICALLY (temp + rename) so a killed build never leaves a half-slim that looks cached. Panics
/// with a clear message on any IO/format failure — a broken bundle must fail the build, not ship a
/// Python that can't run.
fn slim_tarball(src: &Path, dest: &Path) {
    use std::fs::File;
    use std::io::{BufReader, BufWriter};

    let in_f = File::open(src)
        .unwrap_or_else(|e| panic!("neuron: open upstream tarball {}: {e}", src.display()));
    let gz_in = flate2::read::GzDecoder::new(BufReader::new(in_f));
    let mut archive = tar::Archive::new(gz_in);

    // Atomic temp sibling: write fully, then rename into place.
    let tmp = dest.with_file_name(format!(
        "{}.partial",
        dest.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "slim.tar.gz".into())
    ));
    let _ = std::fs::remove_file(&tmp); // clear any stale partial from a killed build

    let out_f = File::create(&tmp)
        .unwrap_or_else(|e| panic!("neuron: create slim temp {}: {e}", tmp.display()));
    // `best` (level 9): the slim is cached + one-time, so spend CPU once for the smallest embedded
    // blob (smaller binary + faster links forever after).
    let gz_out = flate2::write::GzEncoder::new(BufWriter::new(out_f), flate2::Compression::best());
    let mut builder = tar::Builder::new(gz_out);

    let (mut kept, mut skipped) = (0u32, 0u32);
    let entries = archive
        .entries()
        .unwrap_or_else(|e| panic!("neuron: read upstream tarball entries: {e}"));
    for entry in entries {
        let mut entry =
            entry.unwrap_or_else(|e| panic!("neuron: bad entry in upstream tarball: {e}"));
        let path = entry
            .path()
            .unwrap_or_else(|e| panic!("neuron: bad entry path: {e}"))
            .to_string_lossy()
            .into_owned();
        if should_prune(&path) {
            skipped += 1;
            continue;
        }
        // Copy the entry verbatim, header and all. All KEPT paths are short (well under the 100-byte
        // ustar name limit — verified against the real PBS layout), so the upstream header carries
        // the full name and `append(&header, data)` is faithful (no long-name truncation).
        let header = entry.header().clone();
        builder
            .append(&header, &mut entry)
            .unwrap_or_else(|e| panic!("neuron: append {path} to slim tar: {e}"));
        kept += 1;
    }

    // Finish the tar stream, then the gzip stream, then flush the file to disk before the rename.
    let gz_out = builder
        .into_inner()
        .unwrap_or_else(|e| panic!("neuron: finish slim tar: {e}"));
    let buf = gz_out
        .finish()
        .unwrap_or_else(|e| panic!("neuron: finish slim gzip: {e}"));
    buf.into_inner()
        .unwrap_or_else(|e| panic!("neuron: flush slim tarball: {e}"));

    std::fs::rename(&tmp, dest)
        .unwrap_or_else(|e| panic!("neuron: commit slim tarball to {}: {e}", dest.display()));
    println!("cargo:warning=neuron: slimmed CPython tarball — kept {kept} entries, dropped {skipped}");
}

/// Decide whether a tar entry should be DROPPED from the slim interpreter. KEEP everything this
/// doesn't reject (when unsure, keep). The path is normalized `\`→`/` first.
///
/// python-build-standalone lays the stdlib out differently per platform — `python/Lib/…` on
/// Windows, `python/lib/python3.12/…` everywhere else — so the stdlib rules are expressed as names
/// relative to the stdlib root that [`stdlib_rel`] finds, not as literal path fragments. A literal
/// `/Lib/…` fragment matches nothing off Windows, and prunes nothing, silently.
///
/// Drops, and ONLY these (the prune list):
///   * `*.pdb`                  — Windows debug symbols (~82 MB), useless at runtime
///   * `*/__pycache__/*`        — byte-compiled dupes; CPython rebuilds .pyc on first import
///   * `<stdlib>/test/*`        — the CPython regression suite (KEEP `<stdlib>/unittest/`)
///   * `<stdlib>/idlelib/*`     — the IDLE editor
///   * `<stdlib>/lib2to3/*`     — the py2→3 fixers
///   * `<stdlib>/ensurepip/*`   — the pip bootstrapper (and its bundled ~1.8 MB wheel)
///   * `<stdlib>/venv/*`        — the venv builder
///   * `<stdlib>/turtledemo/*`  — turtle DEMOS (KEEP `<stdlib>/turtle.py` + tkinter/tcl)
///   * `<stdlib>/config-*/*`    — the unix build config (Makefile, `libpython*.a`); only ever used
///     to COMPILE against this interpreter, and nothing we ship compiles C extensions
///   * `python/include/*`       — the C headers, same reason
///   * `*/site-packages/{pip,setuptools,pkg_resources,_distutils_hack}*` — packaging machinery
///     (the prefix also catches the matching `*.dist-info` dirs, e.g. `pip-26.1.2.dist-info`)
fn should_prune(raw_path: &str) -> bool {
    let p = raw_path.replace('\\', "/");

    if p.to_ascii_lowercase().ends_with(".pdb") {
        return true;
    }
    if p.contains("/__pycache__/") {
        return true;
    }
    if p.starts_with("python/include/") {
        return true;
    }
    if let Some(rel) = stdlib_rel(&p) {
        const STDLIB_DROP: [&str; 6] = [
            "test/",
            "idlelib/",
            "lib2to3/",
            "ensurepip/",
            "venv/",
            "turtledemo/",
        ];
        if STDLIB_DROP.iter().any(|s| rel.starts_with(s)) {
            return true;
        }
        // `config-3.12-x86_64-linux-gnu/` and friends — the name carries the platform, so match the
        // prefix rather than enumerating triples.
        if rel.starts_with("config-") {
            return true;
        }
    }
    const SITE_DROP: [&str; 4] = [
        "/site-packages/pip",
        "/site-packages/setuptools",
        "/site-packages/pkg_resources",
        "/site-packages/_distutils_hack",
    ];
    if SITE_DROP.iter().any(|s| p.contains(s)) {
        return true;
    }
    false
}

/// The part of `p` below the CPython stdlib root, if `p` is inside one. `/`-normalized input.
///
/// Two layouts, both anchored at the tarball's `python/` prefix:
///   * Windows — `python/Lib/<rel>`
///   * unix    — `python/lib/python3.12/<rel>` (the version is read from the path, not assumed)
///
/// The unix arm deliberately requires the `python3.` segment: `python/lib/` also holds
/// `libpython3.12.so`, `libtcl9.0.so` and the tcl data dirs, none of which are stdlib and none of
/// which the stdlib rules should ever reach. Pure + total → unit-testable (see `tests` below).
fn stdlib_rel(p: &str) -> Option<&str> {
    if let Some(rel) = p.strip_prefix("python/Lib/") {
        return Some(rel);
    }
    let rest = p.strip_prefix("python/lib/")?;
    let (dir, rel) = rest.split_once('/')?;
    dir.starts_with("python3.").then_some(rel)
}

/// Map a Cargo/Rust target triple to the python-build-standalone release triple.
///
/// PBS's triples happen to equal the common Rust target triples, so most are an identity pass —
/// but we ENUMERATE the supported set rather than passing anything through, so an unsupported
/// target fails the build loudly (a wrong/missing Python is worse than a clear compile error).
/// Pure + total over `&str` → unit-testable (see `tests` below).
fn target_to_triple(target: &str) -> Option<&'static str> {
    Some(match target {
        // Windows (MSVC) — what we ship on Windows.
        "x86_64-pc-windows-msvc" => "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-pc-windows-msvc",
        // macOS (Intel + Apple Silicon).
        "x86_64-apple-darwin" => "x86_64-apple-darwin",
        "aarch64-apple-darwin" => "aarch64-apple-darwin",
        // Linux glibc.
        "x86_64-unknown-linux-gnu" => "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-unknown-linux-gnu",
        // Linux musl (static distros / Alpine) — PBS ships these too.
        "x86_64-unknown-linux-musl" => "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-unknown-linux-musl",
        _ => return None,
    })
}

/// The supported Cargo targets, for the unsupported-target error message + tests.
fn supported_targets() -> Vec<&'static str> {
    vec![
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ]
}

/// `<repo>/vendor/pbs-cache/` — the shared, gitignored interpreter cache. CARGO_MANIFEST_DIR is
/// `<repo>/crates/neuron-core`, so the repo root is two levels up.
fn repo_vendor_cache() -> PathBuf {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    manifest
        .join("..")
        .join("..")
        .join("vendor")
        .join("pbs-cache")
}

/// Download `asset` (+ verify against the release SHA256SUMS manifest), writing it ATOMICALLY to
/// `dest` (download to a sibling temp file, fsync-by-rename) so a killed build never leaves a
/// half-tarball that looks cached. Panics with a clear message on any failure — a broken bundle
/// must fail the build, not ship a Python that isn't there.
fn ensure_tarball(dest: &Path, triple: &str, asset: &str) {
    if std::env::var_os("NEURON_PBS_OFFLINE").is_some() {
        panic!(
            "neuron: CPython tarball for `{triple}` is not in the cache ({}) and \
             NEURON_PBS_OFFLINE is set — refusing to download. Pre-seed the cache or unset the var.",
            dest.display()
        );
    }

    let base = format!(
        "https://github.com/astral-sh/python-build-standalone/releases/download/{TAG}"
    );
    // The literal `+` in the asset name must be percent-encoded in the URL path.
    let asset_url = format!("{base}/{}", asset.replace('+', "%2B"));
    let sums_url = format!("{base}/SHA256SUMS");

    println!("cargo:warning=neuron: downloading CPython {PYVER} for {triple} (one-time, ~46 MB)…");

    let want = expected_sha(&sums_url, asset);
    let bytes = http_get(&asset_url);

    let got = {
        let mut h = Sha256::new();
        h.update(&bytes);
        hex(&h.finalize())
    };
    if got != want {
        panic!(
            "neuron: SHA256 MISMATCH for {asset}\n  expected (SHA256SUMS): {want}\n  \
             downloaded:           {got}\nRefusing to bundle a tampered/corrupt interpreter."
        );
    }

    // Atomic commit: write to a temp sibling, then rename into place.
    let tmp = dest.with_extension("gz.partial");
    std::fs::write(&tmp, &bytes)
        .unwrap_or_else(|e| panic!("write temp tarball {}: {e}", tmp.display()));
    std::fs::rename(&tmp, dest)
        .unwrap_or_else(|e| panic!("commit tarball to {}: {e}", dest.display()));
    println!(
        "cargo:warning=neuron: cached + verified CPython tarball at {}",
        dest.display()
    );
}

/// Fetch the SHA256SUMS manifest and return the expected hex digest for `asset` (the line is
/// `<hex>␠␠<filename>`). Panics if the asset isn't listed.
fn expected_sha(sums_url: &str, asset: &str) -> String {
    let body = http_get(sums_url);
    let text = String::from_utf8(body).expect("SHA256SUMS is utf-8 text");
    for line in text.lines() {
        // Format: "<64-hex>  <filename>" (two spaces). Match on the trailing filename so a
        // substring of another asset name can't collide.
        let mut parts = line.split_whitespace();
        let (Some(hexsum), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name == asset {
            return hexsum.to_ascii_lowercase();
        }
    }
    panic!("neuron: {asset} not found in SHA256SUMS manifest ({sums_url})");
}

/// Blocking HTTP GET → bytes. `ureq` (rustls) follows redirects (GitHub release assets 302 to a
/// CDN). Panics with the URL on any failure.
fn http_get(url: &str) -> Vec<u8> {
    let mut resp = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("neuron: HTTP GET {url} failed: {e}"));
    let mut buf = Vec::new();
    use std::io::Read;
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| panic!("neuron: reading {url} body failed: {e}"));
    buf
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_target_maps_to_a_triple() {
        for t in supported_targets() {
            let mapped = target_to_triple(t)
                .unwrap_or_else(|| panic!("supported target {t} must map"));
            // PBS triples equal the Rust triples for our supported set.
            assert_eq!(mapped, t, "{t} should map to itself");
        }
    }

    #[test]
    fn unsupported_targets_return_none() {
        assert!(target_to_triple("mips64-unknown-linux-gnuabi64").is_none());
        assert!(target_to_triple("wasm32-unknown-unknown").is_none());
        assert!(target_to_triple("").is_none());
        assert!(target_to_triple("x86_64-pc-windows-gnu").is_none()); // gnu (not msvc) isn't shipped
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa0]), "000fffa0");
    }

    #[test]
    fn stdlib_rel_finds_both_layouts() {
        assert_eq!(stdlib_rel("python/Lib/encodings/utf_8.py"), Some("encodings/utf_8.py"));
        assert_eq!(
            stdlib_rel("python/lib/python3.12/encodings/utf_8.py"),
            Some("encodings/utf_8.py")
        );
        // `python/lib/` is NOT the stdlib on unix — the shared libs and tcl data live there too.
        assert_eq!(stdlib_rel("python/lib/libpython3.12.so.1.0"), None);
        assert_eq!(stdlib_rel("python/lib/tcl9.0/encoding/cp936.enc"), None);
        assert_eq!(stdlib_rel("python/bin/python3.12"), None);
    }

    /// The prune list must bite IDENTICALLY on both layouts. It once read `/Lib/ensurepip/` only,
    /// which matched nothing on Linux and quietly shipped an un-slimmed interpreter.
    #[test]
    fn prune_list_is_layout_symmetric() {
        for stdlib in ["python/Lib", "python/lib/python3.12"] {
            for drop in [
                "test/test_os.py",
                "idlelib/idle.py",
                "lib2to3/refactor.py",
                "ensurepip/_bundled/pip-25.0.1-py3-none-any.whl",
                "venv/__init__.py",
                "turtledemo/clock.py",
                "config-3.12-x86_64-linux-gnu/libpython3.12.a",
            ] {
                let p = format!("{stdlib}/{drop}");
                assert!(should_prune(&p), "should have pruned {p}");
            }
            for keep in [
                "unittest/case.py",
                "turtle.py",
                "encodings/utf_8.py",
                "tkinter/__init__.py",
                "site-packages/README.txt",
            ] {
                let p = format!("{stdlib}/{keep}");
                assert!(!should_prune(&p), "should have KEPT {p}");
            }
        }
    }

    #[test]
    fn prune_list_covers_the_platform_specific_weight() {
        assert!(should_prune("python/python312.pdb"));
        assert!(should_prune("python/Lib/asyncio/__pycache__/base_events.cpython-312.pyc"));
        assert!(should_prune("python/include/python3.12/Python.h"));
        assert!(should_prune("python/lib/python3.12/site-packages/pip/__init__.py"));
        assert!(should_prune("python/lib/python3.12/site-packages/pip-25.0.1.dist-info/RECORD"));
        // The interpreter itself and its shared library are the whole point of the bundle.
        assert!(!should_prune("python/bin/python3.12"));
        assert!(!should_prune("python/lib/libpython3.12.so.1.0"));
        assert!(!should_prune("python/python.exe"));
    }
}
