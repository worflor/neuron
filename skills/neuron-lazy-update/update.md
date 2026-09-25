# Install, update, roll back, uninstall

The released Windows package uses the updater script in this skill's `scripts/` folder:

| platform | script | runs in |
|---|---|---|
| Windows | `neuron-update.ps1` | the Windows PowerShell that comes with Windows |
| Linux (future package) | `neuron-update.sh` | bash, with `curl` and `tar` |

v0.1.1 has no Linux asset. **Do not run the Linux updater against that release.** The Linux
script and archive steps below are for a future packaged release. For now, build from source as
described in the [README](../../README.md).

The Windows release includes the app and CLI, so updating it closes and reopens neuron. The
updater does not touch user config.

## How to run the script

On Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action check
```

`-ExecutionPolicy Bypass` applies to that single command only. It doesn't change any system
setting.

On Linux, after a package is published:

```bash
bash "<path to skill>/scripts/neuron-update.sh" --action check
```

The flags map one to one: `-Action` is `--action`, `-InstallDir` is `--install-dir`, `-Version`
is `--version`, `-AllowDowngrade` is `--allow-downgrade`. The one that differs by name is the
local-file mode, because the platforms ship different archives: `-ZipPath` on Windows,
`--archive` on Linux. Never run either script with `sudo`.

Read the output as lines:

- `key: value` is a fact.
- `FLAG: CODE - text` is something to tell the user. `(STOP)` in the text means stop.
- `RESULT: code` is always the last line, and it tells you what to do next.

## Update (Windows release)

1. **Check.** Run `-Action check`.
2. **Decide from the `RESULT`:**

   | RESULT | meaning | do this |
   |---|---|---|
   | `up-to-date` | already on the latest release | tell the user, stop |
   | `update-available` | a newer release exists | step 3 |
   | `not-installed` | no neuron in the expected folder | go to **Install** |
   | `source-build` | a developer build from the git repo | go to **Source builds** |
   | `error` | couldn't reach GitHub or read the release | show the `FLAG` line, stop |

3. **Confirm.** Tell the user something like: "neuron `<installed_version>` → `<latest_version>`.
   This closes neuron for a few seconds and reopens it. Your settings aren't touched. OK?"
4. **Apply.** Run `-Action apply`.
5. **Decide from the `RESULT`:**

   | RESULT | do this |
   |---|---|
   | `updated` | say so, and mention any `FLAG` lines |
   | `blocked` | stop, show the `FLAG` lines, don't retry |
   | `needs-admin` | explain it, see **Admin rights** |
   | `error` | show the lines; recovery was attempted when a backup exists. If `ROLLBACK_FAILED` appears, recovery was incomplete. |

## Install (first time)

Install into a per-user directory that is writable without elevation. Neuron keeps config
beside the binaries there, so the folder remains portable. A protected install directory
moves config to a second location.

### Windows

1. Recommend this folder: `%LOCALAPPDATA%\Programs\neuron`. It's per-user and writable, and needs
   no admin rights.
2. Don't install under `C:\Program Files`. The user can't write there without admin, and config
   would end up split across two folders.
3. Confirm with the user, then run:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action apply -InstallDir "$env:LOCALAPPDATA\Programs\neuron"
   ```

4. `RESULT: installed` means it's done. Tell the user how to start it: `neuron-app.exe` in that
   folder.
5. **Autostart is optional.** The user can turn on "start with windows" on the SYSTEM page.
   Neuron registers a task for that user at limited privilege; administrator launch is not needed.
6. Native Chroma shared memory needs the separate protected broker. The current source package
   carries its binary and installer script; [the setup steps](../../docs/PROTOCOL-HOST.md)
   require a one-time administrator PowerShell under the same Windows account. REST and OpenRGB
   work without that step.

### Linux

There is no Linux release asset to install or update in v0.1.1. Build from source using the
[README](../../README.md). The GUI exists but its live input, overlay, and audio paths are
incomplete on `main`. The updater script remains for a future Linux package.

## Roll back

If an update went wrong, confirm with the user, then run `-Action rollback` (`--action rollback`
on Linux). It restores the files from the newest timestamped backup in `.neuron-update-backup` inside
the install folder. Files that existed before the update are restored; newly shipped paths are
removed only when the backup manifest records them and their contents have not changed since install.
User config and files edited after the update are preserved. Backups without a created-files manifest
are not eligible for rollback because the updater cannot safely identify which new files it owns.

