# Install, update, roll back, uninstall

Use the updater in this skill's `scripts/` folder for the selected package's platform:

| platform | script | runs in |
|---|---|---|
| Windows | `neuron-update.ps1` | the Windows PowerShell that comes with Windows |
| Linux | `neuron-update.sh` | bash, with `curl` and `tar` |

Check that the selected release includes the matching platform archive before
using its updater. When a platform has no release asset, use the source-build
instructions in the [README](../../README.md). Check [STATUS](../../docs/STATUS.md)
for runtime and hardware verification.

The Windows release includes the app and CLI, so updating it closes and reopens neuron. The
updater does not touch user config.

## How to run the script

On Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action check
```

`-ExecutionPolicy Bypass` applies to that single command only. It doesn't change any system
setting.

On Linux, with a matching release archive:

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

3. **Establish scope.** Report the installed and selected versions and the app restart.
   Proceed with the requested update; ask if the target or operation needs additional authorization.
4. **Apply.** Run `-Action apply`.
5. **Decide from the `RESULT`:**

   | RESULT | do this |
   |---|---|
   | `updated` | say so, and mention any `FLAG` lines |
   | `blocked` | stop, show the `FLAG` lines, don't retry |
   | `needs-admin` | explain it, see **Admin rights** |
   | `error` | show the lines; recovery was attempted when a backup exists. If `ROLLBACK_FAILED` appears, recovery was incomplete. |

## Install (first time)

The Windows setup executable creates an installer-managed copy whose config lives in
`%LOCALAPPDATA%\neuron`. The update script can also create a marked portable copy whose config
stays beside its binaries. Both layouts use a per-user program directory by default.

### Windows

1. Recommend the release setup executable for a normal Windows installation. It uses
   `%LOCALAPPDATA%\Programs\Neuron` and asks once for administrator approval to provision native
   Chroma; the app itself remains limited.
2. Use the update script when the user wants an agent-managed portable copy or already has Neuron.
   It detects Windows Setup installations and runs the matching verified setup instead of copying
   ZIP files across the installer's ownership boundary.
3. Use the authorized installation target, then run:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action apply -InstallDir "$env:LOCALAPPDATA\Programs\neuron"
   ```

4. `RESULT: installed` means it's done. Tell the user how to start it: `neuron-app.exe` in that
   folder.
5. **Autostart is optional.** The user can turn on "start with windows" on the SYSTEM page.
   Neuron registers a task for that user at limited privilege; administrator launch is not needed.
6. Native Chroma shared memory uses a protected broker. Windows asks for administrator approval
   while setup or the portable updater provisions it; the user does not run a separate command.
   Setup cancels if approval is declined. A portable copy can continue with REST and OpenRGB. The
   machine-wide broker belongs to the Windows user that installed it; update and uninstall it from
   that account.

### Linux

Check the selected release for a Linux archive. If one is available, use the
Linux updater; otherwise follow the [README](../../README.md) source-build
instructions. Use [STATUS](../../docs/STATUS.md) for the verification of input,
overlays, audio, and hardware on the user's platform.

## Roll back

Rollback depends on the installation kind:

- **Windows Setup:** reinstall the wanted release's setup executable. The updater reports
  `INSTALLER_ROLLBACK_REQUIRED` instead of mixing ZIP files with Inno Setup's uninstaller and pinned
  broker helper.
- **Windows portable:** `-Action rollback` restores payload files that existed before the most recent
  update from `.neuron-update-backup`. It does not delete paths introduced by that update, and it
  cannot preserve edits made later to a payload file it restores. Runtime config is outside the
  shipped payload list and is not in this backup.
- **Linux portable:** `--action rollback` uses its manifest to remove newly introduced payload paths
  only when their contents still match the update. It restores older payload files from backup;
  later edits to those pre-existing payload files are overwritten.

`RESULT: rolled-back` means the portable restore completed. Read the `installed_version` line back
to the user. `ROLLBACK_PRESERVED` names a new path the Linux updater left in place because it changed.
`RESULT: blocked` with `NO_BACKUP` means no supported portable backup is available.

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
2. Run the packaged uninstaller when available. It requests administrator approval to remove the
   protected Chroma broker plus the current and historical autostart tasks.
