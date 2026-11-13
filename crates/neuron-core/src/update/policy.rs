// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! When the updater runs at all, what it may install over, and how often it asks.

use super::manifest::Platform;
use super::version::Version;
use std::path::Path;
use std::time::Duration;

pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(8);
pub const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 3600);
/// A dismissed version stays quiet this long, or until a newer one appears.
pub const DISMISS_QUIET: Duration = Duration::from_secs(3 * 86400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disabled {
    /// Steam owns updates for its copy.
    Steam,
    /// Built from a source checkout (a cargo `target` directory).
    SourceBuild,
    /// Not built by the release packaging, so there is no release it corresponds to.
    NotReleaseBuild,
    /// `NEURON_NO_UPDATE` is set.
    Env,
    /// Neither a Windows Setup install nor a marked portable copy.
    Unmanaged,
    /// No release signing key is compiled in.
    NoKey,
    /// No release package exists for this platform.
    Unsupported,
}

impl Disabled {
    pub fn describe(self) -> &'static str {
        match self {
            Disabled::Steam => "steam updates this copy",
            Disabled::SourceBuild => "source build: update with git",
            Disabled::NotReleaseBuild => "not a release build",
            Disabled::Env => "off: NEURON_NO_UPDATE",
            Disabled::Unmanaged => "unmanaged copy: update by hand",
            Disabled::NoKey => "no release key in this build",
            Disabled::Unsupported => "no release package for this platform",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Windows Setup (Inno) owns the directory.
    Installer,
    /// A release archive extracted with its `portable.flag`.
    Portable,
    SourceBuild,
    Unmanaged,
}

/// Classify the directory holding the running executable, the same way the update scripts do.
pub fn install_kind(exe_dir: &Path) -> InstallKind {
    let mut p = Some(exe_dir);
    while let Some(dir) = p {
        let is_target = dir.file_name().is_some_and(|n| n == "target") && dir.parent().is_some_and(|up| up.join("Cargo.toml").is_file());
        if is_target || dir.join(".fingerprint").is_dir() {
            return InstallKind::SourceBuild;
        }
        p = dir.parent();
    }
    if cfg!(windows) && exe_dir.join("unins000.exe").is_file() {
        InstallKind::Installer
    } else if exe_dir.join("portable.flag").is_file() {
        InstallKind::Portable
    } else {
        InstallKind::Unmanaged
    }
}

/// Steam runs its titles with these set, and ships `steam_appid.txt` beside a development copy.
pub fn steam_detected(exe_dir: &Path) -> bool {
    exe_dir.join("steam_appid.txt").is_file()
        || std::env::var_os("SteamAppId").is_some()
        || std::env::var_os("SteamGameId").is_some()
        || exe_dir.components().any(|c| c.as_os_str().eq_ignore_ascii_case("steamapps"))
}

/// The packaging scripts set `NEURON_RELEASE_BUILD` for the build they package.
pub const RELEASE_BUILD: bool = option_env!("NEURON_RELEASE_BUILD").is_some();

#[derive(Clone, Debug)]
pub struct Environment {
    pub release_build: bool,
    pub steam: bool,
    pub env_off: bool,
    pub has_key: bool,
    pub kind: InstallKind,
}

impl Environment {
    pub fn detect(exe_dir: &Path, steam_build: bool, has_key: bool) -> Self {
        Self {
            release_build: RELEASE_BUILD,
            steam: steam_build || steam_detected(exe_dir),
            env_off: std::env::var_os("NEURON_NO_UPDATE").is_some_and(|v| !v.is_empty() && v != "0"),
            has_key,
            kind: install_kind(exe_dir),
        }
    }
}

/// The release asset this copy updates from, or why it does not update itself.
pub fn availability(env: &Environment) -> Result<Platform, Disabled> {
    if env.steam {
        return Err(Disabled::Steam);
    }
    if env.env_off {
        return Err(Disabled::Env);
    }
    if env.kind == InstallKind::SourceBuild {
        return Err(Disabled::SourceBuild);
    }
    if !env.release_build {
        return Err(Disabled::NotReleaseBuild);
    }
    if !env.has_key {
        return Err(Disabled::NoKey);
    }
    platform_for(env.kind)
}

fn platform_for(kind: InstallKind) -> Result<Platform, Disabled> {
    match kind {
        InstallKind::Installer if cfg!(windows) => Ok(Platform::WindowsSetup),
        InstallKind::Portable if cfg!(windows) => Ok(Platform::WindowsPortable),
        InstallKind::Portable if cfg!(target_os = "linux") => Ok(Platform::LinuxPortable),
        InstallKind::Unmanaged | InstallKind::Installer => Err(Disabled::Unmanaged),
        InstallKind::SourceBuild => Err(Disabled::SourceBuild),
        InstallKind::Portable => Err(Disabled::Unsupported),
    }
}

/// Wait after `failures` consecutive failed checks: 15 min doubling to a 12 h ceiling.
pub fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return CHECK_INTERVAL;
    }
    let mins = 15u64.saturating_mul(1u64 << failures.saturating_sub(1).min(10));
    Duration::from_secs(mins.saturating_mul(60)).min(CHECK_INTERVAL)
}

/// Unix seconds of the next automatic check. `retry_at` (a rate limit's reset) wins when later.
pub fn next_check(last_check: Option<u64>, failures: u32, retry_at: Option<u64>, launched: u64) -> u64 {
    let due = match last_check {
        None => launched + FIRST_CHECK_DELAY.as_secs(),
        Some(t) => (t + backoff(failures).as_secs()).max(launched + FIRST_CHECK_DELAY.as_secs()),
    };
    due.max(retry_at.unwrap_or(0))
}

