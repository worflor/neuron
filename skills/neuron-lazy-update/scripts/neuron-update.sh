#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# Install, update, or roll back a neuron release install on Linux.
#
# The Linux release is the CLI only, so unlike the Windows script there is no process to stop
# and no autostart to preserve. Everything around the copy is the same: verify the download,
# back up what gets overwritten, read the version back afterwards.
#
# Output protocol, meant to be read by an agent, identical to neuron-update.ps1:
#   key: value         facts
#   FLAG: CODE - text  something worth telling the user; codes marked STOP mean do not continue
#   RESULT: code       always the last line; see update.md for what each code means
#
#   ./neuron-update.sh --action check
#   ./neuron-update.sh --action apply
#   ./neuron-update.sh --action rollback
#
# It never deletes anything in the install directory. It overwrites only the files a release
# ships, and backs those up first.

set -u

REPO='worflor/neuron'
BACKUP_ROOT='.neuron-update-backup'
UDEV_RULE='/etc/udev/rules.d/70-neuron.rules'
DEFAULT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/neuron"

action='check'
install_dir=''
version=''
archive_path=''
sums_path=''
allow_downgrade=0

say()  { printf '%s: %s\n' "$1" "$2"; }
flag() { printf 'FLAG: %s - %s\n' "$1" "$2"; }
finish() {
    printf 'RESULT: %s\n' "$1"
    case "$1" in
        blocked | error | needs-root) exit 1 ;;
        *) exit 0 ;;
    esac
}

usage() {
    cat >&2 <<'EOF'
usage: neuron-update.sh [--action check|apply|rollback] [--install-dir DIR] [--version TAG]
                        [--archive FILE] [--sums FILE] [--allow-downgrade]
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --action)          action="${2:-}"; shift 2 || usage ;;
        --install-dir)     install_dir="${2:-}"; shift 2 || usage ;;
        --version)         version="${2:-}"; shift 2 || usage ;;
        --archive)         archive_path="${2:-}"; shift 2 || usage ;;
        --sums)            sums_path="${2:-}"; shift 2 || usage ;;
        --allow-downgrade) allow_downgrade=1; shift ;;
        -h | --help)       usage ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage ;;
    esac
done

case "$action" in
    check | apply | rollback) ;;
    *) printf 'unknown action: %s\n' "$action" >&2; usage ;;
esac

# ── helpers ───────────────────────────────────────────────────────────────────────────────────

# Named up front rather than failing halfway through an install.
need() {
    command -v "$1" >/dev/null 2>&1 && return 0
    flag 'MISSING_TOOL' "this needs '$1', which is not on PATH (STOP)"
    finish 'blocked'
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# "v0.1.0-mk1" / "neuron 0.1.0" -> "0.1.0". Empty when there is no version-shaped substring.
parse_version() {
    printf '%s' "$1" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1
}

# Returns 0 when $1 is strictly older than $2, comparing numerically field by field.
version_lt() {
    [ "$1" = "$2" ] && return 1
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$1" ]
}

installed_version_of() {
    local dir="$1" first out
    if [ -f "$dir/SOURCE.txt" ]; then
        first="$(head -n1 "$dir/SOURCE.txt")"
        case "$first" in
            neuron\ v*) printf '%s' "$first" | awk '{print $2}'; return 0 ;;
        esac
    fi
    if [ -x "$dir/neuron" ]; then
        out="$("$dir/neuron" --version 2>/dev/null || true)"
        out="$(parse_version "$out")"
        [ -n "$out" ] && { printf 'v%s' "$out"; return 0; }
    fi
    return 1
}

# True when $dir sits inside a cargo build tree, which means a developer build that a release
# archive must not be copied over. Mirrors runroot.rs: a `target` whose parent holds a Cargo.toml.
is_source_build() {
    local p
    p="$(cd "$1" 2>/dev/null && pwd -P)" || return 1
    while [ -n "$p" ] && [ "$p" != '/' ]; do
        if [ "$(basename "$p")" = 'target' ] && [ -f "$(dirname "$p")/Cargo.toml" ]; then
            return 0
        fi
        p="$(dirname "$p")"
    done
    return 1
}

