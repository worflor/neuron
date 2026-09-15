# Install, update, roll back, uninstall

Everything here goes through one script: `scripts/neuron-update.ps1`, in this skill's folder.
It works in the Windows PowerShell that comes with Windows. Nothing extra needs installing.

## How to run the script

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action check
```

`-ExecutionPolicy Bypass` applies to that single command only. It doesn't change any system
setting.

Read the output as lines:

- `key: value` is a fact.
- `FLAG: CODE - text` is something to tell the user. `(STOP)` in the text means stop.
- `RESULT: code` is always the last line, and it tells you what to do next.

## Update (the common case)

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
   | `error` | show the lines; the script already restored the backup if it had made one |

## Install (first time)

1. Recommend this folder: `%LOCALAPPDATA%\Programs\neuron`. It's per-user and writable, and needs
   no admin rights. neuron keeps its config next to the exes there.
2. Don't install under `C:\Program Files`. The user can't write there without admin, and config
   would end up split across two folders.
3. Confirm with the user, then run:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action apply -InstallDir "$env:LOCALAPPDATA\Programs\neuron"
   ```

4. `RESULT: installed` means it's done. Tell the user how to start it: `neuron-app.exe` in that
   folder.
5. **Autostart is optional and needs admin once.** To start neuron with Windows, the user runs
   `neuron-app.exe` as administrator one time (right-click → Run as administrator), then turns on
   "start with windows" on the SYSTEM page. Explain that; don't do it for them.

## Roll back

If an update went wrong, confirm with the user, then run `-Action rollback`. It restores the files
from the most recent backup in `.neuron-update-backup` inside the install folder.

## Admin rights

`needs-admin` means one of two things, and a `FLAG` line says which:

- `CANNOT_STOP`: neuron is running elevated. Ask the user to right-click the tray icon, quit neuron,
  then run the update again.
- `NOT_WRITABLE`: the install folder needs admin rights. Suggest reinstalling to
  `%LOCALAPPDATA%\Programs\neuron` instead of running as administrator.

## Source builds

`source-build` means neuron runs from a cargo `target` folder inside a git checkout. That's a
developer's copy, and the release zip must not overwrite it. Tell the user to update it from the
repo instead: `git pull`, rebuild with `cargo build --release`, then restart neuron.

## Uninstall

Confirm each step with the user first. Don't delete anything they haven't agreed to lose.

1. The user quits neuron from the tray.
2. If they ever turned on autostart, remove its scheduled task. This needs an administrator
   terminal:

   ```powershell
   schtasks /delete /tn "Neuron (elevated tray)" /f
   ```

3. Delete the install folder. **This deletes their config too**, because it lives in the same
   folder. Ask whether they want to keep a copy of the `profiles` folder and `*.toml` files first.
4. If `%LOCALAPPDATA%\neuron` exists, it holds config as well. Ask before deleting it.

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
| `SOURCE_BUILD` | developer build | see **Source builds** |
| `NO_SOURCE_TXT` | not installed from a release zip | fine; version shown is best effort |
| `ATTESTATION_SKIPPED` | `gh` isn't logged in, so provenance wasn't checked | fine; the checksum still matched |
| `SAME_VERSION` | reinstalling the version already there | fine |
| `CLI_VERSION_MISMATCH` | `neuron.exe` reports a different version than the release | report it; it may be a packaging mistake |
| `OLD_BACKUPS` | more than three update backups kept | offer to delete the older ones |
| `NO_RELEASE_INFO` | GitHub couldn't be reached | check internet; the repo may not be public yet |
| `ASSETS_MISSING` / `BAD_ZIP` | the release is incomplete | report it on the issues page |
| `COPY_FAILED` / `VERIFY_FAILED` | install went wrong partway; the backup was restored | show the lines |
| `NO_BACKUP` | nothing to roll back to | tell the user |

## Offline or specific versions

- `-Version v0.1.0` installs that tag instead of the latest.
- `-ZipPath <zip>` installs a release zip the user already downloaded. `SHA256SUMS.txt` must sit next
  to it, or be passed with `-SumsPath`.
- `-NoRelaunch` leaves neuron closed afterwards.