`RESULT: rolled-back` means it's done — read the `installed_version` line above it back to the
user, because that is the version they are now on. `ROLLBACK_PRESERVED` means a modified or
user-owned new path was left in place. `RESULT: blocked` with `NO_BACKUP` means no complete supported
backup is available. The current updater creates a manifest-backed backup on first install too.

## Admin rights

Windows reports `needs-admin`, Linux reports `needs-root`. Either way a `FLAG` line says which
problem it is, and the answer is never to rerun the script elevated:

- `CANNOT_STOP` (Windows): neuron is running elevated. Ask the user to right-click the tray icon,
  quit neuron, then run the update again.
- `NOT_WRITABLE`: the install folder needs privileges the user doesn't have. Suggest reinstalling
  to a per-user folder instead — `%LOCALAPPDATA%\Programs\neuron` on Windows,
  `~/.local/share/neuron` on Linux.
- `UNSAFE_STARTUP_TASK` (Windows): an old `HighestAvailable` task still targets a user-writable
  Neuron app. The updater stops before replacing any files. Replace that task with a Limited task
  or remove it from an administrator PowerShell, then rerun the updater normally. If it was
  removed, re-enable start-with-Windows in the new app if the user wants it.
- `STARTUP_TASK_QUERY_FAILED` (Windows): the updater cannot verify the task's privilege level.
  Resolve the task query before updating; do not assume a failed query means no task.

The udev rule is the one thing that genuinely needs `sudo`, and the user runs it themselves. It is
a separate, system-wide step, not part of installing.

## Source builds

`source-build` means neuron runs from a cargo `target` folder inside a git checkout. That's a
developer's copy, and the release zip must not overwrite it. Tell the user to update it from the
repo instead: `git pull`, rebuild with `cargo build --release`, then restart neuron.

## Uninstall

Confirm each step with the user first. Don't delete anything they haven't agreed to lose.

1. The user quits neuron from the tray.
2. If they ever turned on autostart, remove its scheduled task from a terminal under the same
   user account. The historical task name is retained for migration:

   ```powershell
   schtasks /delete /tn "Neuron (elevated tray)" /f
   ```

   If an older administrator-created task denies removal, the user may need an administrator
   terminal for this deletion only. Do not launch the app elevated to work around it.

3. Delete the install folder. **This deletes their config too**, because it lives in the same
   folder. Ask whether they want to keep a copy of the `profiles` folder and `*.toml` files first.
4. If `%LOCALAPPDATA%\neuron` exists, it holds config as well. Ask before deleting it.

For a Linux source build, follow its run-root setting in the README before deleting files. A
local symlink in `~/.local/bin` and an installed `/etc/udev/rules.d/70-neuron.rules` are separate
from the build directory; ask before removing either.

## Flags