# Copies every file under $1 into $2, preserving relative paths and the executable bit.
copy_tree() {
    local from="$1" to="$2" rel dest
    ( cd "$from" && find . -type f -print ) | while IFS= read -r rel; do
        rel="${rel#./}"
        dest="$to/$rel"
        mkdir -p "$(dirname "$dest")"
        cp -p "$from/$rel" "$dest"
    done
}

cleanup() { [ -n "${work:-}" ] && [ -d "${work:-}" ] && rm -rf "$work"; }
trap cleanup EXIT

# ── resolve the install directory ─────────────────────────────────────────────────────────────

need tar
[ -z "$install_dir" ] && install_dir="$DEFAULT_DIR"
# Realpath without requiring the directory to exist yet.
install_dir="$(cd "$(dirname "$install_dir")" 2>/dev/null && pwd -P)/$(basename "$install_dir")" \
    || install_dir="$install_dir"

installed=0
[ -x "$install_dir/neuron" ] && installed=1

say 'install_dir' "$install_dir"
say 'installed' "$([ $installed -eq 1 ] && echo true || echo false)"

# Missing rule is the usual cause of "no devices found". Flagged, never fixed here: installing a
# system-wide rule is a one-time sudo and the user's decision.
if [ ! -f "$UDEV_RULE" ]; then
    flag 'NO_UDEV_RULE' "no $UDEV_RULE, so /dev/hidraw* stays root-only and 'neuron list' will likely find nothing. The archive ships 70-neuron.rules; installing it is one sudo, and the two commands are in SOURCE.txt. Do not run them for the user."
fi

current=''
if [ $installed -eq 1 ]; then
    if is_source_build "$install_dir"; then
        say 'kind' 'source build (inside a cargo target directory)'
        flag 'SOURCE_BUILD' 'this is a developer build. Update it with git pull and a rebuild, not a release archive.'
        finish 'source-build'
    fi
    current="$(installed_version_of "$install_dir" || true)"
    say 'installed_version' "${current:-unknown}"
    if [ ! -f "$install_dir/SOURCE.txt" ]; then
        flag 'NO_SOURCE_TXT' 'no SOURCE.txt, so this was not installed from a release archive. Version detection is best effort.'
    fi
fi

# ── rollback ──────────────────────────────────────────────────────────────────────────────────

if [ "$action" = 'rollback' ]; then
    bdir="$install_dir/$BACKUP_ROOT"
    latest="$(ls -1 "$bdir" 2>/dev/null | sort | tail -n1)"
    if [ -z "$latest" ]; then
        flag 'NO_BACKUP' "nothing to roll back to in $bdir"
        finish 'blocked'
    fi
    say 'restoring' "$bdir/$latest"
    if [ ! -w "$install_dir" ]; then
        flag 'NOT_WRITABLE' "cannot write to $install_dir"
        finish 'needs-root'
    fi
    copy_tree "$bdir/$latest" "$install_dir"
    say 'installed_version' "$(installed_version_of "$install_dir" || echo unknown)"
    finish 'rolled-back'
fi

# ── resolve the target release ────────────────────────────────────────────────────────────────

work="$(mktemp -d)"

if [ -n "$archive_path" ]; then
    if [ ! -f "$archive_path" ]; then
        flag 'ARCHIVE_MISSING' "no file at $archive_path"
        finish 'error'
    fi
    archive="$(cd "$(dirname "$archive_path")" && pwd -P)/$(basename "$archive_path")"
    if [ -n "$sums_path" ]; then
        sums="$(cd "$(dirname "$sums_path")" && pwd -P)/$(basename "$sums_path")"
    else
        sums="$(dirname "$archive")/SHA256SUMS.txt"
    fi
    say 'source' "local file $archive"
    if [ "$action" = 'check' ]; then
        say 'note' 'offline check: the target version is read from the archive during apply'
        finish "$([ $installed -eq 1 ] && echo update-available || echo not-installed)"
    fi
