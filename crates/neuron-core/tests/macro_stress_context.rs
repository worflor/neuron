//! NON-DESTRUCTIVE stress tests for the macro **Context** dimension of the "protocol" group.
//!
//! The Context is the world a macro reacts to: foreground app/title, the Explorer/terminal cwd, the
//! clipboard, the selection, and the window that had focus when the trigger fired. `capture()` reads
//! the live OS (read-only, never panics); `synthetic()` builds an adversarial world without touching
//! the OS; `ctx_json` (macro_host.rs) marshals a Context into the JSON the Python sidecar's `Ctx`
//! reads. This file hammers all of that — the pure-Rust struct contract, the OS-capture safety
//! guarantees, and the FULL marshalling round-trip through the real warm sidecar.
//!
//! ## Non-destructiveness contract (held by every test here)
//!   * The sidecar is always DISARMED (`set_armed(false)`) and every probe macro is strictly
//!     READ-ONLY — none call `key`/`type_text`/`run`/`click`/audio/device verbs — so NO real key
//!     presses, clicks, mouse moves, device writes, or audio changes can occur. No physical
//!     keyboard/mouse is ever required.
//!   * `capture()` and `restore_foreground()` are read-only OS queries. The one place a real Win32
//!     `SetForegroundWindow` runs (the "valid handle" path) only ever restores focus to the window
//!     that is ALREADY foreground — a genuine no-op — and is guarded behind `is_some()`.
//!   * The sidecar test isolates the macros/scripts dir into a private temp cwd (the same trick the
//!     other e2e files use) and removes it on teardown, so the user's real config is never touched.
//!   * Skips cleanly (not a failure) when no python runtime is resolvable — CI without python is fine.
//!
//! The sidecar-dependent cases are packed into ONE `#[test]` (`context_protocol_marshalling_via_sidecar`)
//! on purpose: they share the process-global cwd + the one warm sidecar, so running them as separate
//! parallel tests would race the cwd. The pure-Rust cases (no cwd change, no sidecar) are individual
//! parallel-safe tests.

use neuron::macros::context::WindowHandle;
use neuron::macros::{macro_host, Context, MacroHost};
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────────────────────
// PURE-RUST cases — no OS-state dependence beyond read-only capture; no sidecar; no cwd mutation.
// Safe to run in parallel.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// CASE context_synthetic_all_none_fields — a fully-None synthetic context is indistinguishable from
/// `default()`, every accessor returns None/false, and restore on the null handle is a hard false.
#[test]
fn context_synthetic_all_none_fields() {
    let synth = Context::synthetic(None, None, None, None, None);
    let def = Context::default();

    // default and all-None synthetic are indistinguishable on every accessor.
    assert_eq!(synth.app(), def.app());
    assert_eq!(synth.title(), def.title());
    assert_eq!(synth.cwd(), def.cwd());
    assert_eq!(synth.clipboard(), def.clipboard());
    assert_eq!(synth.selection(), def.selection());
    assert_eq!(synth.prev_window(), def.prev_window());

    // and each is the documented empty value.
    assert_eq!(synth.app(), None);
    assert_eq!(synth.title(), None);
    assert_eq!(synth.cwd(), None);
    assert_eq!(synth.clipboard(), None);
    assert_eq!(synth.selection(), None);
    assert!(!synth.prev_window().is_some(), "synthetic prev_window must be the null handle");
    assert!(!def.prev_window().is_some(), "default prev_window must be the null handle");

    // restore on a null handle is guaranteed false and makes NO Win32 call (is_some() guard).
    assert!(!synth.restore_foreground());
    assert!(!def.restore_foreground());
}

