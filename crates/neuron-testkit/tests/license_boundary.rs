// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Machine-enforced mirror of the repository-root `LICENSE.md` Work Notice.
//!
//! Neuron is dual-licensed BY PATH: almost everything is GPL-3.0-or-later (with the
//! Neuron-Woflo exception), and exactly four Work Notice entries are the WLCSL-1.0
//! research exception (`crates/engram/**`, and three files in `crates/neuron-core/src/`).
//!
//! That boundary is a legal fact, not a vibe, and nothing in the normal edit/build/test
//! loop stops it from drifting: a new research file can be added without a WLCSL header,
//! an existing WLCSL file can lose its header in a refactor, or someone can edit
//! `LICENSE.md`'s bullet list without updating what the source actually carries. Any of
//! those is silent until a human reads both the code and the license side by side. This
//! test makes that comparison automatic and turns drift into a red `cargo test`.
//!
//! The mirror list below is intentionally the ONLY place that encodes the boundary in
//! test code; `LICENSE.md` is the legal controlling document. Test 4 keeps them in sync
//! by asserting the mirror's exact bulleted text is still present in `LICENSE.md`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The Work Notice's WLCSL-1.0 entries, verbatim from `LICENSE.md`'s research-components
/// bullet list. Keep this array and the `LICENSE.md` bullets in lockstep — test 4 checks it.
const WLCSL_ENTRIES: &[&str] = &[
    "crates/engram/**",
    "crates/neuron-core/src/glyph.rs",
    "crates/neuron-core/src/logos.rs",
    "crates/neuron-core/src/gwyph.rs",
];

/// Crates whose `src/`, `tests/`, `examples/`, and `build.rs` are walked for license headers.
/// (Every crate in the workspace — the WLCSL/GPL split is decided per-file against
/// `WLCSL_ENTRIES`, not per-crate, so engram is walked exactly like everything else.)
const CRATES: &[&str] = &[
    "engram",
    "neuron-app",
    "neuron-cli",
    "neuron-core",
    "neuron-host",
    "neuron-testkit",
];

/// Crates that must never contain a WLCSL file, spelled out as an explicit guard (test 6)
/// even though it is already implied by test 2's set-equality check.
const NEVER_WLCSL_CRATES: &[&str] = &["neuron-app", "neuron-cli", "neuron-host", "neuron-testkit"];

/// A source file discovered by the walk, with its relative (forward-slash) path and the
/// `SPDX-License-Identifier` values found in it (should be exactly one).
struct Found {
    /// Path relative to the workspace root, forward-slash normalized so the test behaves
    /// identically on Windows and Linux.
    rel: String,
    identifiers: Vec<String>,
}

/// Resolve the workspace root from `CARGO_MANIFEST_DIR` (this crate lives at
/// `crates/neuron-testkit`, so the root is two levels up) and sanity-check it by requiring
/// `LICENSE.md` there — if the crate is ever moved, this fails loudly instead of silently
/// walking the wrong directory and reporting a false "all clear".
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crates/neuron-testkit should have a workspace root two levels up")
        .to_path_buf();
    assert!(
        root.join("LICENSE.md").is_file(),
        "expected workspace root at {} to contain LICENSE.md — \
         crates/neuron-testkit may have moved; update workspace_root() in license_boundary.rs",
        root.display()
    );
    root
}

/// Extensions we can and do assert license headers on. Deliberately narrow: `.md`,
/// `.json`, lockfiles, and binaries either can't carry a `//`/`#` comment header or are not
/// "mandatory source" under the Work Notice, so asserting on them would be noise or wrong.
fn is_source_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "slint")
    )
}

/// Recursively collect every `.rs`/`.slint` file under `dir`, plus the caller adding
/// `build.rs` separately (build.rs lives at the crate root, not under src/tests/examples).
fn walk_source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file_type = entry.file_type().expect("file_type");
        if file_type.is_dir() {
            walk_source_files(&path, out);
        } else if file_type.is_file() && is_source_extension(&path) {
            out.push(path);
        }
    }
}