else
    need curl
    if [ -n "$version" ]; then
        api="https://api.github.com/repos/$REPO/releases/tags/$version"
    else
        api="https://api.github.com/repos/$REPO/releases/latest"
    fi
    rel="$work/release.json"
    if ! curl -fsSL -H 'User-Agent: neuron-lazy-update' -o "$rel" "$api"; then
        flag 'NO_RELEASE_INFO' "could not read releases from GitHub (offline, rate-limited, the repo is private, or tag '${version:-latest}' does not exist)"
        finish 'error'
    fi
    target="$(grep -o '"tag_name"[^,]*' "$rel" | head -n1 | cut -d'"' -f4)"
    say 'latest_version' "${target:-unknown}"

    asset_url="$(grep -o '"browser_download_url"[^,]*' "$rel" | cut -d'"' -f4 \
        | grep -E 'neuron-.*-linux-x86_64\.tar\.gz$' | head -n1)"
    sums_url="$(grep -o '"browser_download_url"[^,]*' "$rel" | cut -d'"' -f4 \
        | grep -E '/SHA256SUMS\.txt$' | head -n1)"
    if [ -z "$asset_url" ] || [ -z "$sums_url" ]; then
        flag 'ASSETS_MISSING' "release ${target:-?} has no linux tarball or no SHA256SUMS.txt"
        finish 'error'
    fi

    if [ "$action" = 'check' ]; then
        [ $installed -eq 0 ] && finish 'not-installed'
        cv="$(parse_version "$current")"
        tv="$(parse_version "$target")"
        if [ -n "$cv" ] && [ -n "$tv" ] && ! version_lt "$cv" "$tv"; then
            finish 'up-to-date'
        fi
        finish 'update-available'
    fi

    archive="$work/$(basename "$asset_url")"
    sums="$work/SHA256SUMS.txt"
    curl -fsSL -H 'User-Agent: neuron-lazy-update' -o "$archive" "$asset_url" \
        || { flag 'DOWNLOAD_FAILED' "could not download $asset_url"; finish 'error'; }
    curl -fsSL -H 'User-Agent: neuron-lazy-update' -o "$sums" "$sums_url" \
        || { flag 'DOWNLOAD_FAILED' "could not download $sums_url"; finish 'error'; }
fi

# ── verify the download ───────────────────────────────────────────────────────────────────────

if [ ! -f "$sums" ]; then
    flag 'NO_CHECKSUMS' 'SHA256SUMS.txt not found next to the archive (STOP)'
    finish 'blocked'
fi
archive_name="$(basename "$archive")"
# The filename field carries a leading '*' when the sums file was written in binary mode, which
# is what sha256sum does by default on some platforms. Releases are generated on linux and are
# not marked, but a user checking a locally-built archive may well be.
expected="$(awk -v n="$archive_name" '{ f = $NF; sub(/^\*/, "", f); if (f == n) print tolower($1) }' "$sums" | tail -n1)"
actual="$(sha256_of "$archive" | tr 'A-F' 'a-f')"
say 'sha256' "$actual"
if [ -z "$expected" ]; then
    flag 'NOT_IN_CHECKSUMS' "$archive_name is not listed in SHA256SUMS.txt (STOP)"
    finish 'blocked'
fi
if [ "$expected" != "$actual" ]; then
    flag 'HASH_MISMATCH' 'the archive does not match its published checksum (STOP). Do not install it.'
    finish 'blocked'
fi
say 'checksum' 'ok'

if [ -z "$archive_path" ] && command -v gh >/dev/null 2>&1; then
    att="$(gh attestation verify "$archive" --repo "$REPO" 2>&1)"
    if [ $? -eq 0 ]; then
        say 'attestation' 'ok'
    elif printf '%s' "$att" | grep -qE 'auth login|not logged'; then
        flag 'ATTESTATION_SKIPPED' 'gh is installed but not logged in, so provenance was not checked. The checksum still matched.'
    else
        flag 'ATTESTATION_FAILED' "gh attestation verify failed (STOP): $att"
        finish 'blocked'
    fi
elif [ -z "$archive_path" ]; then
    say 'attestation' 'skipped (gh not installed; checksum matched)'
fi

# ── unpack and compare versions ───────────────────────────────────────────────────────────────

