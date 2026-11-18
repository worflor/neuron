# neuron FAQ

the everyday questions: installing, using it, and getting help. no coding needed for the main app.

## getting started

### how do i install it?

on Windows 10/11, [download the latest release](https://github.com/worflor/neuron/releases/latest). use the setup `.exe` for a normal install, or extract the portable ZIP and launch `neuron-app.exe`. the installer adds a Start menu shortcut; the portable version runs from its folder.

the builds aren't code-signed, so SmartScreen may ask before the first run: click **More info → Run anyway**. each release carries `SHA256SUMS.txt` and `SOURCE.txt` if you want to check what you downloaded. native Chroma setup asks for admin approval once; neuron itself runs without elevation.

### where do i change my settings?

open the app from Start or the tray. **DEVICE** is for DPI, polling and hardware settings; **LIGHTING** is for effects, gradients and layers; **INPUT** is for buttons, binds and gestures; **SYSTEM** has diagnostics and safety controls. profiles open from the header and can switch with the app in front.

there's a [command-line reference](reference/CLI.md) if you'd rather use a terminal. the app is enough for everyday use.

### do i have to keep Synapse installed?

no. i don't run Synapse on my own Naga V2 Pro or BlackWidow Chroma V2 anymore. on other devices, [check the status page](STATUS.md) for what's been tested before you remove your setup. Synapse and neuron shouldn't control the same device at the same time.

neuron can import supported plaintext Synapse exports. encrypted cloud-only profiles stay with Synapse. see [migration details](GDD.md#life-after-synapse).

### do i need Razer gear?

not for a lot of it. gamepads, spellweaving and the radial, app-aware profile switching, clipboard pockets and transforms, window, audio and screenshot actions, and macros run on ordinary Windows input, so they don't care what brand your gear is. device settings like DPI and polling, and per-key lighting, need supported Razer hardware. Logitech mice and keyboards read their name, battery and DPI, and QMK boards with VIA get brightness.

### does it work with anti-cheat?

neuron itself won't get you banned. it has no kernel driver, injects nothing into games and reads no game memory. it listens to your input through Windows and talks to your devices the same way Synapse does.

what you do with it is on you. a bind or macro that plays the game for you is cheating, whatever app sends it. snap tap is a hardware feature on keyboards that support it, and whether it's allowed is up to each game's anti-cheat; the app says so where you turn it on.

### can i remap buttons and make macros without writing code?

yes. use **INPUT** to connect a button or gesture to an action. HyperShift and app profiles are there where your device supports them. the macro workshop has a visual blocks editor, and custom Python macros are there when you want more. see [macros and beacons](GDD.md#macros--beacons).

### can my lighting react to music?

yes. the **Audio Meter** effect follows whatever's playing: brightness tracks loudness and colour tracks tone. **Typing Heat**, **Ripple** and **Tidepool** react to your keys, and **Pulse** shows your CPU and memory load.

### how do i update it?

from 0.1.8 on, neuron does it for you. when a newer release is out, a small pill shows up in the header and a line in the tray: click `& install` (or the tray line) and it downloads, checks, installs and restarts, keeping your settings. click the version first to read what's new. `neuron update status`, `check` and `install` do the same from a terminal. it's off for source builds and Steam copies, and `NEURON_NO_UPDATE` turns it off anywhere.

by hand: if you used the installer, download and run the newer setup from [Releases](https://github.com/worflor/neuron/releases/latest). your settings live separately in `%LOCALAPPDATA%\neuron`.

with the portable ZIP, open PowerShell in the folder containing `neuron-app.exe`. the bundled updater works on its own, without installing anything else:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ".\skills\neuron-lazy-update\scripts\neuron-update.ps1" -Action check -InstallDir "$PWD"
```

if it says `RESULT: update-available`, quit neuron and run the same command with `-Action apply`. it verifies the download and backs up the existing release files before replacing them. if it says `blocked` or `error`, read the `FLAG` lines. [script reference](../skills/neuron-lazy-update/update.md).

### am i running the installed release or a repository build?

from the repository or an unpacked release, run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ".\skills\neuron-lazy-update\scripts\neuron-update.ps1" -Action status
```

it lists the running copy, the Windows startup target and the installed release separately. `-Action check` compares the selected copy with the latest stable release; `-InstallDir` selects a different copy. an installed release can stay as a checkpoint while you work from the repo. both normally use `%LOCALAPPDATA%\neuron`; a portable copy uses its own folder. the same version number doesn't guarantee the same source commit or binary.

`.\release.ps1` builds and runs the repo copy for the session. add `-UseAtStartup` only when you want that build to become the Windows startup copy. `-NoRelaunch` builds without replacing your current session.

### how do i uninstall without losing my profiles?

remove the installed copy through Windows **Installed apps**. your profiles and settings stay in `%LOCALAPPDATA%\neuron`. a portable copy keeps its settings beside the executables, so keep that data before removing its folder. repository build output is disposable; source-build config lives outside `target`. an explicit `NEURON_RUN_DIR` overrides these defaults.

### where does my configuration live?

Windows installer and source builds: `%LOCALAPPDATA%\neuron`. a marked portable ZIP keeps its settings alongside its executables. `NEURON_RUN_DIR` can override either location. [full path rules](../README.md#where-your-config-lives).

## devices and help

### which hardware is supported?

neuron is a **Windows beta** with device controls tested on the **Naga V2 Pro** and **BlackWidow Chroma V2**, and a PowerA gamepad tested for input and rumble. neuron learns most other Razer devices on its own when you plug them in, and [the status page](STATUS.md) says what's been checked on real hardware. got something else? [help test it](#my-device-isnt-recognized-or-stays-dark-what-should-i-do).

### my device isn't recognized, or stays dark. what should i do?

neuron picks a device up when it's plugged in; there's nothing to scan. if neuron can't learn it, its row on **DEVICE** says why (often another app is holding it, so quit Synapse and other RGB tools). a board whose lighting isn't taking says **lights not landing · check**.

either way, press **check** on the device's row. it asks the device a few read-only questions and shows what answered. press **report** under it and a [device report](https://github.com/worflor/neuron/issues/new?template=device_report.yml) opens with the device, your system and the check filled in; add what you tried and what happened. no row at all? fill the report in by hand. want to send more? [Compatibility Lab has a walkthrough](reference/COMPATIBILITY-LAB.md#try-it-yourself).

### will the check or the compatibility tools change my hardware or upload anything?

no. a row's check only sends read-only questions. `neuron compat collect` reads OS and HID metadata; it doesn't send control commands or change settings. what it collects stays on your machine, and you choose what to export, review and attach. the local dossier can contain identifiers, so keep that one to yourself; the default shareable export leaves out raw input and identifiers, and raw input capture asks separately. [details](reference/COMPATIBILITY-LAB.md#evidence-contract).

### something broke. where do i report it?

the easiest way is **report a bug** on the **SYSTEM** page: say what happened and neuron writes a report with your version, devices and crash log (your name and paths scrubbed out), then opens a pre-filled issue to drop it into. `neuron report bug` does the same from a terminal. or use the [bug report form](https://github.com/worflor/neuron/issues/new?template=bug_report.md) directly.

security issues should be [reported privately](https://github.com/worflor/neuron/security/advisories/new), not as public bugs.

### does it work on Linux or macOS?

Windows is the downloadable release. Linux builds the app and CLI from source but hasn't run on Razer hardware yet, and has no download. macOS has no backend. [current platform status](STATUS.md).

### is it free? can i read the source?

yes, neuron is free and the whole repository is public. most of it is GPL-3.0-or-later with a linking exception; a few research components have a separate source-available license. [the exact split](../LICENSE.md).
