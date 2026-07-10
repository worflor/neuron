//! NON-DESTRUCTIVE stress tests for the INVOKE + STORE dimension of the macro system:
//!   * `neuron.invoke(...)` — cross-macro composition (the sidecar holds every macro as a live fn):
//!     synchronous return-value flow, fire-and-forget, the 16-deep cycle guard, missing targets,
//!     option passing, and per-thread mid/options save/restore down a chain (ctx shared, mid/options
//!     not).
//!   * `neuron.store/load/forget/stored` — the per-macro JSON KV store: basic persistence, per-macro
//!     namespace isolation, atomic-write integrity under a hammering loop, forget (single + all),
//!     stored()-returns-a-copy, big values, unicode, special-char keys, the NEURON_MACRO_STATE
//!     override, survival across a sidecar respawn, concurrent access, missing-key defaults, and
//!     graceful degradation from a corrupted file.
//!
//! ALL of it runs against the REAL python sidecar, DISARMED, with the macros dir isolated into a temp
//! cwd and the KV store isolated via NEURON_MACRO_STATE into a second temp dir (both removed after).
//! The Macro Host is a process-global singleton and the cwd/env are process-global, so — exactly like
//! the existing e2e tests — everything is ONE serial `#[test]` with sequential phases. The state-dir
//! env var is set BEFORE the first host call so the sidecar inherits it at spawn.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::macros::{macro_host, Context};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Path to a macro's on-disk KV store under the test's NEURON_MACRO_STATE dir. The macro ids used
/// here are all plain `[a-z0-9_]`, so the sidecar's filename sanitizer is an identity map on them.
fn store_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join(format!("{id}.json"))
}

/// Read + parse a macro's KV store; `None` if the file is absent or unparseable.
fn read_store(state_dir: &Path, id: &str) -> Option<Value> {
    let s = std::fs::read_to_string(store_path(state_dir, id)).ok()?;
    serde_json::from_str(&s).ok()
}

/// Serializes the cwd/NEURON_MACRO_STATE-mutating tests in this binary so they can't race each
/// other (cwd + env are process-global).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The ONE state dir BOTH tests in this binary must use. They share a process-global Macro Host
/// sidecar (a singleton), and the sidecar captures `NEURON_MACRO_STATE` at SPAWN and keeps it for
/// its whole life — so whichever test spawns the sidecar first fixes the store dir for BOTH. If they
/// disagreed, the second test's stores would land where the FIRST test's sidecar was told to look,
/// not where the second one reads (the exact failure when the bug repro was un-ignored). One shared,
/// process-stable dir (per pid, NOT per test) keeps the store isolated to our temp either way.
fn shared_state_dir() -> PathBuf {
    std::env::temp_dir().join(format!("neuron_invstore_state_{}", std::process::id()))
}

