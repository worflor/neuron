# neuron: state of the project

neuron is in **public beta mk1**: one person, one desk, building in the open, and until now the only eyes and hands on it were mine. this page is the honest map of where every part actually stands, so you know what to lean on and what to expect rough.

nothing here is a promise or a date. it's the current reality and the direction, updated as things move.

## the legend

- 🟢 **solid**: daily-driven, tested, reliable. lean on it.
- 🟡 **works, polishing**: usable every day, still sanding off quirks.
- 🟠 **rough / under-tested**: built, but incomplete or not exercised at scale. expect edges.
- 🔴 **barely started**: a scaffold exists, the behavior doesn't. don't rely on it.
- ⚪ **planned**: not built yet.

## the map

| area | state | where it's at |
|---|---|---|
| device control (dpi, polling, brightness, battery, reads, side-plate, scroll stage) | 🟢 solid | daily-driven, read-back verified. a few writes stay gated until a capture confirms them (see the [honesty table](../README.md#honesty-proven-gated-absent)). |
| trigger → action spine (binds, hypershift, app-focus) | 🟢 solid | the core. one engine, well-tested. |
| audio control (mute / gain / output-flip) | 🟢 solid | rides the OS mixer APIs. |
| synapse import / purge / device discover | 🟢 solid | import, purge, and discover all work. |
| notifications / confirmations | 🟢 solid | visual cards plus optional audio, per kind. |
| lighting engine | 🟡 works, polishing | recently rebuilt: pattern × spectrum, positionable data layers, auto-apply. good already. the live work is ironing out every quirk so it behaves exactly as expected, every time. |
| macros & beacons | 🟡 works, polishing | a warm CPython sidecar, ask/notify, a persistent key-value store, macro-calls-macro, the block builder. next up: proper docs and a cleaner API around the whole thing, plus closing a memory smell in the python bridge. |
| GUI rendering | 🟡 works, polishing | leans on the CPU today, and a few areas feel it (the overlay instruments especially). turning on GPU rendering there is a likely upgrade. |
| on-device memory / onboard storage | 🟠 partial | storage accounting and volatile writes work; full onboard profile-slot persistence and scroll feel-curves aren't done, so "push it all to the mouse and uninstall" isn't fully real yet. |
| spellweaving at scale (many rich weaves, a full radial menu) | 🟠 under-tested | the engine and the eigenmotion recognition are solid; heavy real-world use with a big glyph vault and a fully populated radial hasn't been lived-in. treat rich setups as experimental for now. |
| instruments (teleport, tether, whiteboard, glance, window verbs, dial, knockback, control) | 🟠 built, uneven | all present, maturity varies, under-tested. the whiteboard can't save its ink to disk yet. |
| system integration | 🟠 early | the control glance plus the neuron-host protocol hub (driving other apps and other-brand rigs through the OpenRGB / Chroma bridge). the pieces exist; the glue is thin. |
| momentary mic / push-to-talk | 🔴 barely started | the action shape is wired; the behavior isn't near done. treat it as absent. |
| cross-platform (linux / mac) | ⚪ planned | windows only today. the seams exist and compile as inert stubs; the backends aren't written. a great place to help, see [contributing](../README.md#contributing). |

## a bit more on the moving pieces

**lighting.** the engine is the part i'm proudest of and the part i'm still fussing over. the model is one thing now (a stack of pattern × spectrum layers, data readouts included), edits stream to the board live, and you can place any layer anywhere. what's left is polish: making every interaction land the way you'd expect, every time, with no surprises. it's good; i want it boring-reliable.

**macros & beacons.** the runtime is real and fast: a warm CPython sidecar in its own process, loaded once with imports already paid, so firing a macro is basically a function call, and a crashing macro can never take the app down. what's thin right now is the documentation and the shape of the API you write against. that's the next push: a proper reference, cleaner helpers, and closing out a memory smell in the python bridge.

**rendering.** most of the app is light, but rendering leans on the CPU today, and in a few spots (the overlay-heavy instruments) you can feel it. GPU rendering for those is a likely upgrade rather than a locked plan.

**momentary mic.** honest: i've barely touched this. the enum and the dispatch hook exist so the wiring is there, but it is not a finished feature. treat it as absent until this page says otherwise.

**on-device / onboard.** the dream is pushing everything to the mouse and uninstalling the software entirely. reads and volatile writes are there; the storage-chunk protocol for real onboard profile slots isn't done, so that dream isn't fully real yet.

**spellweaving at scale.** the hard part (recognising a drawn glyph by its eigenmotion, and running a radial off the same capture) works. what hasn't happened is someone living in it for weeks with a large vault of weaves and a fully populated radial. until that shakes out, rich spellweaving setups are experimental.

**system integration.** wiring neuron into the rest of your setup (other apps, other-brand devices through the OpenRGB / Chroma bridge, live system state) is early. the parts are there; the connective tissue is thin.

---

if a feature bites you and it's marked 🟠 or 🔴 above, that's expected and known, and this is the page to point at. if it's marked 🟢 and it breaks, that's a bug and i want to hear about it.
