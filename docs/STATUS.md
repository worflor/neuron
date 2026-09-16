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
| device control (dpi, polling, brightness, battery, reads, scroll stage) | 🟢 solid | daily-driven, read-back verified. a few writes stay gated until a capture confirms them (see the [honesty table](../README.md#honesty-proven-gated-absent)). |
| side plate (naga swappable plates) | 🟢 solid | the mouse pushes a report on every swap; neuron debounces the seating bounce, tells a real swap apart from the device re-announcing its state, names the plate from registry data — and **binds now scope to the seated plate**. a plate is a latched context layer, so `plate:12-button` binds are live exactly while that plate is on the mouse, swapping displaces them rather than stacking, and detaching clears them. a held hypershift layer still outranks a plate layer: what you're doing beats what's merely true. |
| trigger → action spine (binds, hypershift, app-focus) | 🟢 solid | the core. one engine, well-tested. |
| the arm gate (observe · device · input · live) | 🟢 solid | the trust model under everything else: you pick whether neuron may read, write to the device, synthesize input, or all of it — and writes-paused blocks every write, from the GUI and from bound actions alike. does what you said, nothing else. |
| audio control (mute / gain / output-flip) | 🟢 solid | rides the OS mixer APIs. |
| synapse import / purge / device discover | 🟢 solid | import, purge, and discover all work. |
| profiles (capture · apply · rename · auto-switch by app) | 🟡 works, polishing | a profile is one object: settings, lighting, and binds, which are live only while it's active. auto-switch reads rules top to bottom, first match wins, with an optional fallback so closing a game puts you back. the profile you're on survives a restart. it just got a proper pass, so treat the edges as freshly-fixed rather than long-proven. |
| notifications / confirmations | 🟢 solid | visual cards plus optional audio, per kind. |
| device support (capability-driven) | 🟡 works, polishing | you plug a device in and it adopts itself — neuron reads the capabilities off the hardware instead of hardcoding them per model, so the UI only ever shows what your device actually has. any `razer_report` device should adopt itself this way (two are hardware-verified: the Naga V2 Pro and the BlackWidow Chroma V2); other families are a per-family seam — fill the seam once and the whole family works. honest limit: i only own razer gear, so razer is all *i* can actually test. |
| lighting engine | 🟡 works, polishing | recently rebuilt: pattern × spectrum, positionable data layers, auto-apply. good already. the live work is ironing out every quirk so it behaves exactly as expected, every time. |
| macros & beacons | 🟡 works, polishing | a warm CPython sidecar, ask/notify, a persistent key-value store, macro-calls-macro, the block builder. next up: proper docs and a cleaner API around the whole thing, plus closing a memory smell in the python bridge. |
| GUI rendering | 🟡 works, polishing | the main UI is on the GPU now (femtovg, software fallback so it still opens on a VM or bad-driver box). the overlay instruments (weaves, teleport, whiteboard, curtain) are the exception: layered click-through windows composited pixel-by-pixel on the CPU, and that's the part you can still feel. moving those to the GPU is real work, not a switch. |
| UI look & feel | 🟡 works, polishing | everything's legible and does what it says. the design pass has started: the lighting page was rebuilt around what feeds each effect, every page now holds a readable measure instead of stretching to whatever your monitor is, and the copy got rewritten to sound like a person. the other pages haven't had that same structural attention yet, and the overlay instruments have had none. |
| reliability & recovery | 🟡 built, unproven | a flight recorder and crash log turn silent failures into visible state, and the diagnostics bench proves the app's health read-only, on demand. on a crash or hang, windows can relaunch it ("phoenix") — honest caveat: it hasn't crashed on me yet, so that auto-restart has genuinely never had to fire. wired, not battle-tested. |
| on-device memory / onboard storage | 🟠 partial | storage accounting and volatile writes work; full onboard profile-slot persistence and scroll feel-curves aren't done, so "push it all to the mouse and uninstall" isn't fully real yet. |
| spellweaving at scale (many rich weaves, a full radial menu) | 🟠 under-tested | the engine and the eigenmotion recognition are solid; heavy real-world use with a big glyph vault and a fully populated radial hasn't been lived-in. treat rich setups as experimental for now. |
| instruments (teleport, tether, whiteboard, glance, window verbs, dial, knockback, control) | 🟠 built, uneven | all present, maturity varies, under-tested. the whiteboard can't save its ink to disk yet. |
| the CLI (`neuron-cli`) | 🟠 under-tested | a full command-line surface — device control plus a headless daemon run path — running the same engine as the GUI. works in theory, but i've been building neuron more than driving it from the terminal, so expect a few duh-obvious bugs that just haven't been hit yet. |
| integrations, on your terms | 🟠 early | games and apps can push into your lighting through the neuron-host hub (it speaks Chroma and OpenRGB). the point was never that it speaks them — it's that *you* decide what they're allowed to do to your base, how it blends, and that off stays off for real, unlike the thing this replaces. the pipes work; the control surface and the clean-teardown guarantees are what i'm still hardening. |
| chroma interpretation & dynamic redraw | ⚪ planned | the next step for integrations: richer ways to *interpret* what a game or app feeds in, and redrawing the board dynamically off that input instead of just passing it through. i know what i want here; none of it is in the code yet. |
| momentary mic / push-to-talk | 🟠 built, under-tested | wired end-to-end (press holds the mute, release restores it) with teardown guards so a held mic never strands across a config swap, respawn, or exit. light real-world mileage. |
| cross-platform: linux | 🟠 built, hardware-unverified | the whole workspace compiles on linux and the **entire test suite passes there** — CI runs it on every push, same script as windows. a hidraw transport is written against the kernel's own interfaces (sysfs enumeration, `HIDIOCGFEATURE`/`HIDIOCSFEATURE`, `flock` for the wire lock), so device control and the CLI should work, and releases ship a linux CLI tarball with a udev rule. the honest gap: no razer device has been plugged into a linux box running it yet — every linux test is against mocks and parsed descriptors, not a wire. treat the first run on your desk as the real test, and [tell me what happened](https://github.com/worflor/neuron/issues). |
| cross-platform: the linux GUI | ⚪ planned | the app crate builds on linux, but the tray, the overlays, input synthesis and audio are inert stubs there, so it would open a window that does nothing. that's why the linux release ships the CLI only. a great place to help, see [contributing](../README.md#contributing). |
| cross-platform: mac | ⚪ planned | nothing written, no hardware to write it on. the seams the linux port used (transport, input, audio, surfaces) are the same ones an IOKit backend would fill. |

## a bit more on the moving pieces

**lighting.** the engine is the part i'm proudest of and the part i'm still fussing over. the model is one thing now (a stack of pattern × spectrum layers, data readouts included), edits stream to the board live, and you can place any layer anywhere. one honest asterisk: the data-readout layers (the ones that paint from live system state) are built but under-exercised — i haven't lived with them long enough to vouch for them the way i can the rest. what's left is polish: making every interaction land the way you'd expect, every time, with no surprises. the layering itself — stacking, nudging, blending layers — has a pass of tactile feel/ux work coming so it's pleasant to work in, not just correct. it's good; i want it boring-reliable.

**macros & beacons.** the runtime is real and fast: a warm CPython sidecar in its own process, loaded once with imports already paid, so firing a macro is basically a function call, and a crashing macro can never take the app down. what's thin right now is the documentation and the shape of the API you write against. that's the next push: a proper reference, cleaner helpers, and closing out a memory smell in the python bridge.

**rendering.** the main UI runs on the GPU now (femtovg, with a software fallback so it opens anywhere). the overlay instruments are the holdout: they're layered click-through windows painted pixel-by-pixel on the CPU, and in the overlay-heavy ones you can feel it. moving those to the GPU is real work, not a switch.

**momentary mic.** wired end-to-end: the dispatch edge loop holds the mute while the trigger is down and restores it on release, with release-all teardown nets on config swap, worker respawn, and exit so a held mic can never get stranded. honest caveat: it's had light real-world mileage, so treat the edges as under-tested rather than absent.

**on-device / onboard.** the dream is pushing everything to the mouse and uninstalling the software entirely. reads and volatile writes are there; the storage-chunk protocol for real onboard profile slots isn't done, so that dream isn't fully real yet.

**spellweaving at scale.** the hard part (recognising a drawn glyph by its eigenmotion, and running a radial off the same capture) works. what hasn't happened is someone living in it for weeks with a large vault of weaves and a fully populated radial. until that shakes out, rich spellweaving setups are experimental.

**integrations.** letting games, apps, and other-brand gear touch your lighting is the easy half. the half that matters is that you stay in charge of it — what they can do to your base, how it blends, and being able to cut it off and trust it's actually cut off. that guarantee is the whole reason this exists; chroma the protocol is just plumbing i had to build to get there (half of it started as a "can i even do this" gag). the pipes work; the control and teardown are what i'm hardening. going further — really *interpreting* that input and redrawing the board off it — is planned and unwritten.

---

if a feature bites you and it's marked 🟠 or 🔴 above, that's expected and known, and this is the page to point at. if it's marked 🟢 and it breaks, that's a bug and i want to hear about it.