/// Normalize a path (relative to `root`) to forward slashes, so set comparisons and printed
/// diffs are identical on Windows and Linux regardless of which OS ran the walk.
fn relativize(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("walked path should be under workspace root")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Walk every crate's `src/`, `tests/`, `examples/`, and `build.rs`, reading each file's
/// `SPDX-License-Identifier:` line(s). This is the single source of "what does the repo
/// actually say" that every test below checks against the Work Notice's "what should it say".
fn collect_all_source_files() -> Vec<Found> {
    let root = workspace_root();
    let mut paths = Vec::new();

    for crate_name in CRATES {
        let crate_dir = root.join("crates").join(crate_name);
        for sub in ["src", "tests", "examples"] {
            walk_source_files(&crate_dir.join(sub), &mut paths);
        }
        let build_rs = crate_dir.join("build.rs");
        if build_rs.is_file() {
            paths.push(build_rs);
        }
    }

    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let rel = relativize(&root, &path);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {rel}: {e}"));
            let identifiers = text
                .lines()
                .filter_map(|line| {
                    let trimmed = line.trim_start_matches(['/', '#', ' ']);
                    trimmed
                        .strip_prefix("SPDX-License-Identifier:")
                        .map(|rest| rest.trim().to_string())
                })
                .collect();
            Found { rel, identifiers }
        })
        .collect()
}

/// Test 1: every mandatory source file declares EXACTLY one license identifier.
///
/// Zero identifiers means an unheadered file — nobody has said what license it is under,
/// which is exactly the silent-drift case this whole test file exists to catch. Two or
/// more means an ambiguous/duplicated header (e.g. a bad merge or copy-paste), which is
/// just as unsafe as zero: a reader or a tool can't tell which line is authoritative.
#[test]
fn every_mandatory_source_file_has_exactly_one_license_identifier() {
    let files = collect_all_source_files();
    assert!(!files.is_empty(), "walk found no source files — is workspace_root() correct?");

    let mut bad = Vec::new();
    for f in &files {
        if f.identifiers.len() != 1 {
            bad.push(format!(
                "{}: found {} SPDX-License-Identifier line(s) (expected exactly 1): {:?}",
                f.rel,
                f.identifiers.len(),
                f.identifiers
            ));
        }
    }

    assert!(
        bad.is_empty(),
        "\nfiles with zero or multiple license identifiers (add exactly one \
         `SPDX-License-Identifier:` line — GPL-3.0-or-later or LicenseRef-WLCSL-1.0):\n  {}\n",
        bad.join("\n  ")
    );
}

/// Test 2 (the anti-drift core): the set of files that CARRY a WLCSL header must exactly
/// equal the set of files that MATCH a Work Notice entry.
///
/// This is the check that actually catches drift, in both directions:
/// - a new research file added under a name not listed in `LICENSE.md` (carries WLCSL,
///   not listed) is a legal problem: the license text and the code disagree about what
///   is licensed how.
/// - a listed research file that lost its WLCSL header in a refactor (listed, doesn't
///   carry WLCSL) is silently shipping under the wrong claimed default (GPL) even though
///   the Work Notice still says it's WLCSL-governed.
#[test]
fn wlcsl_headers_match_the_work_notice_exactly() {
    let files = collect_all_source_files();

    let carries_wlcsl: BTreeSet<&str> = files
        .iter()
        .filter(|f| f.identifiers.iter().any(|id| id == "LicenseRef-WLCSL-1.0"))
        .map(|f| f.rel.as_str())
        .collect();

    let listed: BTreeSet<&str> = files
        .iter()
        .filter(|f| matches_work_notice(&f.rel))
        .map(|f| f.rel.as_str())
        .collect();

    let carries_but_not_listed: Vec<&str> =
        carries_wlcsl.difference(&listed).copied().collect();
    let listed_but_no_header: Vec<&str> =
        listed.difference(&carries_wlcsl).copied().collect();

    assert!(
        carries_but_not_listed.is_empty() && listed_but_no_header.is_empty(),
        "\nWLCSL header set and Work Notice list have drifted apart:\n\
         carries WLCSL-1.0 but is NOT in the LICENSE.md list (add the path to LICENSE.md, \
         or this is unintentionally-leaked research code):\n  {}\n\
         is listed in LICENSE.md but does NOT carry a LicenseRef-WLCSL-1.0 header \
         (restore the header, or remove the stale bullet from LICENSE.md):\n  {}\n",
        if carries_but_not_listed.is_empty() {
            "(none)".to_string()
        } else {
            carries_but_not_listed.join("\n  ")
        },
        if listed_but_no_header.is_empty() {
            "(none)".to_string()
        } else {
            listed_but_no_header.join("\n  ")
        }
    );
}