/// Accumulate macro-log lines (draining the ring) until `needle` appears or the deadline passes.
fn wait_log(needle: &str, dur: Duration) -> Vec<String> {
    let deadline = Instant::now() + dur;
    let mut acc = Vec::new();
    loop {
        acc.extend(macro_host().drain_log());
        if acc.iter().any(|l| l.contains(needle)) || Instant::now() >= deadline {
            return acc;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn invoke_store_stress_e2e() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let pid = std::process::id();
    let tmp = std::env::temp_dir().join(format!("neuron_invstore_{pid}"));
    let state = shared_state_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    // MUST be set before the sidecar spawns (it inherits the env at spawn): the KV store lands in our
    // private temp dir, never the user's real LOCALAPPDATA/XDG path.
    std::env::set_var("NEURON_MACRO_STATE", &state);

    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping invoke/store stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        std::env::remove_var("NEURON_MACRO_STATE");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&state);
        return;
    }
    host.set_armed(false); // DISARMED throughout.
    let ctx = Context::synthetic(Some("invstore.exe".into()), None, None, None, None);

    // ════════════════════════════════ INVOKE ════════════════════════════════

    // ── invoke_depth_guard_enforced_at_16 ──────────────────────────────────────────────────────
    // A chain lvl1 -> lvl2 -> … each invoking the next with wait=True. The cycle guard allows exactly
    // 16 NESTED invokes (lvl1 runs as the root at depth 0; the 16th invoke runs lvl17 at depth 16);
    // the 17th invoke (inside lvl17, calling the existing lvl18) is refused -> None. So the result
    // propagates back as 'stopped@17', and lvl18 — though registered — NEVER runs (its store marker is
    // never written), which is what isolates "depth guard" from "missing target".
    for k in 1..=17u32 {
        let src = format!(
            "def macro(ctx):\n    print('lvl {k}')\n    r = neuron.invoke('lvl{}', wait=True)\n    return r if r is not None else 'stopped@{k}'\n",
            k + 1
        );
        host.register(&format!("lvl{k}"), &src).unwrap_or_else(|e| panic!("register lvl{k}: {e}"));
    }
    // lvl18 EXISTS (so the stop is the depth guard, not a missing target) and would mark itself if run.
    host.register("lvl18", "def macro(ctx):\n    neuron.store('i_ran', 1)\n    return 'leaf18'\n")
        .expect("register lvl18");
    let r = host.invoke("lvl1", &ctx);
    assert!(
        r.contains("stopped@17"),
        "the cycle guard must refuse the 17th nested invoke (got: {r})"
    );
    assert!(
        !r.contains("leaf18"),
        "lvl18 must never run (if it had, lvl17 would return 'leaf18'): {r}"
    );
    assert!(
        read_store(&state, "lvl18").is_none(),
        "lvl18 never executed, so it never wrote its store marker"
    );

    // ── invoke_sync_with_return_value_flow ─────────────────────────────────────────────────────
    host.register("inv_b", "def macro(ctx):\n    return 'value-from-B'\n").unwrap();
    host.register(
        "inv_a",
        "def macro(ctx):\n    return 'A-saw:' + str(neuron.invoke('inv_b', wait=True))\n",
    )
    .unwrap();
    let r = host.invoke("inv_a", &ctx);
    assert!(r.contains("A-saw:value-from-B"), "invoke(wait=True) returns the callee's value: {r}");

    // ── invoke_async_fire_and_forget ───────────────────────────────────────────────────────────
    // invoke(wait=False) queues the callee on its OWN serial worker and returns None to the caller at
    // once. We verify BOTH halves through the STORE (a deterministic side-effect, immune to log
    // timing): the caller returns immediately with rv=None, and the child's store marker appears
    // shortly after on its own worker. (A wait=False child's stdout/traceback now ALSO reaches the
    // macro log — see `bug_invoke_wait_false_child_output_is_dropped` below, the once-#[ignore]d repro
    // that is now green — but the store stays the cleanest race-free proof the child executed.)
    host.register("inv_b2", "def macro(ctx):\n    neuron.store('ran', 1)\n").unwrap();
    host.register(
        "inv_a2",
        "def macro(ctx):\n    rv = neuron.invoke('inv_b2', wait=False)\n    neuron.store('rv_is_none', rv is None)\n    return 'A-done'\n",
    )
    .unwrap();
    assert!(host.invoke("inv_a2", &ctx).contains("A-done"), "the caller returns without blocking on the child");
    let a2 = read_store(&state, "inv_a2").expect("inv_a2 store");
    assert_eq!(a2.get("rv_is_none").and_then(Value::as_bool), Some(true), "wait=False returns None immediately");
    // the child ran on its own worker — its store marker appears shortly after, concurrently.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut b_ran = false;
    while Instant::now() < deadline {
        if read_store(&state, "inv_b2").and_then(|v| v.get("ran").and_then(Value::as_i64)) == Some(1) {
            b_ran = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(b_ran, "invoke(wait=False) executed the child on its own serial worker");

    // ── invoke_missing_target_error ────────────────────────────────────────────────────────────
    host.register(
        "inv_missing",
        "def macro(ctx):\n    return 'r=%r' % neuron.invoke('does_not_exist_xyz', wait=True)\n",
    )
    .unwrap();
    let r = host.invoke("inv_missing", &ctx);
    assert!(r.contains("r=None"), "invoking a non-existent macro returns None without raising: {r}");
    assert!(!r.contains("error"), "missing-target must NOT surface as a macro error: {r}");

    // ── invoke_options_passed_to_called_macro ──────────────────────────────────────────────────
    host.register(
        "inv_b3",
        "NEURON_OPTIONS = [\n    {'key': 'speed', 'type': 'choice', 'choices': ['slow','fast'], 'default': 'slow'},\n    {'key': 'verbose', 'type': 'bool', 'default': False},\n]\ndef macro(ctx):\n    return 'speed=%s verbose=%r' % (neuron.option('speed'), neuron.option('verbose'))\n",
    )
    .unwrap();
    host.register(
        "inv_a3",
        "def macro(ctx):\n    return neuron.invoke('inv_b3', wait=True, speed='fast', verbose=True)\n",
    )
    .unwrap();
    let r = host.invoke("inv_a3", &ctx);
    assert!(
        r.contains("speed=fast verbose=True"),
        "invoke kwargs become the callee's options: {r}"
    );

    // ── invoke_mid_and_options_restore_after_return ────────────────────────────────────────────
    // A -> B -> C. ctx is SHARED down the chain (C sees the original app). After C returns, B's
    // per-thread mid AND options are restored: B's `option('bopt')` reads the same value before and
    // after, and B's store AFTER the call still lands in B's namespace (mid restored) — not C's.
    host.register(
        "inv_c",
        "def macro(ctx):\n    neuron.store('cmark', 'C')\n    return 'app=%s z=%r' % (ctx.app, neuron.option('z'))\n",
    )
    .unwrap();
    host.register(
        "inv_b4",
        "def macro(ctx):\n    before = neuron.option('bopt')\n    neuron.store('pre', 'B')\n    c = neuron.invoke('inv_c', wait=True, z='zval')\n    neuron.store('post', 'B')\n    after = neuron.option('bopt')\n    return 'before=%r after=%r same=%s | %s' % (before, after, before == after, c)\n",
    )
    .unwrap();
    host.register(
        "inv_a4",
        "def macro(ctx):\n    return neuron.invoke('inv_b4', wait=True, bopt='bee')\n",
    )
    .unwrap();
    let r = host.invoke("inv_a4", &ctx);
    assert!(r.contains("same=True"), "B's options are restored after the nested call: {r}");
    assert!(r.contains("before='bee' after='bee'"), "B sees its OWN option both sides of the call: {r}");
    assert!(r.contains("app=invstore.exe"), "ctx is shared down the chain (C saw the original app): {r}");
    assert!(r.contains("z='zval'"), "C received the kwargs passed to it: {r}");
    // mid was switched to C and restored to B: B's pre+post land in B's store, C's mark in C's store.
    let b_store = read_store(&state, "inv_b4").expect("inv_b4 store exists");
    assert_eq!(b_store.get("pre").and_then(Value::as_str), Some("B"));
    assert_eq!(b_store.get("post").and_then(Value::as_str), Some("B"), "post-call store landed in B (mid restored)");
    assert!(b_store.get("cmark").is_none(), "C's mark must NOT leak into B's namespace");
    let c_store = read_store(&state, "inv_c").expect("inv_c store exists");
    assert_eq!(c_store.get("cmark").and_then(Value::as_str), Some("C"));
    assert!(c_store.get("pre").is_none() && c_store.get("post").is_none(), "B's marks must NOT leak into C");

    // ════════════════════════════════ STORE ════════════════════════════════

    // ── store_basic_load_cycle ─────────────────────────────────────────────────────────────────
    host.register(
        "store_basic",
        "def macro(ctx):\n    n = neuron.load('count', 0)\n    neuron.store('count', n + 1)\n    return 'count=%d' % neuron.load('count')\n",
    )
    .unwrap();
    assert!(host.invoke("store_basic", &ctx).contains("count=1"), "first fire stores 1");
    assert!(host.invoke("store_basic", &ctx).contains("count=2"), "second fire loads + increments -> persisted");
    assert!(host.invoke("store_basic", &ctx).contains("count=3"), "value survives across fires");

    // ── store_per_macro_isolation ──────────────────────────────────────────────────────────────
    host.register("store_iso_a", "def macro(ctx):\n    neuron.store('x', 'A')\n    return 'x=%r' % neuron.load('x')\n").unwrap();
    host.register("store_iso_b", "def macro(ctx):\n    neuron.store('x', 'B')\n    return 'x=%r' % neuron.load('x')\n").unwrap();
    assert!(host.invoke("store_iso_a", &ctx).contains("x='A'"));
    assert!(host.invoke("store_iso_b", &ctx).contains("x='B'"));
    assert!(host.invoke("store_iso_a", &ctx).contains("x='A'"), "each macro has an isolated namespace (A unaffected by B)");

    // ── store_atomic_write_safety ──────────────────────────────────────────────────────────────
    // Hammer 500 writes (cycling 26 keys) while a concurrent reader repeatedly opens + parses the
    // file. The atomic temp+os.replace means a reader can only ever see a COMPLETE prior-or-new file —
    // never a torn one. Any successful, non-empty read that fails to parse would be a real corruption.
    host.register(
        "store_atomic",
        "def macro(ctx):\n    for i in range(500):\n        neuron.store('k%d' % (i % 26), i)\n    print('atomic done %d' % len(neuron.stored()))\n",
    )
    .unwrap();
    host.drain_log();
    assert!(host.fire_async("store_atomic", &ctx).contains("dispatched"));
    let p = store_path(&state, "store_atomic");
    let mut reads = 0u32;
    let read_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < read_deadline {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if !s.trim().is_empty() {
                serde_json::from_str::<Value>(&s)
                    .unwrap_or_else(|e| panic!("torn/corrupt store observed mid-write: {e}\n{s}"));
                reads += 1;
            }
        }
        // also stop once the writer reports completion.
        if macro_host().drain_log().iter().any(|l| l.contains("atomic done 26")) {
            break;
        }
    }
    // ensure the writer is finished, then the final file is valid with all 26 keys.
    let _ = wait_log("atomic done 26", Duration::from_secs(5));
    let fin = read_store(&state, "store_atomic").expect("final atomic store parses");
    assert_eq!(fin.as_object().map(|m| m.len()), Some(26), "all 26 keys present, file intact");
    eprintln!("store atomicity: {reads} concurrent reads, every one parsed cleanly");

    // ── store_forget_single_key ────────────────────────────────────────────────────────────────
    host.register(
        "store_forget1",
        "def macro(ctx):\n    neuron.store('a', 1)\n    neuron.store('b', 2)\n    neuron.store('c', 3)\n    neuron.forget('b')\n    return 'a=%r b=%r c=%r' % (neuron.load('a'), neuron.load('b'), neuron.load('c'))\n",
    )
    .unwrap();
    assert!(host.invoke("store_forget1", &ctx).contains("a=1 b=None c=3"), "forget(key) drops only that key");

    // ── store_forget_all_keys ──────────────────────────────────────────────────────────────────
    host.register(
        "store_forgetall",
        "def macro(ctx):\n    neuron.store('a', 1)\n    neuron.store('b', 2)\n    neuron.forget()\n    return 'stored=%r' % neuron.stored()\n",
    )
    .unwrap();
    assert!(host.invoke("store_forgetall", &ctx).contains("stored={}"), "forget() wipes the whole store");
    assert!(
        !store_path(&state, "store_forgetall").exists(),
        "forget() with no key removes the store file entirely"
    );

    // ── store_stored_returns_copy ──────────────────────────────────────────────────────────────
    host.register(
        "store_copy",
        "def macro(ctx):\n    neuron.store('a', 1)\n    d = neuron.stored()\n    d['a'] = 999\n    d['b'] = 'injected'\n    return 'after=%r' % neuron.stored()\n",
    )
    .unwrap();
    let r = host.invoke("store_copy", &ctx);
    assert!(r.contains("'a': 1"), "stored() is a copy — mutating it doesn't touch the store: {r}");
    assert!(!r.contains("999") && !r.contains("injected"), "mutations to the returned dict never persist: {r}");

    // ── store_large_values_edge_case ───────────────────────────────────────────────────────────
    host.register(
        "store_large",
        "def macro(ctx):\n    big = 'x' * (1024 * 1024)\n    neuron.store('big', big)\n    v = neuron.load('big')\n    return 'len=%d match=%s' % (len(v), v == big)\n",
    )
    .unwrap();
    let r = host.invoke("store_large", &ctx);
    assert!(r.contains("len=1048576 match=True"), "a 1MB value round-trips without truncation: {r}");
    let large = read_store(&state, "store_large").expect("large store parses");
    assert_eq!(large.get("big").and_then(Value::as_str).map(str::len), Some(1024 * 1024), "the file holds the full 1MB value");

    // ── store_unicode_keys_and_values ──────────────────────────────────────────────────────────
    host.register(
        "store_unicode",
        "def macro(ctx):\n    k = '\u{1f511}\u{952e}'\n    v = 'na\u{ef}ve caf\u{e9} \u{65e5}\u{672c}\u{8a9e} \u{1f600}'\n    neuron.store(k, v)\n    return 'match=%s back=%s' % (neuron.load(k) == v, neuron.load(k))\n",
    )
    .unwrap();
    let r = host.invoke("store_unicode", &ctx);
    assert!(r.contains("match=True"), "unicode keys + values round-trip (no mojibake): {r}");
    assert!(r.contains("caf\u{e9}") && r.contains("\u{1f600}"), "the unicode value survives verbatim: {r}");

    // ── store_empty_and_special_char_keys ──────────────────────────────────────────────────────
    // Dict KEYS (unlike the macro-id filename) are stored raw in JSON, so empty/slash/colon/long keys
    // are all valid and round-trip — the path stays safe because only the macro ID names the file.
    host.register(
        "store_special",
        "def macro(ctx):\n    longk = 'L' * 300\n    neuron.store('', 'empty')\n    neuron.store('a/b\\\\c:d*?', 'special')\n    neuron.store(longk, 'long')\n    return 'e=%r s=%r l=%r' % (neuron.load(''), neuron.load('a/b\\\\c:d*?'), neuron.load(longk))\n",
    )
    .unwrap();
    let r = host.invoke("store_special", &ctx);
    assert!(r.contains("e='empty' s='special' l='long'"), "empty/special/long keys all round-trip: {r}");

    // ── store_neuron_macro_state_override ───────────────────────────────────────────────────────
    // The whole test runs under our NEURON_MACRO_STATE override; prove it's honored by finding the
    // macro's file PHYSICALLY in the override dir (never the default user data path).
    host.register("store_override", "def macro(ctx):\n    neuron.store('marker', 'here')\n    return 'ok'\n").unwrap();
    assert!(host.invoke("store_override", &ctx).contains("ok"));
    let ov = read_store(&state, "store_override").expect("override store file is in the NEURON_MACRO_STATE dir");
    assert_eq!(ov.get("marker").and_then(Value::as_str), Some("here"), "the override dir holds the data");

    // ── store_concurrent_access_multiple_macros ────────────────────────────────────────────────
    // Four DIFFERENT macros fire concurrently (distinct serial workers), each writing 50 keys into
    // its OWN namespace. The global store lock serializes I/O so no file tears; each file ends with
    // exactly its own 50 keys (no cross-macro leakage, no lost writes).
    for tag in ["P", "Q", "R", "S"] {
        let id = format!("store_conc_{}", tag.to_lowercase());
        let src = format!(
            "def macro(ctx):\n    for i in range(50):\n        neuron.store('key%d' % i, '{tag}-%d' % i)\n    print('{tag} conc done %d' % len(neuron.stored()))\n",
        );
        host.register(&id, &src).unwrap();
    }
    host.drain_log();
    for tag in ["p", "q", "r", "s"] {
        assert!(host.fire_async(&format!("store_conc_{tag}"), &ctx).contains("dispatched"));
    }
    let _ = wait_log("S conc done 50", Duration::from_secs(8));
    // give the slowest worker a moment, then verify every file independently.
    std::thread::sleep(Duration::from_millis(300));
    for (tag, low) in [("P", "p"), ("Q", "q"), ("R", "r"), ("S", "s")] {
        let m = read_store(&state, &format!("store_conc_{low}")).unwrap_or_else(|| panic!("store_conc_{low} parses"));
        let obj = m.as_object().unwrap_or_else(|| panic!("store_conc_{low} is an object"));
        assert_eq!(obj.len(), 50, "{tag}: all 50 concurrent writes landed");
        assert_eq!(obj.get("key0").and_then(Value::as_str), Some(format!("{tag}-0").as_str()), "{tag}: values carry its own tag (no leakage)");
    }

    // ── load_missing_key_returns_default ───────────────────────────────────────────────────────
    host.register("store_missing", "def macro(ctx):\n    return 'v=%r' % neuron.load('never_set_key', 'DEFAULT')\n").unwrap();
    assert!(host.invoke("store_missing", &ctx).contains("v='DEFAULT'"), "load(missing) returns the default");

    // ── load_from_corrupted_json_gracefully_fails ──────────────────────────────────────────────
    // Hand a macro a corrupt store file. load()/stored() must degrade to default/{} (never crash),
    // and the next store() must HEAL the file back to valid JSON.
    std::fs::write(store_path(&state, "store_corrupt"), b"{ this is not valid json :: ").unwrap();
    host.register(
        "store_corrupt",
        "def macro(ctx):\n    a = neuron.load('x', 'DEF')\n    b = neuron.stored()\n    neuron.store('healed', 1)\n    return 'x=%r stored_was=%r' % (a, b)\n",
    )
    .unwrap();
    let r = host.invoke("store_corrupt", &ctx);
    assert!(r.contains("x='DEF'"), "load from corrupt JSON degrades to the default: {r}");
    assert!(r.contains("stored_was={}"), "stored() from corrupt JSON degrades to {{}}: {r}");
    let healed = read_store(&state, "store_corrupt").expect("the corrupt file was healed to valid JSON");
    assert_eq!(healed.get("healed").and_then(Value::as_i64), Some(1), "a later store heals the file");

    // ── store_persistence_across_sidecar_respawn ───────────────────────────────────────────────
    // Store a value, CRASH the firewalled sidecar, then read back: the host respawns + re-registers
    // transparently and the on-disk store is untouched, so the value survives. ONE deliberate crash,
    // well under the breaker ceiling. (Kept LAST so the crash can't disturb earlier phases.)
    host.register(
        "store_persist",
        "def macro(ctx):\n    cur = neuron.load('survivor', None)\n    if cur is None:\n        neuron.store('survivor', 42)\n        return 'stored'\n    return 'loaded=%r' % cur\n",
    )
    .unwrap();
    assert!(host.invoke("store_persist", &ctx).contains("stored"), "first fire stores the survivor");
    // crash the sidecar (its whole reason to exist is to contain this).
    host.register("store_kill", "import os\ndef macro(ctx):\n    os._exit(1)\n").unwrap();
    let _ = host.fire_async("store_kill", &ctx);
    // the next call respawns + re-registers; the store on disk is intact -> the value is still there.
    let mut loaded = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let r = host.invoke("store_persist", &ctx);
        if r.contains("loaded=42") {
            loaded = r;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(loaded.contains("loaded=42"), "the stored value survived the sidecar respawn (got: {loaded:?})");

    // ── teardown ───────────────────────────────────────────────────────────────────────────────
    let mut ids: Vec<String> = (1..=18).map(|k| format!("lvl{k}")).collect();
    ids.extend(
        [
            "inv_a", "inv_b", "inv_a2", "inv_b2", "inv_missing", "inv_a3", "inv_b3", "inv_a4",
            "inv_b4", "inv_c", "store_basic", "store_iso_a", "store_iso_b", "store_atomic",
            "store_forget1", "store_forgetall", "store_copy", "store_large", "store_unicode",
            "store_special", "store_override", "store_conc_p", "store_conc_q", "store_conc_r",
            "store_conc_s", "store_missing", "store_corrupt", "store_persist", "store_kill",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    for id in ids {
        host.unregister(&id);
    }
    std::env::set_current_dir(prev).ok();
    std::env::remove_var("NEURON_MACRO_STATE");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&state);
}

/// REGRESSION GUARD for a fixed bug — asserts the CORRECT behavior, runs in the default suite.
///
/// A macro launched fire-and-forget via `neuron.invoke(name, wait=False)` DOES run, but its stdout
/// AND its tracebacks USED to be SILENTLY DROPPED — they never reached the macro-log ring, so a
/// fire-and-forget child that crashed failed INVISIBLY (no error line, nothing to debug from).
///
/// Root cause: `neuron.invoke(wait=False)` dispatches the child fire with `rid: None`
/// (runtime/host/neuron.py `invoke`), and the host reader's result handler
/// (macro_host.rs `reader_loop`, the `Some("result")` arm) gated ALL log/error surfacing behind
/// `if let Some(rid) = v.get("rid").and_then(Value::as_u64)`, so a null-rid result was discarded
/// before the "no waiter -> surface to the log" branch was reached. A top-level `fire_async` (which
/// uses a NUMERIC rid with no waiter) logged correctly — only the wait=False sub-fire was a black hole.
///
/// Fix: `reader_loop` now treats ANY `result` frame with no matching waiter (null rid OR a numeric
/// rid nobody is waiting on) as "surface to the macro log", and surfaces the FULL traceback (not just
/// its first banner line). So an async-invoked child's crash now reaches the log like any other fire.
#[test]
fn bug_invoke_wait_false_child_output_is_dropped() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let pid = std::process::id();
    let tmp = std::env::temp_dir().join(format!("neuron_invbug_{pid}"));
    // The SAME store dir the main e2e uses — see `shared_state_dir`: the singleton sidecar bakes in
    // whichever path is set when it spawns, so the two tests must agree (this repro doesn't store, but
    // it may be the test that spawns the sidecar first, fixing the dir for the e2e that runs after).
    let stt = shared_state_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&stt).unwrap();
    std::env::set_var("NEURON_MACRO_STATE", &stt);
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        std::env::set_current_dir(prev).ok();
        std::env::remove_var("NEURON_MACRO_STATE");
        return;
    }
    host.set_armed(false);
    let ctx = Context::synthetic(Some("invbug.exe".into()), None, None, None, None);

    // a fire-and-forget child that RAISES — its traceback MUST surface to the macro log (the same way
    // a top-level fire_async's error does). The parent returns normally; the crash must not be silent.
    host.register("bug_child", "def macro(ctx):\n    raise ValueError('boom-from-async-child')\n").unwrap();
    host.register(
        "bug_parent",
        "def macro(ctx):\n    neuron.invoke('bug_child', wait=False)\n    return 'parent-ok'\n",
    )
    .unwrap();
    host.drain_log();
    let _ = host.invoke("bug_parent", &ctx);
    let log = wait_log("boom-from-async-child", Duration::from_secs(5));

    host.unregister("bug_child");
    host.unregister("bug_parent");
    std::env::set_current_dir(prev).ok();
    std::env::remove_var("NEURON_MACRO_STATE");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&stt);

    assert!(
        log.iter().any(|l| l.contains("boom-from-async-child")),
        "CORRECT behavior: a fire-and-forget invoked child's traceback must reach the macro log; \
         it is currently dropped (the host reader discards the child's null-rid result). log={log:?}"
    );
}
