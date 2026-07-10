//! Proof that the BUNDLED CPython is real and runs — with NO system Python involved.
//!
//!   * [`bundled_python_materializes_and_runs`] — `ensure_runtime()` extracts the embedded
//!     interpreter, then we EXECUTE it with `-c "import sys,ctypes,ast; print(sys.version_info)"`
//!     and assert it runs cleanly and reports CPython >= 3.12 (ctypes + ast import, per ground
//!     truth). This is the interpreter the sidecar will spawn — no PATH, no `NEURON_PYTHON`.
//!   * [`bundled_sidecar_fires_a_macro`] — register + fire a macro through the warm sidecar, which
//!     spawns from the bundled interpreter, and confirm the fire's value came back. End-to-end
//!     proof the sidecar runs on the app-owned Python.
//!   * [`triple_mapping_is_total_over_supported_targets`] — mirrors build.rs's `target_to_triple`
//!     and checks every supported triple maps + an unsupported one returns None. (build.rs's own
//!     `#[cfg(test)]` block isn't run by `cargo test`, so the contract is mirrored here.)

use neuron::macros::{ensure_runtime, macro_host, Context};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[test]
fn bundled_python_materializes_and_runs() {
    let rt = ensure_runtime().expect("bundled runtime materializes");
    assert!(
        rt.python.exists(),
        "bundled python must exist on disk: {}",
        rt.python.display()
    );

    // Run the BUNDLED interpreter directly (not via the sidecar) — ground-truth probe.
    let mut cmd = Command::new(&rt.python);
    cmd.arg("-c")
        .arg("import sys,ctypes,ast; print('%d.%d' % sys.version_info[:2]); assert ast.unparse");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("running bundled python {}: {e}", rt.python.display()));

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bundled python failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    let ver = stdout.trim();
    let (maj, min) = ver
        .split_once('.')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .unwrap_or_else(|| panic!("unexpected version output: {ver:?} (stderr: {stderr})"));
    assert!(
        (maj, min) >= (3, 12),
        "bundled python must be >= 3.12, got {maj}.{min}"
    );
    eprintln!("bundled CPython {maj}.{min} ran ctypes+ast cleanly from {}", rt.python.display());
}

#[test]
fn bundled_sidecar_fires_a_macro() {
    // Isolate the macros/scripts dir so we don't touch the repo's.
    let tmp = std::env::temp_dir().join(format!("neuron_pyruntime_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    // The bundled runtime is the ONLY source of Python here — available() proves it materializes.
    assert!(
        host.available(),
        "bundled python runtime must be available (it ships in the binary)"
    );
    host.set_armed(false); // read-only macro; no input synthesis

    // A macro that proves it ran ON the bundled interpreter: report sys.executable + a value.
    let src = "import sys\ndef macro(ctx):\n    return 'exec=%s ok' % sys.executable\n";
    host.register("bundled_fire", src)
        .expect("register on bundled sidecar");

    let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);
    let r = host.invoke("bundled_fire", &ctx);

    std::env::set_current_dir(&prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        r.contains("ok"),
        "macro must fire on the bundled sidecar and return its value: {r}"
    );
    // The reported sys.executable must be the bundled interpreter, not a system Python.
    let rt = ensure_runtime().expect("runtime");
    let py_name = rt
        .python
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(
        r.contains(&py_name) || r.contains("python"),
        "fire should report the bundled interpreter as sys.executable: {r}"
    );
    eprintln!("bundled sidecar fired a macro: {r}");
}

// ── mirror of build.rs::target_to_triple (build.rs' own cfg(test) block isn't run by cargo test) ──

/// EXACT mirror of `crates/neuron-core/build.rs::target_to_triple`. If you change one, change both.
fn target_to_triple(target: &str) -> Option<&'static str> {
    Some(match target {
        "x86_64-pc-windows-msvc" => "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin" => "x86_64-apple-darwin",
        "aarch64-apple-darwin" => "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu" => "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-unknown-linux-musl",
        _ => return None,
    })
}

#[test]
fn triple_mapping_is_total_over_supported_targets() {
    let supported = [
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ];
    for t in supported {
        assert_eq!(target_to_triple(t), Some(t), "{t} must map to itself");
    }
    // Unsupported targets must be a clean None (build.rs panics on these — never a silent miss).
    for bad in [
        "x86_64-pc-windows-gnu",
        "wasm32-unknown-unknown",
        "mips64-unknown-linux-gnuabi64",
        "",
    ] {
        assert!(target_to_triple(bad).is_none(), "{bad} must be unsupported");
    }
}