3. For a portable copy, delete the install folder. **This deletes their config too**, because it lives in the same
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
| `CHROMA_BROKER_SETUP_FAILED` | automatic protected broker setup was declined or failed | rerun the installer or updater to retry; REST still works |
| `CHROMA_BROKER_OTHER_USER` | the machine-wide broker belongs to another Windows user | use the owning account to update or uninstall Neuron |
| `CHROMA_BROKER_OWNER_UNKNOWN` | protected broker state exists without a valid owner receipt | repair or remove that state from an administrator PowerShell, then retry |
| `INSTALLER_ROLLBACK_REQUIRED` | a Windows Setup install cannot be restored by copying a ZIP backup | reinstall the wanted setup version |
| `SETUP_REQUIRED` / `SETUP_HASH_MISMATCH` | an installer-managed update lacks its matching verified setup | download the complete release assets and retry |
| `LEGACY_INSTALLER_DOWNGRADE` | v0.1.1 predates the limited-tray broker boundary | use a portable copy for historical testing |
| `RELAUNCH_SKIPPED` | the updater is elevated, so relaunch would inherit administrator privileges | start neuron from a normal PowerShell after the update |
| `SOURCE_BUILD` | developer build | see **Source builds** |
| `NO_SOURCE_TXT` | not installed from a release zip | fine; version shown is best effort |
| `ATTESTATION_UNAVAILABLE` | a locally built beta asset through v0.1.2 has no provenance attestation | the checksum matched; tell the user build provenance was not verified |
| `ATTESTATION_SKIPPED` | `gh` isn't logged in, so provenance wasn't checked | the checksum matched; tell the user provenance was not verified |
| `SAME_VERSION` | reinstalling the version already there | fine |
| `CLI_VERSION_MISMATCH` | `neuron.exe` reports a different version than the release | report it; it may be a packaging mistake |
| `OLD_BACKUPS` | more than three update backups kept | offer to delete the older ones |
| `BACKUP_FAILED` | a complete backup could not be created | installation did not start; report the error |
| `ROLLBACK_FAILED` | recovery could not finish | report it; don't claim the previous release was restored |
| `ROLLBACK_PRESERVED` | rollback kept a modified or user-owned new path | tell the user which state the updater preserved |
| `NO_RELEASE_INFO` | release metadata could not be read | check connectivity, repository access, and the reported error |
| `ASSETS_MISSING` / `BAD_ZIP` / `BAD_ARCHIVE` | the release is incomplete | report it on the issues page |
| `COPY_FAILED` / `VERIFY_FAILED` | install went wrong partway; recovery was attempted | show the lines and any `ROLLBACK_FAILED` or `ROLLBACK_PRESERVED` flag |
| `NO_BACKUP` | nothing to roll back to | tell the user |
| `NO_UDEV_RULE` (Linux) | `/dev/hidraw*` is still root-only | install finished fine, but `neuron list` will find nothing until the user runs the two `sudo` commands from `SOURCE.txt` and replugs the device |
| `NOT_ON_PATH` (Linux) | the install folder isn't on `PATH` | they'd have to type the full path; offer the symlink line the flag prints |
| `MISSING_TOOL` (Linux, STOP) | `curl` or `tar` isn't installed | tell them which one; their package manager has it |
| `DOWNLOAD_FAILED` (Linux) | an asset couldn't be fetched | check internet, then retry |
| `ZIP_MISSING` / `ARCHIVE_MISSING` | `-ZipPath` / `--archive` points at nothing | check the path |

## Offline or specific versions

- `-Version <tag>` / `--version <tag>` selects that release tag.
- `-ZipPath <zip>` (Windows) or `--archive <tar.gz>` (Linux) installs an archive the user already
  downloaded. `SHA256SUMS.txt` must sit next to it, or be passed with `-SumsPath` / `--sums`. An
  installer-managed Windows copy also needs the matching `*-setup.exe` beside the ZIP; both hashes
  are checked before setup runs.
- `-NoRelaunch` (Windows) leaves Neuron closed afterwards.

## Linux: no device found

Check device permissions and discovery in order:

1. Is the udev rule installed? `ls /etc/udev/rules.d/70-neuron.rules`. If it isn't, that's the
   answer — the two commands are in the archive's `SOURCE.txt`, the user runs them, then replugs.
2. Was the device replugged after the rule went in? The rule applies when the device next appears,
   not retroactively.
3. Does `sudo neuron list` find it when a plain `neuron list` doesn't? Then it is permissions, so
   it is the rule, not neuron. Say that plainly rather than suggesting they keep using `sudo`.
4. No systemd-logind (some minimal or non-systemd distros)? `uaccess` does nothing there. The rule
   file has a commented-out `plugdev` group line for exactly that case; the user swaps which line
   is active and adds themselves to the group.

If the device remains undiscovered, collect the device identity, platform,
permission checks, and observed results. Compare them with `docs/STATUS.md`
and follow [issues.md](issues.md).

## Doing it by hand

For a matching Linux release archive, the manual steps are:

```bash
sha256sum -c SHA256SUMS.txt --ignore-missing   # must say OK, or stop
tar -xzf neuron-<version>-linux-x86_64.tar.gz
mv neuron-<version>-linux-x86_64 ~/.local/share/neuron
~/.local/share/neuron/neuron --version
```

Then the udev rule once, from `SOURCE.txt`, and a replug. The only thing lost by doing it this way
is the backup the script would have taken, so there is nothing to roll back to afterwards.