| FLAG | meaning | what to tell the user or do |
|---|---|---|
| `HASH_MISMATCH` (STOP) | the download doesn't match its published checksum | don't install; it may be corrupted or tampered with |
| `ATTESTATION_FAILED` (STOP) | provenance check failed | don't install; point them to the GitHub issues page |
| `NO_CHECKSUMS` / `NOT_IN_CHECKSUMS` (STOP) | can't verify the download | don't install |
| `DOWNGRADE` | target is older than what's installed | only proceed if the user explicitly wants to go back |
| `STOP_UNCONFIRMED` | something is using neuron's exe, and it can't be identified | user quits neuron from the tray, then rerun |
| `CANNOT_STOP` | neuron is running elevated | same as above |
| `NOT_WRITABLE` | can't write to the install folder | see **Admin rights** |
| `TASK_ELSEWHERE` | autostart points at a different neuron folder | ask which install they mean |
| `UNSAFE_STARTUP_TASK` | an older task can start neuron elevated at sign-in; update stops before copying | see **Admin rights**, then rerun the updater |
| `STARTUP_TASK_QUERY_FAILED` | the startup task's privilege level could not be read | resolve the task query before updating |
| `CHROMA_BROKER_OUTDATED` | an installed protected broker differs from the packaged broker | rerun the broker setup in [the host guide](../../docs/PROTOCOL-HOST.md) under the same account |
| `CHROMA_BROKER_CHECK_FAILED` | the installed broker could not be compared | check the broker installation before claiming native Chroma works |
| `RELAUNCH_SKIPPED` | the updater is elevated, so relaunch would inherit administrator privileges | start neuron from a normal PowerShell after the update |
| `SOURCE_BUILD` | developer build | see **Source builds** |
| `NO_SOURCE_TXT` | not installed from a release zip | fine; version shown is best effort |
| `ATTESTATION_UNAVAILABLE` | the locally built v0.1.0 and v0.1.1 assets have no provenance attestation | the checksum matched; tell the user build provenance was not verified |
| `ATTESTATION_SKIPPED` | `gh` isn't logged in, so provenance wasn't checked | the checksum matched; tell the user provenance was not verified |
| `SAME_VERSION` | reinstalling the version already there | fine |
| `CLI_VERSION_MISMATCH` | `neuron.exe` reports a different version than the release | report it; it may be a packaging mistake |
| `OLD_BACKUPS` | more than three update backups kept | offer to delete the older ones |
| `BACKUP_FAILED` | a complete backup could not be created | installation did not start; report the error |
| `ROLLBACK_FAILED` | recovery could not finish | report it; don't claim the previous release was restored |
| `ROLLBACK_PRESERVED` | rollback kept a modified or user-owned new path | tell the user which state the updater preserved |
| `NO_RELEASE_INFO` | GitHub couldn't be reached | check internet; the repo may not be public yet |
| `ASSETS_MISSING` / `BAD_ZIP` / `BAD_ARCHIVE` | the release is incomplete | report it on the issues page |
| `COPY_FAILED` / `VERIFY_FAILED` | install went wrong partway; recovery was attempted | show the lines and any `ROLLBACK_FAILED` or `ROLLBACK_PRESERVED` flag |
| `NO_BACKUP` | nothing to roll back to | tell the user |
| `NO_UDEV_RULE` (Linux) | `/dev/hidraw*` is still root-only | install finished fine, but `neuron list` will find nothing until the user runs the two `sudo` commands from `SOURCE.txt` and replugs the device |
| `NOT_ON_PATH` (Linux) | the install folder isn't on `PATH` | they'd have to type the full path; offer the symlink line the flag prints |
| `MISSING_TOOL` (Linux, STOP) | `curl` or `tar` isn't installed | tell them which one; their package manager has it |
| `DOWNLOAD_FAILED` (Linux) | an asset couldn't be fetched | check internet, then retry |
| `ZIP_MISSING` / `ARCHIVE_MISSING` | `-ZipPath` / `--archive` points at nothing | check the path |

## Offline or specific versions

- `-Version v0.1.1` / `--version v0.1.1` installs that tag instead of the latest.
- `-ZipPath <zip>` (Windows) or `--archive <tar.gz>` (Linux) installs an archive the user already
  downloaded. `SHA256SUMS.txt` must sit next to it, or be passed with `-SumsPath` / `--sums`.
- `-NoRelaunch` (Windows) leaves neuron closed afterwards. Linux has nothing to relaunch.

## Linux: no device found

This is the common one, and it is almost never a neuron bug. Work through it in order:

1. Is the udev rule installed? `ls /etc/udev/rules.d/70-neuron.rules`. If it isn't, that's the
   answer — the two commands are in the archive's `SOURCE.txt`, the user runs them, then replugs.
2. Was the device replugged after the rule went in? The rule applies when the device next appears,
   not retroactively.
3. Does `sudo neuron list` find it when a plain `neuron list` doesn't? Then it is permissions, so
   it is the rule, not neuron. Say that plainly rather than suggesting they keep using `sudo`.
4. No systemd-logind (some minimal or non-systemd distros)? `uaccess` does nothing there. The rule
   file has a commented-out `plugdev` group line for exactly that case; the user swaps which line
   is active and adds themselves to the group.

If all four check out and the device still isn't found, that is worth an issue — and a genuinely
useful one, because **no Razer device has been plugged into a Linux box running neuron by anyone
yet**. Say so honestly; the user is not doing something wrong, they are first. See
[issues.md](issues.md).

## Doing it by hand

For a future Linux release archive, the script can perform these steps. v0.1.1 has no such asset.

```bash
sha256sum -c SHA256SUMS.txt --ignore-missing   # must say OK, or stop
tar -xzf neuron-<version>-linux-x86_64.tar.gz
mv neuron-<version>-linux-x86_64 ~/.local/share/neuron
~/.local/share/neuron/neuron --version
```

Then the udev rule once, from `SOURCE.txt`, and a replug. The only thing lost by doing it this way
is the backup the script would have taken, so there is nothing to roll back to afterwards.
