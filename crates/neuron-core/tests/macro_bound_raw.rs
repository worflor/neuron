// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! End-to-end authority proof for the two Python macro domains. One test owns this integration-test
//! process so its process-global MacroHost, run-root override and macro-state override cannot race a
//! sibling test. No hardware is touched and input stays disarmed for the entire run.

use neuron::macros::{macro_host, Context, MacroHost};
use std::time::Duration;

/// Run a macro and wait for it to finish. Correctness checks use a completion budget, not the
/// interactive 2.5 s one, so a loaded machine slows them down instead of failing them.
fn invoke_done(host: &MacroHost, id: &str, ctx: &Context) -> String {
    host.invoke_with_budget(id, ctx, Duration::from_secs(30))
}

#[test]
fn bound_and_raw_are_distinct_authority_domains() {
    let tmp = std::env::temp_dir().join(format!(
        "neuron_bound_raw_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);
    std::env::set_var("NEURON_MACRO_STATE", tmp.join("state"));
    // A sitecustomize on PYTHONPATH runs at interpreter start, before neuron_host.py can seal
    // BOUND. It leaves a marker named after the sidecar's mode.
    let site = tmp.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(
        site.join("sitecustomize.py"),
        "import os\nopen(os.path.join(os.path.dirname(__file__), 'loaded-' + os.environ.get('NEURON_MACRO_MODE', '?')), 'w').close()\n",
    )
    .unwrap();
    std::env::set_var("PYTHONPATH", &site);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping bound/raw e2e: bundled python runtime did not materialize");
        std::env::remove_var("NEURON_MACRO_STATE");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }

    neuron::action::arm_input(false);
    host.set_armed(false);
    let ctx = Context::synthetic(
        Some("authority.exe".into()),
        None,
        Some(tmp.clone()),
        None,
        None,
    );

    // The first scan establishes the one-time migration marker while the directory is empty.
    // Everything registered after this point is genuinely new source and absence of a pragma means BOUND.
    assert!(neuron::macros::macro_host::scan_macro_dir().is_empty());

    // Check is a compiler-policy pass, not merely syntax. BOUND gets a precise RAW-required
    // diagnostic before any candidate executes; the same code is valid when source opts RAW.
    for (src, needle) in [
        ("def macro(ctx):\n    neuron.run('echo nope')\n", "neuron.run requires RAW mode"),
        ("def macro(ctx):\n    import os\n", "import 'os' requires RAW mode"),
        ("def macro(ctx):\n    open('nope.txt', 'w')\n", "open requires RAW mode"),
    ] {
        let err = host.check(src).expect_err("BOUND authority violation must fail compiler check");
        assert!(err.contains(needle), "policy diagnostic {err:?} did not contain {needle:?}");
        assert!(err.contains("line 2:"), "policy diagnostic should name the source line: {err}");
    }
    assert!(
        host.check("# neuron: raw\ndef macro(ctx):\n    neuron.run('echo allowed-when-armed')\n").is_ok(),
        "the exact RAW directive must make the full-Python source compiler-valid"
    );

    host.register(
        "raw_surface",
        r#"# neuron: raw
import os

def macro(ctx):
    p = ctx.cwd + "/raw_touch.txt"
    with open(p, "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))
    return "raw-ok"
"#,
    )
    .expect("register RAW surface probe");
    let raw = invoke_done(host, "raw_surface", &ctx);
    assert!(raw.contains("raw-ok"), "RAW lost ordinary Python power: {raw}");
    assert!(tmp.join("raw_touch.txt").exists(), "RAW file write did not land");

    // Same-domain synchronous invocation keeps native Python return values.
    host.register(
        "bound_num",
        "from collections import deque\ndef macro(ctx):\n    return deque([7])[0]\n",
    )
    .expect("register BOUND numeric callee with curated from-import");
    host.register(
        "bound_same_domain",
        "def macro(ctx):\n    return neuron.invoke('bound_num') + 1\n",
    )
    .expect("register BOUND same-domain caller");
    assert!(
        invoke_done(host, "bound_same_domain", &ctx).contains("8"),
        "same-domain BOUND invoke lost native Python return semantics"
    );

    host.register(
        "bound_target",
        "def macro(ctx):\n    return 'bound:%s:%s' % (neuron.option('x', '?'), ctx.app)\n",
    )
    .expect("register BOUND bridge target");
    host.register(
        "raw_calls_bound",
        "# neuron: raw\ndef macro(ctx):\n    return neuron.invoke('bound_target', x='X')\n",
    )
    .expect("register RAW caller");
    let down = invoke_done(host, "raw_calls_bound", &ctx);
    assert!(
        down.contains("bound:X:authority.exe"),
        "RAW -> BOUND did not execute in the target domain: {down}"
    );

    host.register(
        "raw_target",
        "# neuron: raw\ndef macro(ctx):\n    return 'raw-target'\n",
    )
    .expect("register RAW target");
    host.register(
        "bound_calls_raw",
        "def macro(ctx):\n    return repr(neuron.invoke('raw_target'))\n",
    )
    .expect("register BOUND caller");
    let up = invoke_done(host, "bound_calls_raw", &ctx);
    assert!(
        up.contains("None"),
        "BOUND -> RAW crossed the authority boundary: {up}"
    );

    // BOUND state is host-owned but file-compatible with RAW, so changing a macro's mode does not
    // silently fork or lose its persistent namespace.
    host.register(
        "state_bridge",
        "def macro(ctx):\n    n = neuron.load('n', 0) + 1\n    neuron.store('n', n)\n    return n\n",
    )
    .expect("register BOUND state macro");
    assert!(invoke_done(host, "state_bridge", &ctx).contains("1"));
    assert!(invoke_done(host, "state_bridge", &ctx).contains("2"));
    host.register(
        "state_bridge",
        "# neuron: raw\ndef macro(ctx):\n    return neuron.load('n', 0)\n",
    )
    .expect("switch state macro to RAW");
    assert!(
        invoke_done(host, "state_bridge", &ctx).contains("2"),
        "mode switch lost the macro's persistent state"
    );

    // A BOUND async invocation remains on the callee's serial worker and is observable through state.
    host.register(
        "bound_async_target",
        "def macro(ctx):\n    neuron.store('ran', 1)\n",
    )
    .expect("register BOUND async target");
    host.register(
        "raw_async_caller",
        "# neuron: raw\ndef macro(ctx):\n    neuron.invoke('bound_async_target', wait=False)\n    return 'queued'\n",
    )
    .expect("register RAW async caller");
    assert!(invoke_done(host, "raw_async_caller", &ctx).contains("queued"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state_path = tmp.join("state").join("bound_async_target.json");
        if std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("ran").and_then(serde_json::Value::as_i64))
            == Some(1)
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("cross-domain wait=False never reached the BOUND target");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Process isolation is structural too: killing RAW must not disturb the warm BOUND interpreter.
    host.register(
        "bound_survivor",
        "COUNT = [0]\ndef macro(ctx):\n    COUNT[0] += 1\n    return COUNT[0]\n",
    )
    .expect("register BOUND survivor");
    assert!(invoke_done(host, "bound_survivor", &ctx).contains("1"));
    host.register(
        "raw_crash",
        "# neuron: raw\nimport os\ndef macro(ctx):\n    os._exit(23)\n",
    )
    .expect("register RAW crash");
    let _ = host.fire_async("raw_crash", &ctx);
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        invoke_done(host, "bound_survivor", &ctx).contains("2"),
        "RAW process death reset or disturbed the BOUND interpreter"
    );

    // Every exported helper, OBS and signal included, resolves in both domains by attribute and by
    // from-import, and effect helpers share one result contract: a falsy "[reason]" when nothing
    // happened (disarmed, or refused by the host), a truthy result when it did.
    let surface = "from neuron import obs_scene, signal\n\
                   def macro(ctx):\n    \
                   return 'surface:%s:%s:%s' % (callable(obs_scene), callable(signal), callable(neuron.obs_connected))\n";
    let contract = "def macro(ctx):\n    \
                    out = []\n    \
                    for r in (neuron.key('f'), neuron.dpi(800), neuron.signal(9, 1.0)):\n        \
                    out.append('%s/%s' % (bool(r), str(r).startswith('[')))\n    \
                    done = neuron.signal(1, 0.25)\n    \
                    neuron.signal(1, 0.0)\n    \
                    out.append('%s/%s' % (bool(done), str(done).startswith('[')))\n    \
                    return 'contract:' + ','.join(out)\n";
    let obs_request = "def macro(ctx):\n    \
                       req = neuron.obs_request('GetVersion')\n    \
                       return 'obs_request=%s/%s' % (bool(req), req)\n";
    for (mode, header) in [("bound", ""), ("raw", "# neuron: raw\n")] {
        host.register(&format!("{mode}_helper_surface"), &format!("{header}{surface}"))
            .expect("register helper surface probe");
        let got = invoke_done(host, &format!("{mode}_helper_surface"), &ctx);
        assert!(got.contains("surface:True:True:True"), "{mode} lost exported helpers: {got}");
        host.register(&format!("{mode}_result_contract"), &format!("{header}{contract}"))
            .expect("register result contract probe");
        let got = invoke_done(host, &format!("{mode}_result_contract"), &ctx);
        assert!(
            got.contains("contract:False/True,False/True,False/True,True/False"),
            "{mode} effect results broke the contract: {got}"
        );
    }

    // The raw obs-websocket passthrough is RAW-only: BOUND refuses it at registration, and RAW
    // still meets the arm gate.
    let refused = host.register("bound_obs_request", obs_request)
        .expect_err("BOUND must refuse obs_request");
    assert!(refused.contains("requires RAW"), "{refused}");
    host.register("raw_obs_request", &format!("# neuron: raw\n{obs_request}"))
        .expect("register RAW obs_request probe");
    let got = invoke_done(host, "raw_obs_request", &ctx);
    assert!(got.contains("obs_request=False/[disarmed]"), "RAW obs_request ignored the arm gate: {got}");

    // String-driven reflection gets past the static policy and reaches the real builtins, so the
    // runtime wall must refuse every machine-touching operation that follows; RAW keeps them.
    // Each probe is a known way to turn a string into an attribute lookup, reach the sidecar's own
    // globals, and empty the runtime wall's deny set before touching the machine. None may get a
    // value back: the reflection primitive is withheld at import, and the wall's policy is not
    // reachable from Python even where one is found.
    let escapes = [
        ("bound_escape_attrgetter", r#"def macro(ctx):
    import operator
    ag = operator.attrgetter
    g = ag("__globals__")(neuron.key)
    ag("_BOUND_DENIED_EVENTS")(g["sys"].modules["__main__"]).clear()
    return "escaped"
"#),
        ("bound_escape_from_import", r#"from operator import attrgetter
def macro(ctx):
    return "escaped"
"#),
        ("bound_escape_formatter", r#"def macro(ctx):
    import re, string
    main = re.enum.sys.modules["__main__"]
    string.Formatter().get_field("0._BOUND_DENIED_EVENTS", (main,), {})[0].clear()
    return "escaped"
"#),
        ("bound_escape_submodule_import", r#"def macro(ctx):
    import re
    return "escaped:" + str(re.functools.sys)
"#),
    ];
    for (id, src) in escapes {
        match host.register(id, src) {
            Err(_) => {}
            Ok(_) => {
                let got = invoke_done(host, id, &ctx);
                assert!(!got.contains("escaped"), "{id} escaped the BOUND wall: {got}");
            }
        }
    }
    let proxied = "import re, operator\n\
                   def macro(ctx):\n    \
                   return 'proxy:%s:%s:%s' % (hasattr(re, 'sub'), hasattr(re, 'enum'), hasattr(operator, 'itemgetter'))\n";
    host.register("bound_proxy", proxied).expect("register BOUND proxy probe");
    let got = invoke_done(host, "bound_proxy", &ctx);
    assert!(got.contains("proxy:True:False:True"), "BOUND module surface is wrong: {got}");
    host.register("raw_reflection", "# neuron: raw\nimport operator\ndef macro(ctx):\n    return 'raw:' + str(operator.attrgetter('__name__')(operator))\n")
        .expect("register RAW reflection probe");
    let got = invoke_done(host, "raw_reflection", &ctx);
    assert!(got.contains("raw:operator"), "RAW lost reflection: {got}");

    // Ordinary computation the wall must leave alone: lazily loaded stdlib paths included.
    let everyday = "import datetime, random, json, re, statistics\n\
                    def macro(ctx):\n    \
                    year = datetime.datetime.strptime('2026-11-07', '%Y-%m-%d').year\n    \
                    bom = 'x'.encode('utf-8-sig').hex()\n    \
                    pick = random.Random(7).choice(['a', 'b'])\n    \
                    data = json.loads(json.dumps({'k': re.sub('a', 'b', 'aa')}))['k']\n    \
                    mid = statistics.median([1, 2, 3])\n    \
                    return 'everyday:%s:%s:%s:%s:%s' % (year, bom, pick in 'ab', data, mid)\n";
    host.register("bound_everyday", everyday).expect("register BOUND everyday probe");
    let got = invoke_done(host, "bound_everyday", &ctx);
    assert!(got.contains("everyday:2026:efbbbf78:True:bb:2"), "the wall broke ordinary BOUND code: {got}");

    assert!(site.join("loaded-raw").exists(), "the probe never ran, so it proves nothing");
    assert!(!site.join("loaded-bound").exists(), "BOUND ran code from PYTHONPATH before sealing");
    std::env::remove_var("PYTHONPATH");

    for id in [
        "bound_everyday",
        "bound_escape_attrgetter",
        "bound_escape_from_import",
        "bound_escape_formatter",
        "bound_escape_submodule_import",
        "bound_proxy",
        "raw_reflection",
        "raw_obs_request",
        "bound_helper_surface",
        "raw_helper_surface",
        "bound_result_contract",
        "raw_result_contract",
        "raw_surface",
        "bound_survivor",
        "raw_crash",
        "bound_num",
        "bound_same_domain",
        "bound_target",
        "raw_calls_bound",
        "raw_target",
        "bound_calls_raw",
        "state_bridge",
        "bound_async_target",
        "raw_async_caller",
    ] {
        host.unregister(id);
    }

    std::env::remove_var("NEURON_MACRO_STATE");
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