/// Whether `candidate` should be offered over `current`: strictly newer, and not a version the
/// person dismissed within [`DISMISS_QUIET`].
pub fn offer(candidate: &Version, current: &Version, dismissed: Option<(&Version, u64)>, now: u64) -> bool {
    if candidate <= current {
        return false;
    }
    match dismissed {
        Some((v, at)) if v == candidate => now.saturating_sub(at) >= DISMISS_QUIET.as_secs(),
        _ => true,
    }
}

/// `candidate` is newer than `current` and than a version that rolled back.
pub fn offer_unskipped(candidate: &Version, current: &Version, skip: Option<&str>) -> bool {
    candidate > current && skip.and_then(Version::parse).is_none_or(|s| candidate > &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn env(kind: InstallKind) -> Environment {
        Environment { release_build: true, steam: false, env_off: false, has_key: true, kind }
    }

    #[test]
    fn disabled_modes_win_in_order() {
        let mut e = env(InstallKind::Portable);
        assert!(availability(&e).is_ok());
        e.release_build = false;
        assert_eq!(availability(&e), Err(Disabled::NotReleaseBuild));
        e.kind = InstallKind::SourceBuild;
        assert_eq!(availability(&e), Err(Disabled::SourceBuild));
        e.env_off = true;
        assert_eq!(availability(&e), Err(Disabled::Env));
        e.steam = true;
        assert_eq!(availability(&e), Err(Disabled::Steam));
        let mut e = env(InstallKind::Unmanaged);
        assert_eq!(availability(&e), Err(Disabled::Unmanaged));
        e.kind = InstallKind::Portable;
        e.has_key = false;
        assert_eq!(availability(&e), Err(Disabled::NoKey));
    }

    #[test]
    fn platform_follows_the_install_kind() {
        let portable = availability(&env(InstallKind::Portable));
        if cfg!(windows) {
            assert_eq!(availability(&env(InstallKind::Installer)), Ok(Platform::WindowsSetup));
            assert_eq!(portable, Ok(Platform::WindowsPortable));
        } else if cfg!(target_os = "linux") {
            assert_eq!(portable, Ok(Platform::LinuxPortable));
            assert_eq!(availability(&env(InstallKind::Installer)), Err(Disabled::Unmanaged));
        } else {
            assert_eq!(portable, Err(Disabled::Unsupported));
        }
    }

    #[test]
    fn install_kind_reads_markers_and_cargo_targets() {
        let root = std::env::temp_dir().join(format!("neuron-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo_target = root.join("repo").join("target").join("release");
        std::fs::create_dir_all(&repo_target).unwrap();
        std::fs::write(root.join("repo").join("Cargo.toml"), "").unwrap();
        assert_eq!(install_kind(&repo_target), InstallKind::SourceBuild);
        let portable = root.join("portable");
        std::fs::create_dir_all(&portable).unwrap();
        assert_eq!(install_kind(&portable), InstallKind::Unmanaged);
        std::fs::write(portable.join("portable.flag"), "").unwrap();
        assert_eq!(install_kind(&portable), InstallKind::Portable);
        std::fs::write(portable.join("unins000.exe"), "").unwrap();
        let expect = if cfg!(windows) { InstallKind::Installer } else { InstallKind::Portable };
        assert_eq!(install_kind(&portable), expect);
        assert!(!steam_detected(&portable) || std::env::var_os("SteamAppId").is_some());
        std::fs::write(portable.join("steam_appid.txt"), "1").unwrap();
        assert!(steam_detected(&portable));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backoff_doubles_to_the_ceiling() {
        let mins: Vec<u64> = (1..=8).map(|f| backoff(f).as_secs() / 60).collect();
        assert_eq!(mins, vec![15, 30, 60, 120, 240, 480, 720, 720]);
        assert_eq!(backoff(0), CHECK_INTERVAL);
        assert_eq!(backoff(u32::MAX), CHECK_INTERVAL);
    }

    #[test]
    fn schedule_waits_after_launch_and_honours_rate_limits() {
        let launch = 1_000_000;
        assert_eq!(next_check(None, 0, None, launch), launch + 8);
        assert_eq!(next_check(Some(launch - 100), 0, None, launch), launch - 100 + 12 * 3600);
        assert_eq!(next_check(Some(launch - 13 * 3600), 0, None, launch), launch + 8, "overdue checks wait out the launch delay");
        assert_eq!(next_check(Some(launch), 2, None, launch), launch + 30 * 60);
        assert_eq!(next_check(None, 0, Some(launch + 3600), launch), launch + 3600);
    }

    #[test]
    fn offers_only_newer_and_respects_dismissal() {
        let now = 10_000_000;
        assert!(offer(&v("0.1.8"), &v("0.1.7"), None, now));
        assert!(!offer(&v("0.1.7"), &v("0.1.7"), None, now), "same version");
        assert!(!offer(&v("0.1.6"), &v("0.1.7"), None, now), "downgrade");
        assert!(!offer(&v("0.1.8-rc.1"), &v("0.1.8"), None, now), "pre-release of the running version");
        assert!(offer(&v("0.1.8"), &v("0.1.8-rc.1"), None, now));
        assert!(!offer(&v("0.1.8"), &v("0.1.7"), Some((&v("0.1.8"), now - 60)), now), "dismissed recently");
        assert!(offer(&v("0.1.8"), &v("0.1.7"), Some((&v("0.1.8"), now - 4 * 86400)), now), "quiet period over");
        assert!(offer(&v("0.1.9"), &v("0.1.7"), Some((&v("0.1.8"), now - 60)), now), "a newer version");
    }
}