/// CASE context_window_handle_is_copy_and_zero_means_null — `WindowHandle` is `Copy`, 0 is the only
/// null value, every non-zero (incl. negative / extreme) is "some", and equality is value equality.
#[test]
fn context_window_handle_is_copy_and_zero_means_null() {
    // 0 is the ONLY null handle.
    assert!(!WindowHandle(0).is_some(), "0 must be the null handle");
    assert!(!WindowHandle::default().is_some(), "default must be the null handle");

    // every non-zero value — including 1, -1, large positive/negative, and the isize extremes — is
    // a "real" handle as far as is_some() is concerned (it only checks != 0).
    for v in [1isize, -1, 42, -42, 0x1234_5678, -0x1234_5678, isize::MAX, isize::MIN] {
        assert!(WindowHandle(v).is_some(), "{v} (non-zero) must read as a real handle");
    }

    // Copy: a value is still usable after being assigned elsewhere (no move-out).
    let h = WindowHandle(0xABCD);
    let h2 = h; // copy, not move
    assert_eq!(h.0, 0xABCD, "original handle is still usable after copy -> Copy, not move");
    assert_eq!(h2.0, 0xABCD);

    // value equality (derive(PartialEq, Eq)).
    assert_eq!(WindowHandle(7), WindowHandle(7));
    assert_ne!(WindowHandle(7), WindowHandle(8));
    assert_eq!(WindowHandle(0), WindowHandle::default());
}

/// CASE context_capture_does_not_panic_on_windows_and_non_windows — capture() is read-only and never
/// panics; fired many times in rapid succession it always yields a usable Context. selection is
/// hard-guarded to None on EVERY platform (capture must never clobber the clipboard).
#[test]
fn context_capture_is_panic_free_and_repeatable() {
    for i in 0..64 {
        let c = Context::capture();
        // selection is never probed by capture (it would require a clipboard-clobbering Ctrl+C).
        assert!(c.selection().is_none(), "capture must never set selection (fire {i})");
        // every other field may be Some or None depending on the live OS; just exercise them all —
        // the contract is "no panic, always a valid Context".
        let _ = c.app();
        let _ = c.title();
        let _ = c.cwd();
        let _ = c.clipboard();
        let _ = c.prev_window().is_some();
    }
}

/// CASE context_restore_foreground_null_handle_safety — restore on a null handle is a hard false with
/// no Win32 call; on an invalid non-null handle it fails GRACEFULLY (false, no panic); the real
/// Win32 path is exercised only as a no-op (restore focus to the already-foreground window).
#[test]
fn context_restore_foreground_handle_safety() {
    // null handle -> guaranteed false, no SetForegroundWindow call (is_some() guard short-circuits).
    assert!(!Context::default().restore_foreground());
    assert!(
        !Context::synthetic(Some("a".into()), None, None, None, None).restore_foreground(),
        "synthetic contexts carry a null prev_window -> restore is a no-op false"
    );

    // a non-null but INVALID handle must fail gracefully — never panic/segfault. Handle value 1 is
    // never a valid HWND (window handles are pointer-aligned), so on Windows SetForegroundWindow(1)
    // returns FALSE and changes no focus; on non-Windows it is a hard false stub.
    let bogus = Context { prev_window: WindowHandle(1), ..Default::default() };
    assert!(
        !bogus.restore_foreground(),
        "an invalid handle must fail gracefully (false), never panic"
    );

    // exercise the REAL Win32 path NON-DESTRUCTIVELY: restore focus to whatever is ALREADY
    // foreground (a true no-op). Guarded so headless/CI (null handle) just skips it. The point is
    // only that a valid-handle restore drives SetForegroundWindow and returns a bool without panic.
    let live = Context::capture();
    if live.prev_window().is_some() {
        let restore_self = Context { prev_window: live.prev_window(), ..Default::default() };
        let _ = restore_self.restore_foreground(); // bool either way; must not panic
    }
}

/// CASE context_selection_always_none_safe_guard — capture() NEVER probes the selection (which would
/// clobber the user's clipboard via a synthetic Ctrl+C); a synthetic selection still round-trips as a
/// plain field.
#[test]
fn context_selection_capture_never_probes_clipboard() {
    for i in 0..16 {
        assert!(
            Context::capture().selection().is_none(),
            "capture must hard-guard selection to None (fire {i}) — never a clipboard probe"
        );
    }
    // a synthetic selection is just a carried field; it has nothing to do with capture's guard.
    let s = Context::synthetic(None, None, None, None, Some("synthetic-sel".into()));
    assert_eq!(s.selection(), Some("synthetic-sel"));
}

