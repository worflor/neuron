#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

set -euo pipefail
version="${1:-v0.1.1}"
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || {
    echo 'version must look like v0.1.1 or v0.1.1-rc1' >&2
    exit 2
}
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
head="$(git rev-parse HEAD)"
dirty=clean
if [[ -n "$(git status --porcelain)" ]]; then
    dirty=modified
    [[ "${NEURON_ALLOW_DIRTY:-0}" == 1 ]] || {
        echo 'commit the source before packaging a release' >&2
        exit 2
    }
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target-lane-release-linux}"
if [[ "${NEURON_SKIP_BUILD:-0}" != 1 ]]; then
    cargo build --release -p neuron-app -p neuron-cli --locked
fi
cli="$CARGO_TARGET_DIR/release/neuron"
app="$CARGO_TARGET_DIR/release/neuron-app"
for bin in "$cli" "$app"; do
    [[ -x "$bin" ]] || { echo "release binary missing: $bin" >&2; exit 2; }
done
floor="$(readelf -W --version-info "$cli" "$app" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | tail -1)"
[[ -n "$floor" ]] || { echo 'could not determine glibc requirement' >&2; exit 2; }

name="neuron-$version-linux-x86_64"
stage="$repo/dist/$name"
mkdir -p "$repo/dist"
case "$stage" in "$repo/dist/"*) ;; *) echo 'stage escaped dist' >&2; exit 2 ;; esac
[[ ! -L "$repo/dist" && ! -L "$stage" ]] || {
    echo 'refusing to package through a symlinked dist path' >&2
    exit 2
}
rm -rf -- "$stage"
mkdir -p "$stage"
cp "$cli" "$app" "$stage/"
cp README.md LICENSE.md THIRD-PARTY-NOTICES.md SECURITY.md "$stage/"
cp -R LICENSES skills "$stage/"
cp packaging/linux/70-neuron.rules "$stage/"
cat > "$stage/SOURCE.txt" <<EOF
Neuron $version — Linux x86_64 app and CLI
Repository: https://github.com/worflor/neuron
Commit: $head
Build: local Linux release
Working tree at build: $dirty
ELF glibc symbol floor: $floor

Run ./neuron-app for the GUI or ./neuron for the CLI. The GUI needs GTK 3,
AppIndicator, libxdo, libxkbcommon-x11 and a graphical desktop session.
On Ubuntu, libxkbcommon-x11 is provided by libxkbcommon-x11-0. Global hotkeys use X11.

For non-root hidraw access, from this extracted directory run:
  sudo install -m 0644 70-neuron.rules /etc/udev/rules.d/70-neuron.rules
  sudo udevadm control --reload-rules && sudo udevadm trigger
Reconnect the device after installing the rule.

This build has no GitHub Actions provenance attestation. Check SHA256SUMS.txt
against the downloaded archive and review the source commit above. The matching
source and license terms are in the repository.
EOF
tar -czf "$repo/dist/$name.tar.gz" -C "$repo/dist" "$name"
(cd "$repo/dist" && sha256sum "$name.tar.gz" > SHA256SUMS.txt)
echo "Packaged $repo/dist/$name.tar.gz and $repo/dist/SHA256SUMS.txt ($floor)"