unpacked="$work/unpacked"
mkdir -p "$unpacked"
tar -xzf "$archive" -C "$unpacked" || { flag 'BAD_ARCHIVE' 'could not extract the tarball'; finish 'error'; }
payload_dir="$(dirname "$(find "$unpacked" -type f -name neuron | head -n1)")"
if [ -z "$payload_dir" ] || [ ! -f "$payload_dir/neuron" ]; then
    flag 'BAD_ARCHIVE' 'the tarball has no neuron binary'
    finish 'error'
fi
chmod +x "$payload_dir/neuron" 2>/dev/null || true
new_version="$(installed_version_of "$payload_dir" || true)"
say 'new_version' "${new_version:-unknown}"

if [ $installed -eq 1 ] && [ -n "$current" ] && [ -n "$new_version" ]; then
    cv="$(parse_version "$current")"
    nv="$(parse_version "$new_version")"
    if [ -n "$cv" ] && [ -n "$nv" ]; then
        if version_lt "$nv" "$cv" && [ $allow_downgrade -eq 0 ]; then
            flag 'DOWNGRADE' "$new_version is older than the installed $current. Rerun with --allow-downgrade only if the user asked for that."
            finish 'blocked'
        fi
        if [ "$nv" = "$cv" ]; then
            flag 'SAME_VERSION' "$new_version is already installed; reinstalling the same files"
        fi
    fi
fi

# ── install ───────────────────────────────────────────────────────────────────────────────────

mkdir -p "$install_dir" 2>/dev/null || true
if [ ! -d "$install_dir" ] || [ ! -w "$install_dir" ]; then
    flag 'NOT_WRITABLE' "cannot write to $install_dir. Use a per-user directory such as $DEFAULT_DIR rather than running this with sudo."
    finish 'needs-root'
fi

backup=''
if [ $installed -eq 1 ]; then
    stamp="${current:-unknown}-$(date +%Y%m%d-%H%M%S)"
    backup="$install_dir/$BACKUP_ROOT/$stamp"
    mkdir -p "$backup"
    ( cd "$payload_dir" && find . -type f -print ) | while IFS= read -r rel; do
        rel="${rel#./}"
        if [ -f "$install_dir/$rel" ]; then
            mkdir -p "$(dirname "$backup/$rel")"
            cp -p "$install_dir/$rel" "$backup/$rel"
        fi
    done
    say 'backup' "$backup"
    n="$(ls -1 "$install_dir/$BACKUP_ROOT" 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$n" -gt 3 ]; then
        flag 'OLD_BACKUPS' "$n update backups are kept in $BACKUP_ROOT. Older ones can be deleted if the user wants the space."
    fi
fi

if ! copy_tree "$payload_dir" "$install_dir"; then
    flag 'COPY_FAILED' "could not copy the release into $install_dir"
    [ -n "$backup" ] && { copy_tree "$backup" "$install_dir"; say 'restored_from' "$backup"; }
    finish 'error'
fi
chmod +x "$install_dir/neuron" 2>/dev/null || true

# ── verify ────────────────────────────────────────────────────────────────────────────────────

after="$(installed_version_of "$install_dir" || true)"
say 'installed_version' "${after:-unknown}"
if [ -n "$new_version" ] && [ "$after" != "$new_version" ]; then
    flag 'VERIFY_FAILED' "expected $new_version after install, found ${after:-unknown}"
    [ -n "$backup" ] && { copy_tree "$backup" "$install_dir"; say 'restored_from' "$backup"; }
    finish 'error'
fi

cli_out="$("$install_dir/neuron" --version 2>/dev/null || true)"
say 'cli_reports' "$cli_out"
if [ -n "$new_version" ] && [ "$(parse_version "$cli_out")" != "$(parse_version "$new_version")" ]; then
    flag 'CLI_VERSION_MISMATCH' "neuron reports '$cli_out' but SOURCE.txt says $new_version"
fi

# The run root follows the binary: config lands beside it while this directory stays writable,
# which is what makes the install portable.
say 'config_dir' "$install_dir (portable: config sits beside the binary)"

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) flag 'NOT_ON_PATH' "$install_dir is not on PATH. The user can add it, or symlink the binary: ln -s '$install_dir/neuron' ~/.local/bin/neuron (a symlink is safe - neuron resolves its real location, so config stays in the install directory)." ;;
esac

finish "$([ $installed -eq 1 ] && echo updated || echo installed)"