/// Test 3: every file NOT matched by a Work Notice entry must declare GPL-3.0-or-later,
/// and in particular must never declare WLCSL. This is the complement of test 2 stated
/// as a positive claim about the GPL majority, so a reviewer reading test names alone
/// gets the full boundary, not just the WLCSL side of it.
#[test]
fn non_work_notice_files_are_gpl_never_wlcsl() {
    let files = collect_all_source_files();

    let mut bad = Vec::new();
    for f in &files {
        if matches_work_notice(&f.rel) {
            continue;
        }
        let is_gpl = f.identifiers.iter().any(|id| id == "GPL-3.0-or-later");
        let is_wlcsl = f.identifiers.iter().any(|id| id == "LicenseRef-WLCSL-1.0");
        if is_wlcsl {
            bad.push(format!(
                "{}: declares LicenseRef-WLCSL-1.0 but is not in the LICENSE.md Work Notice list",
                f.rel
            ));
        } else if !is_gpl {
            bad.push(format!(
                "{}: declares {:?}, expected GPL-3.0-or-later (file is outside the WLCSL list)",
                f.rel, f.identifiers
            ));
        }
    }

    assert!(
        bad.is_empty(),
        "\nnon-Work-Notice files with the wrong license:\n  {}\n",
        bad.join("\n  ")
    );
}

/// Parse the WLCSL path list straight out of the Work Notice: the FIRST backticked span of every
/// `- ` bullet under the research-components heading, stopping at the next `## ` heading.
///
/// Taking only the first span per bullet is deliberate — a bullet may legitimately mention other
/// backticked names (engram's entry carves out its own nested `LICENSE.md`), and the path is always
/// the leading one. Continuation lines are not bullets, so they are skipped.
fn work_notice_wlcsl_paths(license_text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut in_section = false;
    for line in license_text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // the one section that grants WLCSL to specific paths
            in_section = heading.contains("WLCSL-1.0") && heading.contains("research components");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(bullet) = line.strip_prefix("- ") else {
            continue;
        };
        if let Some(open) = bullet.find('`') {
            if let Some(len) = bullet[open + 1..].find('`') {
                found.insert(bullet[open + 1..open + 1 + len].to_string());
            }
        }
    }
    found
}

