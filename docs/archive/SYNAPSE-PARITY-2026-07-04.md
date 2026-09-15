> **archived snapshot — 2026-07-04. Not current.** Competitive research captured during
> the protocol-host R&D pass, split out of that doc because it is about Synapse parity,
> not about the protocol host. Some of its own entries were already corrected in place
> ("already exists in-tree") when it was written. Treat every ranking here as what was
> believed on that date. See [`README.md`](README.md).

# Synapse parity research (archived)

**Sourcing caveat:** reddit.com was firewalled to the research crawler the entire
time. Evidence is from Razer Insider forums, Tom's Hardware, PC Gamer, TechPowerUp,
RTINGS, GitHub/GitLab, and OSS-tool communities. Equivalent quality, not literal
Reddit threads.

###  What people LOVE (keep / match)
- Per-key/button remap that persists. **Hypershift** hold-layer (we already have
  multi-layer in `neuron-cli`; Synapse caps at ONE layer, hold-only, no lighting
  feedback → shipping toggle-mode + layer-lighting-feedback beats them outright).
- Macros with **real logic** — Synapse is dumb record-replay (no `if`, no clean
  hold-until-release, no repeat-while-held; degrades over runtime; S4 "forces
  minimum macro delay"). Our `MacroNode` tree (RepeatN/RepeatWhile/conditionals/
  cross-macro) already beats the Synapse+AutoHotkey combo people are forced into.
- DPI stages + OTF cycle, polling rate, lift-off distance. Snap Tap (we do it
  *better*: gated + sealed-consent; add per-game legality labels).
- Layered lighting (our Pattern×Spectrum ≈ Chroma Studio) + cross-device sync
  (`lighting mirror` ≈ Chroma's most-missed feature).
- Beloved effect vocabulary to cover: **Static, Breathing, Spectrum Cycling, Wave,
  Reactive, Ripple, Starlight**. (Fire/Starlight/Immersive = niche.)

###  Genuine feature gaps (ranked)
1. **Reactive / Ripple lighting** — input-driven; needs a live key-event feed into
   the compositor. In the "iconic Chroma" tier.
2. **Wireless battery QoL** — Synapse's *most-hated small feature*. What people
   actually want: **accurate tray readout** + **dismissible/threshold low-battery
   warning** (false-low-on-wake is the real hatred). Sleep/threshold sliders
   themselves are niche/rarely-touched — don't over-invest. Our own "vitals 1Hz
   wakes a sleeping mouse" bug is the same failure class — fix it.
3. **Audio-reactive lighting (visualizer)** — moderate-loved. **CORRECTION
   (lifecycle map): already exists in-tree** — `neuron-core/src/audio_level.rs`
   (~60Hz peak sampler, lock-free atomic, auto-stops ~2s unread) feeds a readout
   pattern. Gap is polish/exposure, not existence.
4. **Ambient/screen-mirror lighting** — niche but high-delight (SignalRGB's
   headline paid feature). **CORRECTION: also already exists** —
   `neuron-core/src/screen_ambient.rs` (~18Hz desktop grab → 22×6 zone grid,
   auto-stop) + readout pattern. Same: polish, not existence.
5. **Gaming Mode / Win-key lock** — table-stakes; ship with a visible on-state.
6. **DPI sniper/clutch button** — essential FPS; make it first-class.
7. **Rapid Trigger / adjustable actuation** — *the* top competitive-keyboard demand
   of 2024-26 (207+ pro CS2 players on Hall-effect). CONDITIONAL: our BlackWidow
   Chroma V2 isn't analog. Synapse's Rapid Trigger is **global (all-keys)** — so
   *per-key* actuation would beat them if we ever target analog/Hall-effect boards.

###  What people HATE (our architecture already answers)
Mandatory account/cloud login (root of the 2 worst bugs: destructive cloud-sync
wiping macros on S3→S4 migration, + slow startup). Bloat (~30% idle CPU, 160MB
"to run a mouse", 2-3min startup, "1GB bloatware"). Telemetry/AI-training clauses.
Always-on elevated service w/ world-writable dirs (= CVE-2021-44226). Updates that
break macros/profiles. Cloud-sync conflict UX. Lighting/macros dying on app close.

###  Market gap (why this is worth doing)
**No actively-maintained, cross-platform, full-feature (lighting + macros + DPI +
battery + profiles) Synapse replacement exists.** OpenRGB/SignalRGB = lighting-only,
still need Synapse for DPI/macros. OpenRazer *deliberately removed* macros. Every
Windows-native attempt (razer-ctl, Knife) is archived/dead. Live ones are all
single-OS or single-device-class. **That gap is exactly where neuron sits.**

###  Two late additions
- **macOS restoration**: Synapse 2.0 ran on Mac; S3 dropped it, never returned.
  Our cross-platform-by-traits Rust discipline makes Mac a "we restore what they
  took away" differentiator (user has a Mac collaborator).
- **Synapse macro XML import** = migration on-ramp ("I lost 200+ profiles" rage).

---
