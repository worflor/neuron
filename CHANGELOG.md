# changelog

## unreleased
more devices that just work, and lighting layers that help you out.
![the stack names Tidepool hidden under Wildlife and offers fixes, while the keyboard previews Wildlife at 50%](docs/media/release-layers-fix.webp)
![the fix kept: Wildlife at 50% over Tidepool, and the warning gone](docs/media/release-layers-kept.webp)

### new
- **plug in and go.** devices show up when you plug them in, no scan. one neuron can't learn says why.
- **Logitech.** mice and keyboards, plugged in or on a receiver, with their name, battery and DPI.
- **custom keyboards.** QMK boards with VIA turned on, brightness included.
- **layers that help.** neuron spots a layer hiding another and offers a fix you can preview.
- **check any device.** a button on its row shows what the device answers, ready to copy or send as a report.

### better
- **Razer keyboards.** more of them light up, and stay lit on animated effects. (#33, #23)
- **Razer mice.** the ones neuron learns light every LED, paced for their receiver.

### fixed
- **a crash.** opening the window after neuron had been running a while.
- **closing neuron.** your devices are left as neuron found them.
- **HyperPolling.** only shows a rate your device has confirmed.
- **small fixes.** in the colour picker, guides, the Python editor and lighting tiles.

### thanks
thanks to @kurakanai (#33) and @shinobun33t (#23) for their BlackWidow reports.

## 0.1.8 · 2026-11-14
the colour picker grew a brain: Luma-T, a real-time colour model that edits with you. neuron updates itself now, plus a pile of everyday fixes.
![picker v2 with Luma-T's first idea drafted on a stop, contour rings around where it fits](docs/media/picker-v2-suggest.webp)
![a fresh roll on show, with the roll drawer open](docs/media/picker-v2-roll.webp)
![Tidepool, neuron's new shallow-water effect, rendered on a keyboard](docs/media/release-v0.1.8-tidepool.webp)

### new
- **Luma-T, in the colour picker.** 663k parameters, running on your computer in real time, with no GPU or internet.
  - **suggestions.** colours for the stop you're editing, a change carried through the palette, new stops coloured in. hover to try one, click to keep it.
  - **ask and roll.** ask for the ideas it would keep to itself, or roll a whole palette around the stops you hold.
  - **its thinking, drawn.** a squiggle on a stop it would change, rings and hills where a colour would fit.
  - **your taste.** it learns from what you keep and pass over, on your machine only.
- **picker v2, now the default.** lightness and vividness on a plane, hue on a rail, in an even colour space. arrows nudge a stop, Tab takes an idea, `R` rolls. `v1` in the spectrum header brings the old one back.
- **Tidepool.** sunlit shallow water on your keys, rippling where you type.
- **neuron updates itself.** a pill in the header says when a signed release is out: click it for what's new, `& install` for the rest. the old version stays until the new one runs cleanly. also from the tray, and `neuron update` in a terminal.
- **Fn keys survive driver mode.** keyboards held for their macro keys keep their Fn combos and media keys, and there's a new `game-mode` action.
- **a tray and status bar that talk back.**
  - **the tray** says whether neuron's live and what everything is set to, with the hotkeys beside what they do.
  - **the status bar** undoes the last change, jumps to the rule that fired, and makes an app rule from the app in front.
- **report a bug** from the system page: neuron writes a report with your name and paths scrubbed and opens an issue for it, or copies a prompt for your agent.
- **see inside pockets and transforms.** click a pocket to read and copy it; paste a sample to watch a clipboard transform work step by step.
- **effect options preview themselves.**
- **neuron has a face.** a lowercase `n.`, in the tray, the taskbar and on the exe.

### better
- **system, sorted.** one page in the order you reach for things, with an index to jump around.
- **macros know where they're bound.** a macro's card shows what fires it, or binds it in one click.
- **add a layer above or below** the one you're on, straight from an effect's tile.
- **devices update live** when you adopt or edit one.
- **crisper UI.** thin lines stay sharp, and long names fade at the edge.
- **longer timeline holds.** `stay` goes to 30 s and `fade in` to 10 s.

### fixed
mostly input, found living with it every day.
- **binds on keys a device can't remap itself** fire again, and a key shared by several keyboards goes to the one you typed on.
- **HyperShift** keys type normally unless you're holding HyperShift.
- **a device asleep at startup** gets retried, and input arms once it wakes.
- **macros on a busy machine** get the time they need to arm.
- **quitting with a sleeping wireless Naga** is quick again.
- **closing a guide card** leaves your next click for what's under it.
- **shared device files** can't attach commands to keys a built-in definition doesn't have.

### p.s.
- **Tidepool** gets an honourable second mention, just cuz. use it, try it, it's cool. pls <3

## 0.1.7 · 2026-11-08
a hardening pass. i went through neuron trying to break it and fixed what fell out: macros and shared setups locked down, and the keyboards you reported now light up properly.

### new
- **a macro API reference,** in `docs/reference/MACROS.md`.
- **more for BOUND macros.** `signal` and the OBS helpers work in BOUND mode. `obs_request` stays RAW-only.

### better
- **BOUND macros live in a box.** file, process, network, native-library and new-module access is refused by every route i know of, and BOUND ignores `PYTHONPATH`, `PYTHONHOME` and user site packages.
- **imports ask before they run code.** `neuron apply` and `profile import` list and refuse RAW macros, command binds, path screenshots and onboard-memory profiles unless you pass `--allow-code`.
- **device files have limits.** a definition can only use commands neuron knows; anything refused or unreadable is reported.
- **tighter helpers.** the Chroma REST server only answers requests addressed to this computer, the updater checks its source and the installer's attestation, the bundled Python is pinned, and elevated helpers never load from folders you can write to.
- **broker updates can't strand you.** the Chroma broker stages and checks its replacement before stopping the old one, and puts it back if anything fails.
- **macro helpers answer clearly.** effect helpers return `True`, or something falsy with the reason, in both modes.

### fixed
- **BlackWidow V3 and V4 X lighting.** generated definitions now cover the whole board and the backlight. older files get rebuilt, with a backup (#33, #23).
- **M keys on a keyboard plugged in after neuron started** (#23).
- **sleep and Modern Standby** no longer set off false stall warnings.
- **quitting from the tray** no longer crashes on the way out.
- **macro odds and ends.** `datetime.strptime` works in BOUND macros, dry runs leave your live lighting alone, and a slow parse doesn't restart Python anymore.
- **`neuron apply`** rejects sections it doesn't know and says which one failed.

### thanks
thanks to @shinobun33t (#23) and @kurakanai (#32, #33) for trying neuron on their keyboards and telling me what broke.

## 0.1.6 · 2026-10-16
tagged but never published. everything here went out in 0.1.7.

### new
- **see every copy at once.** `neuron-update.ps1 -Action status` (`-Json` for agents) tells apart the build you're running, the installed copy, what starts with Windows and where the config lives.

### fixed
- **lighting ownership** survives a host restart, and single-LED OpenRGB updates work after a surface resizes.
- **macro keys** pick up a keyboard adopted mid-session, with held keys tracked across its sibling devices.
- **malformed replies** from Razer discovery and HID++ get rejected instead of trusted.
- **other copies stay put.** builds, installers and uninstall leave other copies' startup target and the shared Chroma broker alone, and never take your config with them.
- **updates you already have** skip the download and backup, and clean up after themselves.

## 0.1.5 · 2026-10-13
a much faster lighting page. then it kept going: layers, gradients, pairings, and a lab for hardware neuron doesn't know yet.
![lighting layers: an exploded engine render beside the real spectrum editor](artifacts/promo/release-v0.1.5/diagonal-screenshot-draft.png)

### new
- **lighting layers.** an ordered stack with visibility, duplicates, per-layer opacity and blends (normal, screen, add, multiply, cutout), and dissolves between looks.
- **gradient presets.** rainbow, fire, aurora, thermal, sunset, meadow, ice and bubble (a soap-film ramp from actual thin-film interference). one click swaps the colours and keeps your motion and blend.
- **suggested pairings.** a chip on each effect tile tries it with another gradient or mode. my favourites first, the rest scored.
- **Compatibility Lab.** HID inventory, cleaned-up evidence bundles and issue drafts for unknown hardware. input trials only run with your say-so.
- **palette names in their own colours,** with an "edited" tag once you change a stock one.

### better
- **a snappier editor.** simulation, thumbnails and saved effects run off the UI thread, and tiles you can't see go to sleep.
- **a tighter inspector,** with a colour-mix dropdown, reversible colour motion and inline property pickers.
- **Fire and Typing Heat** keep a dark foot on bright gradients, and Comet's light painting now lingers about 30 s.

### fixed
- **Ripple** keeps its waves when you type fast.
- **animation timing:** effect swaps, Comet retriggering, Heat on repeated timestamps and the breathing loop's seam.

## 0.1.4 · 2026-10-04
more intentional UI, general controllers, and a little wildlife on your keyboard.
![Wildlife: neuron's lighting rendered on a mock keyboard](docs/media/release-v0.1.4-wildlife.webp)

### new
- **controllers.** general controller input on Windows, radial casting with a stick, Xbox-protocol pad rumble, device badges and sensor reads.
- **Wildlife,** a seasonal lighting habitat that lives off your typing.
- **clipboard tools.** transforms, session history, screenshots, lighting-layer actions and session undo.
- **a CLI for everything.** `neuron <noun> <verb>` with `--json`, writes that check themselves, live reload of a running app, and your whole setup as one document.
- **Synapse 3 import,** from its mapping logs.

### better
- **binds and lighting** got grouped action picking, inline controls, filtered bind lists and device identity in the preview.
- **hardened by trying to break it:** controller identity, Naga side-plate binds, clipboard formats, screenshots and nested actions.

## 0.1.3 · 2026-09-29
games' lighting, picked apart. a game paints a lot onto your keys; now you can see what each part of it is.

### new
- **Chroma lab.** splits a game's lighting into ambient, held keys, animated keys and repeating effects, with replay, hiding and a colour lens.
- **game scenes and rules.** a heads-up when a flagged scene ends while the game's in the background, plus rules that fire a clip, OBS scene, profile or mute.
- **Game Light,** a layer that carries a Chroma game's colours to any device.

### better
- **native Chroma decoding,** rewritten per device type, with SDK preset effects and suspended games recognised.
- **one signal language** for colours and lamps. faders take typed values, reset and fine adjustment.

## 0.1.2 · 2026-09-26
quieter in the tray. neuron lives there, so it shouldn't be busy while nothing's happening.

### better
- **less idle work** in dispatch, capture, the tray, audio, gestures and previews.
- **renderer fallback** got smarter, and there's an opt-in WGPU renderer.
- **the Windows lifecycle.** autostart without admin, Chroma setup that sets itself up and recovers, and your 0.1.1 config carried forward.
- **safe mode** starts with input disarmed and device writes paused.

### fixed
- **hardware replies and writes,** hardened. intercepted keys let go properly and sniper DPI comes back on the right mouse.
- **profile updates** no longer trip over each other across processes.

## 0.1.1 · 2026-09-20
Windows beta. the updater finds beta releases, and the docs got shorter and truer.

### fixed
- **updates** find prereleases and download from private repositories.
- **the docs** for install, platforms, troubleshooting and macros.

## 0.1.0 · 2026-09-19
beta mk1. the first public one.

### new
- **neuron.** a Windows tray app and CLI for Razer device control, profiles, remaps, lighting, macros and audio. input stays disarmed until you arm it, and every device write is checked by reading it back.
