// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The self-updater: trust is a signature, not a domain. Each release carries a manifest signed
//! with the release key compiled in here ([`release_key`]); the updater acts only on a manifest
//! that verifies, only on files whose size and SHA-256 that manifest names, and never on an older
//! or equal version. Installing goes through the lifecycle that owns the copy ([`install`]).

pub mod client;
pub mod guard;
pub mod install;
pub mod manifest;
pub mod notes;
pub mod policy;
pub mod state;
pub mod version;

use client::{Endpoints, Error, LatestRelease};
use manifest::{Manifest, Platform};
use state::State;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use version::Version;

pub const REPO: &str = "worflor/neuron";

/// The release signing public key (base64 of 32 raw Ed25519 bytes). Its private half lives
/// offline with the maintainer and in the release workflow's secret.
const RELEASE_KEY: &str = include_str!("release-key.pub");

/// The compiled-in release key, or `None` when this build carries none.
pub fn release_key() -> Option<[u8; 32]> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(RELEASE_KEY.trim()).ok()?;
    raw.try_into().ok()
}

/// A release newer than the running one, with its verified manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub version: Version,
    pub manifest: Manifest,
    pub release: LatestRelease,
    pub platform: Platform,
}

impl Offer {
    pub fn asset(&self) -> Option<&manifest::Asset> {
        self.manifest.asset(self.platform)
    }
}