/// Test 4: the test's mirror of the Work Notice list (`WLCSL_ENTRIES`) and `LICENSE.md`'s actual
/// bullet list must be THE SAME SET, checked in both directions.
///
/// A one-directional check ("is every mirror entry present in LICENSE.md?") is not enough, and the
/// gap is not theoretical: adding a fifth protected path to `LICENSE.md` without updating this
/// mirror would leave every other test in this file measuring headers against the STALE four-entry
/// list. If that new path then shipped a GPL header, test 2 would call it correctly-GPL, test 3
/// would agree, and the suite would go green while the controlling legal document said WLCSL. That
/// is exactly the drift this file exists to make impossible, so the comparison runs both ways.
#[test]
fn mirror_list_matches_license_md_verbatim() {
    let root = workspace_root();
    let license_path = root.join("LICENSE.md");
    let license_text = fs::read_to_string(&license_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", license_path.display()));

    let in_notice = work_notice_wlcsl_paths(&license_text);
    let in_mirror: BTreeSet<String> = WLCSL_ENTRIES.iter().map(|s| (*s).to_string()).collect();

    assert!(
        !in_notice.is_empty(),
        "\nparsed ZERO WLCSL bullets out of LICENSE.md — the Work Notice's \
         \"## Woflo research components: WLCSL-1.0\" heading or its bullet shape changed, so this \
         test can no longer see the list it is supposed to police. Fix the parser or the heading.\n"
    );

    let notice_only: Vec<&String> = in_notice.difference(&in_mirror).collect();
    let mirror_only: Vec<&String> = in_mirror.difference(&in_notice).collect();
    assert!(
        notice_only.is_empty() && mirror_only.is_empty(),
        "\nLICENSE.md's WLCSL bullet list and this test's WLCSL_ENTRIES mirror disagree — update \
         whichever is wrong so they match again:\n  \
         in LICENSE.md but NOT in the test mirror (every other test here is measuring against a \
         stale list): {notice_only:?}\n  \
         in the test mirror but NOT in LICENSE.md (the test is policing a path the controlling \
         document no longer grants): {mirror_only:?}\n"
    );
}

/// Test 5: every GPL-declaring file also names the Neuron-Woflo exception via an
/// `Additional permission:` line. The exception is what makes it legal to combine GPL
/// code with the WLCSL research components in one binary (see `LICENSE.md` §"Combined
/// Neuron builds"); a GPL header missing that line is silently claiming plain GPL-3.0,
/// which is a narrower (and, for this repo, incorrect) grant.
#[test]
fn gpl_files_name_the_neuron_woflo_exception() {
    let root = workspace_root();
    let files = collect_all_source_files();

    let mut missing_exception = Vec::new();
    for f in &files {
        if !f.identifiers.iter().any(|id| id == "GPL-3.0-or-later") {
            continue;
        }
        let full_path = root.join(&f.rel);
        let text = fs::read_to_string(&full_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", f.rel));
        // Look near the header (first ~10 lines) for the additional-permission line,
        // rather than anywhere in the file, so a mention buried in a doc comment
        // elsewhere doesn't count as the file's own license notice.
        let header_has_exception = text
            .lines()
            .take(10)
            .any(|line| line.contains("Additional permission") && line.contains("Neuron-Woflo"));
        if !header_has_exception {
            missing_exception.push(f.rel.as_str());
        }
    }

    // Pre-existing GPL files that legitimately lack the exception line are reported
    // here, not silently exempted — see the task report for whether any showed up.
    assert!(
        missing_exception.is_empty(),
        "\nGPL-3.0-or-later files missing the 'Additional permission: Neuron-Woflo exception' \
         line in their header (add the 3-line GPL header form used elsewhere in the repo):\n  {}\n",
        missing_exception.join("\n  ")
    );
}

/// Test 6: an explicit, narrowly-worded guard that app/CLI/host/testkit code never
/// carries WLCSL. Redundant with test 2's set-equality (none of these crates' paths can
/// match `WLCSL_ENTRIES`), but a guard phrased as "these four crates are plain-GPL
/// consumers of the research core, full stop" is the sentence a contributor is most
/// likely to remember when adding a new file, so it earns its keep as a separate test.
#[test]
fn app_cli_host_testkit_never_carry_wlcsl() {
    let files = collect_all_source_files();

    let mut bad = Vec::new();
    for f in &files {
        let in_guarded_crate = NEVER_WLCSL_CRATES
            .iter()
            .any(|c| f.rel.starts_with(&format!("crates/{c}/")));
        if !in_guarded_crate {
            continue;
        }
        if f.identifiers.iter().any(|id| id == "LicenseRef-WLCSL-1.0") {
            bad.push(f.rel.as_str());
        }
    }

    assert!(
        bad.is_empty(),
        "\nfiles under neuron-app/neuron-cli/neuron-host/neuron-testkit declare WLCSL-1.0, \
         but these crates are plain-GPL consumers of the research core and must never carry \
         the research license themselves:\n  {}\n",
        bad.join("\n  ")
    );
}

/// Does `rel` (a forward-slash workspace-relative path) match a `WLCSL_ENTRIES` pattern?
/// `**` matches the whole subtree (any depth, including the entry's own directory);
/// `*` (unused today, but supported per the Work Notice's own wording) matches a single
/// path segment. Everything else must match the entry's path exactly.
fn matches_work_notice(rel: &str) -> bool {
    WLCSL_ENTRIES.iter().any(|entry| pattern_matches(entry, rel))
}

fn pattern_matches(pattern: &str, rel: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return rel == prefix || rel.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // Single path segment: rel must be prefix/<one-segment-with-no-further-slash>.
        return rel
            .strip_prefix(&format!("{prefix}/"))
            .is_some_and(|rest| !rest.contains('/'));
    }
    pattern == rel
}

#[test]
fn pattern_matcher_self_test() {
    // Sanity-check the matcher itself against the shapes it needs to handle, independent
    // of the actual filesystem contents — keeps a future edit to pattern_matches honest.
    assert!(matches_work_notice("crates/engram/src/lib.rs"));
    assert!(matches_work_notice("crates/engram/tests/integration.rs"));
    assert!(matches_work_notice("crates/engram/Cargo.toml"));
    assert!(matches_work_notice("crates/neuron-core/src/glyph.rs"));
    assert!(matches_work_notice("crates/neuron-core/src/logos.rs"));
    assert!(matches_work_notice("crates/neuron-core/src/gwyph.rs"));
    assert!(!matches_work_notice("crates/neuron-core/src/glyph2.rs"));
    assert!(!matches_work_notice("crates/neuron-core/src/dispatch.rs"));
    assert!(!matches_work_notice("crates/engram2/src/lib.rs"));
    assert!(!matches_work_notice("crates/neuron-app/src/main.rs"));
}
