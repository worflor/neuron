# Install, update, roll back, uninstall

Everything here goes through one script, one per platform, in this skill's `scripts/` folder:

| platform | script | runs in |
|---|---|---|
| Windows | `neuron-update.ps1` | the Windows PowerShell that comes with Windows |
| Linux | `neuron-update.sh` | bash, with `curl` and `tar` |

Nothing extra needs installing. **Pick by the machine the user is on, and never point one at the
other platform.** Both speak the same output protocol and the same `RESULT:` codes, so every
table below applies to either — only the command line differs.

The Windows release is the app plus the CLI, so updating it closes and reopens neuron. The Linux
release is the CLI alone: nothing is resident, so there is no process to stop, and one extra
first-install step (a udev rule) that Windows does not have. Neither script ever touches the
user's config.

## How to run the script

On Windows:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "<path to skill>\scripts\neuron-update.ps1" -Action check
```

`-ExecutionPolicy Bypass` applies to that single command only. It doesn't change any system
setting.

On Linux:

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

Both platforms install the same way: into a per-user directory the user can write to without
becoming root. neuron then keeps its config in that same directory, beside the binary, which is
what makes the install portable — the whole folder can be moved or copied and the setup goes
with it. Installing somewhere the user cannot write splits config into a second location, so
don't.

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
5. **Autostart is optional and needs admin once.** To start neuron with Windows, the user runs
   `neuron-app.exe` as administrator one time (right-click → Run as administrator), then turns on
   "start with windows" on the SYSTEM page. Explain that; don't do it for them.

### Linux

1. The default is `~/.local/share/neuron` (`$XDG_DATA_HOME/neuron` when that is set). It needs no
   root and needs no flag — leave `--install-dir` off unless the user wants it elsewhere.
2. Confirm with the user, then run:

   ```bash
   bash "<path to skill>/scripts/neuron-update.sh" --action apply
   ```

3. `RESULT: installed` means the files are in place. The script then tells you two things worth
   passing on: `config_dir`, which is where their profiles and binds will live, and a
   `NOT_ON_PATH` flag if they'd have to type the full path to run it.
4. **`FLAG: NO_UDEV_RULE` is the one that matters.** Without the rule, `/dev/hidraw*` stays
   root-only and `neuron list` finds nothing even though everything installed correctly. The
   archive ships `70-neuron.rules` and `SOURCE.txt` has the exact two commands. It is one `sudo`,
   it is a system-wide change, and it is the user's to make — show them the commands, explain what
   they do, and let them run them. The device has to be replugged afterwards.
5. There is no autostart step and no tray: the Linux build is the CLI, so nothing runs in the
   background until the user runs `neuron run`.

## Roll back

If an update went wrong, confirm with the user, then run `-Action rollback` (`--action rollback`
on Linux). It restores the files from the most recent backup in `.neuron-update-backup` inside the
install folder. Only files a release ships were backed up and only those come back, so the user's
config is not affected either way.

`RESULT: rolled-back` means it's done — read the `installed_version` line above it back to the
user, because that is the version they are now on. `RESULT: blocked` with `NO_BACKUP` means there
was never an update to undo, which is the case on a fresh install.

## Admin rights

Windows reports `needs-admin`, Linux reports `needs-root`. Either way a `FLAG` line says which
problem it is, and the answer is never to rerun the script elevated:

- `CANNOT_STOP` (Windows): neuron is running elevated. Ask the user to right-click the tray icon,
  quit neuron, then run the update again.
- `NOT_WRITABLE`: the install folder needs privileges the user doesn't have. Suggest reinstalling
  to a per-user folder instead — `%LOCALAPPDATA%\Programs\neuron` on Windows,
  `~/.local/share/neuron` on Linux.

The udev rule is the one thing that genuinely needs `sudo`, and the user runs it themselves. It is
a separate, system-wide step, not part of installing.

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

On Linux there is no tray and no autostart, so steps 1 and 2 don't apply. Delete the install
folder (`~/.local/share/neuron` by default), with the same warning about config living in it, and
check `~/.local/share/neuron` separately if they installed somewhere else. Two leftovers to ask
about: any symlink they made into `~/.local/bin`, and `/etc/udev/rules.d/70-neuron.rules`, which
needs `sudo rm` and which they may want to keep if they use other Razer tooling.

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
| `ASSETS_MISSING` / `BAD_ZIP` / `BAD_ARCHIVE` | the release is incomplete | report it on the issues page |
| `COPY_FAILED` / `VERIFY_FAILED` | install went wrong partway; the backup was restored | show the lines |
| `NO_BACKUP` | nothing to roll back to | tell the user |
| `NO_UDEV_RULE` (Linux) | `/dev/hidraw*` is still root-only | install finished fine, but `neuron list` will find nothing until the user runs the two `sudo` commands from `SOURCE.txt` and replugs the device |
| `NOT_ON_PATH` (Linux) | the install folder isn't on `PATH` | they'd have to type the full path; offer the symlink line the flag prints |
| `MISSING_TOOL` (Linux, STOP) | `curl` or `tar` isn't installed | tell them which one; their package manager has it |
| `DOWNLOAD_FAILED` (Linux) | an asset couldn't be fetched | check internet, then retry |
| `ZIP_MISSING` / `ARCHIVE_MISSING` | `-ZipPath` / `--archive` points at nothing | check the path |

## Offline or specific versions

- `-Version v0.1.0` / `--version v0.1.0` installs that tag instead of the latest.
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

The script is the supported path, but nothing it does is magic, and a user who wants to see each
step can do this instead. Do not improvise a different order — this is the same sequence.

```bash
sha256sum -c SHA256SUMS.txt --ignore-missing   # must say OK, or stop
tar -xzf neuron-<version>-linux-x86_64.tar.gz
mv neuron-<version>-linux-x86_64 ~/.local/share/neuron
~/.local/share/neuron/neuron --version
```

Then the udev rule once, from `SOURCE.txt`, and a replug. The only thing lost by doing it this way
is the backup the script would have taken, so there is nothing to roll back to afterwards.