/// One copy's updater: where releases come from, where its state lives, what it is.
#[derive(Clone, Debug)]
pub struct Engine {
    pub ep: Endpoints,
    pub run_root: PathBuf,
    pub platform: Platform,
    pub current: Version,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Engine {
    pub fn dir(&self) -> PathBuf {
        state::dir(&self.run_root)
    }

    fn manifest_cache(&self, tag: &str) -> (PathBuf, PathBuf) {
        let d = self.dir().join("manifest");
        (d.join(format!("{tag}.json")), d.join(format!("{tag}.json.sig")))
    }

    pub fn downloads(&self) -> PathBuf {
        self.dir().join("downloads")
    }

    pub fn media_cache(&self) -> PathBuf {
        self.dir().join("media")
    }

    /// Ask for the latest release, recording the attempt, its schedule and its tag in `st`.
    /// `Ok(None)` means nothing newer than the running version.
    pub fn check(&self, st: &mut State) -> Result<Option<Offer>, Error> {
        let now = now_unix();
        let result = self.check_inner(st);
        st.last_check = Some(now);
        match &result {
            Ok(_) => {
                st.failures = 0;
                st.retry_at = None;
            }
            Err(Error::RateLimited { retry_at }) => {
                st.failures = st.failures.saturating_add(1);
                st.retry_at = retry_at.filter(|t| *t > now);
            }
            Err(_) => st.failures = st.failures.saturating_add(1),
        }
        result
    }

    fn check_inner(&self, st: &mut State) -> Result<Option<Offer>, Error> {
        let etag = st.release.as_ref().and(st.etag.as_deref());
        let release = match client::latest(&self.ep, etag)? {
            client::Check::NotModified => match st.release.clone() {
                Some(r) => r,
                None => return Err(Error::Malformed("not-modified without a cached release".into())),
            },
            client::Check::Release { release, etag } => {
                st.etag = etag;
                st.release = Some(release.clone());
                release
            }
        };
        let Some(tagged) = Version::parse(&release.tag) else {
            return Err(Error::Malformed(format!("release tag {} is not a version", release.tag)));
        };
        if tagged <= self.current {
            self.prune(None);
            return Ok(None);
        }
        let (manifest, version) = self.verified_manifest(&release)?;
        if !policy::offer_unskipped(&version, &self.current, st.skip.as_deref()) {
            return Ok(None);
        }
        let offer = Offer { version, manifest, release, platform: self.platform };
        let Some(asset) = offer.asset() else { return Err(Error::NoAsset) };
        client::asset_for(&self.ep, &offer.release, asset)?;
        self.prune(Some(&asset.name));
        Ok(Some(offer))
    }

    fn verified_manifest(&self, release: &LatestRelease) -> Result<(Manifest, Version), Error> {
        let (json, sig) = self.manifest_cache(&release.tag);
        if let (Ok(body), Ok(signature)) = (std::fs::read(&json), std::fs::read(&sig)) {
            if let Ok((m, v)) = manifest::verify(&body, &signature, &self.ep.key) {
                if m.tag == release.tag {
                    return Ok((m, v));
                }
            }
        }
        let signed = client::manifest(&self.ep, release)?;
        if let Some(dir) = json.parent() {
            let _ = std::fs::remove_dir_all(dir);
            if std::fs::create_dir_all(dir).is_ok() {
                let _ = std::fs::write(&json, &signed.body);
                let _ = std::fs::write(&sig, &signed.signature);
            }
        }
        let (m, v) = (signed.manifest, signed.version);
        Ok((m, v))
    }

    /// Remove downloaded packages other than `keep`.
    fn prune(&self, keep: Option<&str>) {
        let Ok(entries) = std::fs::read_dir(self.downloads()) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let base = name.strip_suffix(".part").unwrap_or(&name);
            if Some(base) != keep {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    pub fn package_path(&self, offer: &Offer) -> Option<PathBuf> {
        offer.asset().map(|a| self.downloads().join(&a.name))
    }

    /// Whether the package for `offer` is already downloaded and verified.
    pub fn package_ready(&self, offer: &Offer) -> bool {
        match (offer.asset(), self.package_path(offer)) {
            (Some(a), Some(p)) => std::fs::metadata(&p).is_ok_and(|m| m.len() == a.size) && client::sha256_file(&p).is_ok_and(|h| h == a.sha256),
            _ => false,
        }
    }

    /// Bytes already on disk toward the package (complete or partial).
    pub fn package_progress(&self, offer: &Offer) -> u64 {
        self.package_path(offer).map_or(0, |p| {
            std::fs::metadata(&p).or_else(|_| std::fs::metadata(client::partial_path(&p))).map_or(0, |m| m.len())
        })
    }

    /// Download and verify the package for `offer`.
    pub fn download(&self, offer: &Offer, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<PathBuf, Error> {
        let asset = offer.asset().ok_or(Error::NoAsset)?;
        let remote = client::asset_for(&self.ep, &offer.release, asset)?;
        let dest = self.downloads().join(&asset.name);
        client::download(&remote.url, &dest, asset.size, &asset.sha256, progress, cancel)?;
        Ok(dest)
    }

    /// The notes media for `offer`, each verified; failures are left out.
    pub fn media(&self, offer: &Offer) -> Vec<(manifest::MediaFile, Vec<u8>)> {
        offer
            .manifest
            .media
            .iter()
            .filter_map(|m| client::media(&self.ep, &offer.manifest.tag, m, &self.media_cache()).ok().map(|b| (m.clone(), b)))
            .collect()
    }

    /// Re-verify the downloaded package, build the handoff and record it as pending in `st`.
    pub fn prepare_install(&self, offer: &Offer, install_dir: &Path, pid: u32, relaunch: bool, st: &mut State) -> Result<install::Handoff, Error> {
        let asset = offer.asset().ok_or(Error::NoAsset)?;
        let package = self.package_path(offer).ok_or(Error::NoAsset)?;
        if !self.package_ready(offer) {
            return Err(Error::HashMismatch);
        }
        let kept = guard::kept_dir(&self.run_root, &self.current.to_string());
        guard::keep_previous(self.platform, install_dir, &package, &kept).map_err(|e| match e.raw_os_error() {
            Some(112 | 39 | 28) => Error::DiskFull,
            _ => Error::Io(format!("previous version not kept: {e}")),
        })?;
        let work = self.dir().join("handoff");
        let _ = std::fs::remove_dir_all(&work);
        let handoff = install::plan(&install::Inputs {
            platform: self.platform,
            package: &package,
            install_dir,
            work: &work,
            package_sha256: &asset.sha256,
            pid,
            relaunch,
        })
        .map_err(|e| Error::Io(e.to_string()))?;
        st.pending = Some(state::Pending {
            from: self.current.to_string(),
            to: offer.version.to_string(),
            started: now_unix(),
            outcome: install::outcome_path(&work).to_string_lossy().into_owned(),
        });
        guard::begin_probation(st, &self.current, &offer.version, &kept);
        Ok(handoff)
    }
}