/// CASE context_non_windows_stubs_return_none_or_false — on a non-Windows target every OS probe is a
/// stub: capture() yields an all-None context with a null prev_window, and restore on ANY handle is
/// false. (Compiles+runs only on non-Windows CI; the `cfg` keeps it correct there without affecting
/// the Windows dev build.)
#[cfg(not(windows))]
#[test]
fn context_non_windows_capture_is_all_none_and_restore_is_false() {
    let c = Context::capture();
    assert_eq!(c.app(), None, "non-Windows foreground_app stub is None");
    assert_eq!(c.title(), None, "non-Windows title stub is None");
    assert_eq!(c.cwd(), None, "non-Windows explorer_path stub is None");
    assert_eq!(c.clipboard(), None, "non-Windows clipboard stub is None");
    assert_eq!(c.selection(), None, "non-Windows selection stub is None");
    assert!(!c.prev_window().is_some(), "non-Windows foreground_window stub is the null handle");

    // restore is a hard false on non-Windows even for a (would-be) live handle.
    let live_ish = Context { prev_window: WindowHandle(0x1234_5678), ..Default::default() };
    assert!(!live_ish.restore_foreground(), "non-Windows restore_foreground stub is always false");
    assert!(!c.restore_foreground());
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// SIDECAR cases — the FULL marshalling + firing round-trip against the real warm python sidecar.
// Packed into one test (shared cwd + one warm sidecar). All macros are strictly READ-ONLY.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A macro that returns EVERY ctx field as one JSON object — the exact marshalling round-trip probe.
/// (single-quoted python keys so the Rust literal needs no double-quote escaping.)
const CTX_ECHO: &str = "import json\ndef macro(ctx):\n    return json.dumps({'app': ctx.app, 'title': ctx.title, 'cwd': ctx.cwd, 'clipboard': ctx.clipboard, 'selection': ctx.selection, 'prev_window': ctx.prev_window, 'armed': ctx.armed}, ensure_ascii=False)\n";

/// Isolate the macros/scripts dir into a private temp cwd; return (prev_cwd, tmp) or None to skip.
fn setup() -> Option<(PathBuf, PathBuf)> {
    let tmp = std::env::temp_dir().join(format!("neuron_macro_stress_ctx_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping context stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(&prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return None;
    }
    host.set_armed(false); // read-only probes; no input synthesis anywhere in this file
    Some((prev, tmp))
}

fn teardown(prev: PathBuf, tmp: PathBuf) {
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Fire `ctx_echo` synchronously and parse its returned JSON object (the marshalled context).
fn echo_fields(host: &MacroHost, ctx: &Context) -> serde_json::Value {
    let r = host.invoke("ctx_echo", ctx);
    let body = r
        .strip_prefix("macro 'ctx_echo': ")
        .unwrap_or_else(|| panic!("ctx_echo did not return a clean value (sidecar issue?): {r:?}"));
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("ctx_echo returned invalid JSON: {e}; raw value: {body:?}"))
}

/// Drain the macro log into an accumulator until `needle` appears `want` times or the budget expires.
fn wait_for_log_count(host: &MacroHost, needle: &str, want: usize, budget: Duration) -> Vec<String> {
    let deadline = Instant::now() + budget;
    let mut acc: Vec<String> = Vec::new();
    loop {
        acc.extend(host.drain_log());
        if acc.iter().filter(|l| l.contains(needle)).count() >= want {
            return acc;
        }
        if Instant::now() >= deadline {
            return acc;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A non-UTF-8 path: an unpaired UTF-16 surrogate on Windows / raw invalid bytes on Unix. `cwd`
/// marshals via `to_string_lossy()`, so this must come back with U+FFFD and never panic.
#[cfg(windows)]
fn non_utf8_pathbuf() -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    // "C:\" + an unpaired high surrogate 0xD800 + "\x" — valid WTF-8 OsString, NOT valid UTF-8.
    let wide: [u16; 6] = [0x0043, 0x003A, 0x005C, 0xD800, 0x005C, 0x0078];
    PathBuf::from(OsString::from_wide(&wide))
}
#[cfg(not(windows))]
fn non_utf8_pathbuf() -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(&[b'/', 0xFF, 0xFE, b'x']).to_os_string())
}

#[test]
fn context_protocol_marshalling_via_sidecar() {
    let Some((prev, tmp)) = setup() else {
        return;
    };
    let host = macro_host();

    host.register("ctx_echo", CTX_ECHO).expect("register ctx_echo");
    // warm-confirm: a synchronous invoke proves the sidecar is up before we rely on fire_async.
    let warm = echo_fields(host, &Context::synthetic(Some("warm.exe".into()), None, None, None, None));
    assert_eq!(warm["app"], "warm.exe", "sidecar must be warm and marshalling app");
    host.drain_log(); // clear warm/register noise

    // ── CASE context_synthetic_all_fields_populated: every field survives the round-trip ──
    {
        let ctx = Context::synthetic(
            Some("discord.exe".into()),
            Some("general — server".into()),
            Some(PathBuf::from("C:/work/repo")),
            Some("clipboard text".into()),
            Some("selected text".into()),
        );
        let j = echo_fields(host, &ctx);
        assert_eq!(j["app"], "discord.exe");
        assert_eq!(j["title"], "general — server");
        assert_eq!(j["cwd"], "C:/work/repo");
        assert_eq!(j["clipboard"], "clipboard text");
        assert_eq!(j["selection"], "selected text");
        assert_eq!(j["prev_window"], 0, "synthetic null handle marshals as 0");
        assert_eq!(j["armed"], false, "disarmed host marshals armed=false");
    }

    // ── CASE context_clipboard_empty_vs_null + context_cwd_none_vs_current_dir ──
    {
        // None clipboard / None cwd -> JSON null (a macro can test `is None`).
        let j = echo_fields(host, &Context::synthetic(Some("a".into()), None, None, None, None));
        assert!(j["clipboard"].is_null(), "None clipboard must marshal as JSON null: {j}");
        assert!(j["cwd"].is_null(), "None cwd must marshal as JSON null: {j}");
        assert!(j["selection"].is_null(), "None selection must marshal as JSON null: {j}");

        // EMPTY-STRING clipboard -> "" (NOT null): the '' vs None distinction survives marshalling.
        let j = echo_fields(
            host,
            &Context::synthetic(Some("a".into()), None, None, Some(String::new()), None),
        );
        assert!(!j["clipboard"].is_null(), "empty clipboard must NOT collapse to null");
        assert_eq!(j["clipboard"], "", "empty clipboard marshals as the empty string");

        // a present cwd -> a string distinct from null.
        let j = echo_fields(
            host,
            &Context::synthetic(None, None, Some(PathBuf::from("C:/some/dir")), None, None),
        );
        assert_eq!(j["cwd"], "C:/some/dir");
        assert!(j["app"].is_null(), "None app still marshals as null alongside a present cwd");
    }

    // ── CASE context_unicode_edge_cases_in_synthetic: emoji / RTL / combining / 10KB / null byte ──
    {
        let emoji = "🎮🕹️👾🔥";
        let rtl = "مرحبا بالعالم"; // arabic, right-to-left
        let combining = "e\u{0301}\u{0327}o\u{0308}"; // base + combining marks
        let long = "x".repeat(10_000);
        let nullbyte = "before\u{0}after"; // an embedded NUL must survive JSON escaping
        let ctx = Context::synthetic(
            Some(emoji.into()),
            Some(rtl.into()),
            Some(PathBuf::from(combining)),
            Some(long.clone()),
            Some(nullbyte.into()),
        );
        let j = echo_fields(host, &ctx);
        assert_eq!(j["app"], emoji, "emoji must round-trip byte-for-byte");
        assert_eq!(j["title"], rtl, "RTL text must round-trip stable");
        assert_eq!(j["cwd"], combining, "combining chars must survive the path marshalling");
        assert_eq!(
            j["clipboard"].as_str().map(str::len),
            Some(10_000),
            "a 10KB string must not be truncated"
        );
        assert_eq!(j["clipboard"], long.as_str());
        assert_eq!(j["selection"], nullbyte, "an embedded NUL must survive JSON escaping");
    }

    // ── CASE context_pathbuf_non_utf8_in_cwd: lossy marshalling, no panic ──
    {
        let bad = non_utf8_pathbuf();
        let expected = bad.to_string_lossy().into_owned(); // exactly what ctx_json marshals
        assert!(
            expected.contains('\u{FFFD}'),
            "the crafted path must actually be lossy (carry U+FFFD): {expected:?}"
        );
        let j = echo_fields(host, &Context::synthetic(Some("a".into()), None, Some(bad), None, None));
        assert_eq!(
            j["cwd"], expected.as_str(),
            "a non-UTF-8 cwd must marshal via to_string_lossy (replacement chars), never panic"
        );
    }

    // ── CASE context_json_marshalling_round_trip: prev_window marshals as an isize number ──
    {
        let ctx = Context { prev_window: WindowHandle(0x1234_5678), ..Default::default() };
        let j = echo_fields(host, &ctx);
        assert_eq!(
            j["prev_window"], 0x1234_5678i64,
            "a non-null prev_window must marshal as its isize value"
        );
        // a comprehensive mixed snapshot (nulls + values + a handle) is lossless in one shot.
        let ctx = Context {
            app: Some("mix.exe".into()),
            window_title: None,
            cwd: Some(PathBuf::from("C:/mixed/path")),
            clipboard: Some(String::new()),
            selection: Some("sel".into()),
            prev_window: WindowHandle(99),
        };
        let j = echo_fields(host, &ctx);
        assert_eq!(j["app"], "mix.exe");
        assert!(j["title"].is_null());
        assert_eq!(j["cwd"], "C:/mixed/path");
        assert_eq!(j["clipboard"], "");
        assert_eq!(j["selection"], "sel");
        assert_eq!(j["prev_window"], 99);
    }

    // ── CASE context_armed_flag_does_not_affect_capture: armed rides the frame, context is identical ──
    {
        let ctx = Context::synthetic(
            Some("armed_probe.exe".into()),
            Some("t".into()),
            Some(PathBuf::from("C:/x")),
            Some("c".into()),
            Some("s".into()),
        );
        // ctx_echo is read-only (no key/type/run), so toggling the global arm flag is non-destructive.
        host.set_armed(false);
        let j_off = echo_fields(host, &ctx);
        host.set_armed(true);
        let j_on = echo_fields(host, &ctx);
        host.set_armed(false); // restore disarmed immediately

        assert_eq!(j_off["armed"], false, "disarmed fire -> armed=false");
        assert_eq!(j_on["armed"], true, "armed fire -> armed=true");
        for f in ["app", "title", "cwd", "clipboard", "selection", "prev_window"] {
            assert_eq!(j_off[f], j_on[f], "context field {f:?} must be identical regardless of armed");
        }

        // fire_mock forces THIS fire disarmed (mock=true) even with the global flag armed.
        host.drain_log();
        host.register("ctx_arm", "import json\ndef macro(ctx):\n    print('CTXARM=' + json.dumps(ctx.armed))\n")
            .expect("register ctx_arm");
        host.set_armed(true);
        let kicked = host.fire_mock("ctx_arm", &ctx);
        host.set_armed(false);
        assert!(
            kicked.contains("dispatched") || kicked.contains("warming"),
            "mock fire must dispatch: {kicked}"
        );
        let lines = wait_for_log_count(host, "CTXARM=", 1, Duration::from_secs(8));
        assert!(
            lines.iter().any(|l| l.contains("CTXARM=false")),
            "fire_mock must force ctx.armed=false even when the host is globally armed: {lines:?}"
        );
    }

    // ── CASE context_macro_can_read_all_fields_via_prelude ──
    {
        host.drain_log();
        host.register(
            "ctx_readall",
            "def macro(ctx):\n    print('READALL app=%r title=%r cwd=%r clip=%r sel=%r prev=%r' % (ctx.app, ctx.title, ctx.cwd, ctx.clipboard, ctx.selection, ctx.prev_window))\n",
        )
        .expect("register ctx_readall");
        let ctx = Context {
            app: Some("read.exe".into()),
            window_title: Some("the title".into()),
            cwd: Some(PathBuf::from("C:/read/here")),
            clipboard: Some("clip!".into()),
            selection: Some("sel!".into()),
            prev_window: WindowHandle(4242),
        };
        // fire_async (not invoke): a fire's captured print() reaches the macro-log ring only for the
        // async path — a sync invoke delivers the result to its waiter and never pushes the captured
        // stdout to the ring. This is the realistic live-dispatch path anyway. (Disarmed; read-only.)
        let r = host.fire_async("ctx_readall", &ctx);
        assert!(r.contains("dispatched"), "ctx_readall must dispatch warm: {r}");
        let lines = wait_for_log_count(host, "READALL", 1, Duration::from_secs(8));
        let l = lines
            .iter()
            .find(|l| l.contains("READALL"))
            .unwrap_or_else(|| panic!("no READALL line in the macro log: {lines:?}"));
        assert!(l.contains("app='read.exe'"), "app field reachable: {l}");
        assert!(l.contains("title='the title'"), "title field reachable: {l}");
        assert!(l.contains("cwd='C:/read/here'"), "cwd field reachable: {l}");
        assert!(l.contains("clip='clip!'"), "clipboard field reachable: {l}");
        assert!(l.contains("sel='sel!'"), "selection field reachable: {l}");
        assert!(l.contains("prev=4242"), "prev_window field reachable: {l}");
    }

    // ── CASE context_concurrent_fires_independent_snapshots ──
    {
        host.drain_log();
        host.register("ctx_snap", "def macro(ctx):\n    print('SNAP:%s' % ctx.app)\n")
            .expect("register ctx_snap");
        const N: usize = 24;
        for i in 0..N {
            let c = Context::synthetic(Some(format!("snapapp{i:02}.exe")), None, None, None, None);
            let kicked = host.fire_async("ctx_snap", &c);
            assert!(kicked.contains("dispatched"), "fire {i} must dispatch warm: {kicked}");
        }
        let lines = wait_for_log_count(host, "SNAP:", N, Duration::from_secs(20));
        let snaps: Vec<&String> = lines.iter().filter(|l| l.contains("SNAP:")).collect();
        assert_eq!(snaps.len(), N, "every fire must log exactly once: {snaps:?}");
        // each fire saw ONLY its own ctx — every distinct app appears EXACTLY once (no cross-talk).
        for i in 0..N {
            let needle = format!("SNAP:snapapp{i:02}.exe");
            let count = snaps.iter().filter(|l| l.contains(&needle)).count();
            assert_eq!(count, 1, "app {i} must appear exactly once (independent snapshot): {snaps:?}");
        }
        // serial per-macro queue -> the snapshots also landed in firing order.
        for (i, line) in snaps.iter().enumerate() {
            assert!(
                line.contains(&format!("snapapp{i:02}.exe")),
                "fire {i} out of order (got {line}): {snaps:?}"
            );
        }
    }

    // ── CASE context_error_in_one_fire_does_not_corrupt_next_fire_context ──
    {
        host.register("ctx_boom", "def macro(ctx):\n    raise ValueError('intentional boom')\n")
            .expect("register ctx_boom");
        let ctx_a = Context::synthetic(Some("alpha.exe".into()), None, None, None, None);
        let ctx_c = Context::synthetic(Some("charlie.exe".into()), None, None, None, None);

        // A: normal fire sees its own context.
        assert_eq!(echo_fields(host, &ctx_a)["app"], "alpha.exe");
        // B: a deliberately raising fire surfaces an error (but must not poison the next fire).
        let err = host.invoke("ctx_boom", &ctx_a);
        assert!(err.contains("error"), "a raising macro must surface an error: {err}");
        // C: the very next fire sees ONLY its own context — no bleed from A or B's error state.
        assert_eq!(
            echo_fields(host, &ctx_c)["app"],
            "charlie.exe",
            "post-error fire must see only its own ctx — no cross-fire context bleed"
        );
        // and A's context still marshals cleanly after the error (the echo macro is uncorrupted).
        assert_eq!(echo_fields(host, &ctx_a)["app"], "alpha.exe");
    }

    for id in ["ctx_echo", "ctx_arm", "ctx_readall", "ctx_snap", "ctx_boom"] {
        host.unregister(id);
    }
    teardown(prev, tmp);
}
