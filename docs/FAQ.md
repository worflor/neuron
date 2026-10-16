# neuron FAQ

just want to use your hardware without turning it into a research project? start here. these are the ordinary ways to use neuron. no coding required for the main app.

## getting started

### how do i install it?

on Windows 10/11, [download the latest release](https://github.com/worflor/neuron/releases/latest). use the setup `.exe` for a normal install, or extract the portable ZIP and launch `neuron-app.exe`. the installer makes a Start menu shortcut; the portable version runs from its folder.

Windows may show a SmartScreen warning because the builds aren't code-signed. check the release's `SHA256SUMS.txt` and `SOURCE.txt` if you want to verify what you downloaded. native Chroma setup may ask for admin approval; Neuron itself runs without elevation.

### where do i change my settings?

open the app from Start or the tray. **DEVICE** is for DPI, polling and hardware settings; **LIGHTING** is for effects, gradients and layers; **INPUT** is for buttons, binds and gestures; **SYSTEM** has diagnostics and safety controls. profiles open from the header and can switch with the foreground app.

there's also a [command-line reference](reference/CLI.md) if you'd rather use a terminal. the GUI is enough for everyday use.

### do i have to keep Synapse installed?

no. i don't run Synapse on my own Naga V2 Pro or BlackWidow Chroma V2 anymore. Neuron handles what i use there. supported features on other devices still need testing, so [check the status page](STATUS.md) before removing your existing setup. Synapse and Neuron shouldn't control the same device at the same time.

Neuron can import supported plaintext Synapse exports. it can't recover encrypted cloud-only profiles. see [migration details](GDD.md#life-after-synapse).

### can i remap buttons and make macros without writing code?

yes for ordinary binds: use **INPUT** to connect a button or gesture to a supported action. HyperShift and app profiles are available where supported. the macro workshop also has a visual blocks editor; custom Python macros are an option when you want more than blocks provide. see [macros and beacons](GDD.md#macros--beacons).

### how do i update it?

if you used the installer, download and run the newer setup from [Releases](https://github.com/worflor/neuron/releases/latest). your settings are kept separately in `%LOCALAPPDATA%\neuron`.

if you're using the portable ZIP, open PowerShell in the folder containing `neuron-app.exe`. the bundled updater works on its own, without installing anything else:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ".\skills\neuron-lazy-update\scripts\neuron-update.ps1" -Action check -InstallDir "$PWD"
```

if it says `RESULT: update-available`, quit Neuron and run the same command with `-Action apply`. it verifies the download and backs up the existing release files before replacing them. if it says `blocked` or `error`, stop and read the `FLAG` lines. [script reference](../skills/neuron-lazy-update/update.md).

### am i running the installed release or a repository build?

from the repository or an unpacked release, run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ".\skills\neuron-lazy-update\scripts\neuron-update.ps1" -Action status
```

it lists the running copy, Windows startup target and installed release separately. `-Action check`
compares the selected copy with the latest stable release; `-InstallDir` selects a different copy.
An installed release can stay as a checkpoint while you work from the repo. Both normally use
`%LOCALAPPDATA%\neuron`; a portable copy uses its own folder. The same version number doesn't
guarantee the same source commit or binary.

`.\release.ps1` builds and runs the repo copy for the session. Add `-UseAtStartup` only when you
want that build to become the Windows startup copy. `-NoRelaunch` builds without replacing your
current session.

### how do i uninstall without losing my profiles?

remove the installed copy through Windows **Installed apps**. Your profiles and settings stay in
`%LOCALAPPDATA%\neuron`. A portable copy keeps its settings beside the executables, so keep that
data before removing its folder. Repository build output is disposable; source-build config lives
outside `target`. An explicit `NEURON_RUN_DIR` overrides these defaults.

### where does my configuration live?

Windows installer/source builds: `%LOCALAPPDATA%\neuron`. a marked portable ZIP keeps its settings alongside its executables. `NEURON_RUN_DIR` can override either location. [full path rules](../README.md#where-your-config-lives).

## devices and help

### which hardware is supported?

Neuron is a **Windows beta** with device controls tested on the **Naga V2 Pro** and **BlackWidow Chroma V2**. other devices might respond to its discovery process, but being detected isn't the same as being verified. on Windows, a PowerA gamepad has also been tested for input and rumble. [the status page](STATUS.md) separates tested, partial and unverified features.

### my device isn't recognized. what should i do?

first check that it's connected and Synapse isn't fighting for control. in Neuron, run the read-only diagnostics on **SYSTEM**. from PowerShell in Neuron's install folder, `.\neuron.exe list` shows recognized devices and `.\neuron.exe compat --help` shows the hardware-report tools in your build.

then [report your device](https://github.com/worflor/neuron/issues/new?template=device_report.yml): model, wired/dongle/Bluetooth, what you tried and what happened. no PID or coding required. want to send more evidence? [Compatibility Lab has a manual walkthrough](reference/COMPATIBILITY-LAB.md#try-it-yourself).

### will the compatibility tools change my hardware or upload anything?

ordinary `neuron compat collect` reads OS/HID metadata. it doesn't send control commands or change settings. collected data stays local; you choose what to export, review and attach. the local dossier can contain identifiers, so don't upload it. default shareable exports omit raw input and identifiers. any raw input capture needs separate consent. [details](reference/COMPATIBILITY-LAB.md#evidence-contract).

### something broke. where do i report it?

use the [bug report form](https://github.com/worflor/neuron/issues/new?template=bug_report.md). your device model, Neuron version, what happened and how to reproduce it are enough to start. screenshots and reviewed diagnostics help, but aren't required. don't post serials, private paths or your entire configuration.

security issues should be [reported privately](https://github.com/worflor/neuron/security/advisories/new), not as public bugs.

### does it work on Linux or macOS?

Windows is the current downloadable release. Linux app and CLI build from source, but its live input and hardware support are not yet verified on native Razer hardware. no Linux release asset yet. macOS has no backend. [current platform status](STATUS.md).

### is it free? can i inspect the source?

yes, Neuron is free and you can read the repository. most of the project is GPL-3.0-or-later with a linking exception; some research components have a separate source-available license. [the exact split](../LICENSE.md).
