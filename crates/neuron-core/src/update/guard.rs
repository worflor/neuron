// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Never brick on update. Before a handoff the running version keeps a copy of its own program
//! files (never config). The new version is on probation until it has run [`HEALTHY_AFTER`] with
//! its window, tray and dispatch up; each launch that ends without a clean exit before then counts
//! against it, and after [`MAX_UNPROVEN_STARTS`] such launches the next one restores the kept copy,
//! records the version as skipped, and starts the previous version again.

use super::install::Handoff;
use super::manifest::Platform;
use super::state::{dir, Probation, RolledBack, State};
use super::version::Version;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);
pub const MAX_UNPROVEN_STARTS: u32 = 2;

pub fn kept_dir(run_root: &Path, version: &str) -> PathBuf {
    dir(run_root).join("previous").join(version)
}

/// Relative paths of the files a release package ships, top-level folder stripped.
pub fn package_payload(platform: Platform, package: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    match platform {
        Platform::WindowsPortable => {
            let mut zip = zip::ZipArchive::new(std::fs::File::open(package)?).map_err(std::io::Error::other)?;
            for i in 0..zip.len() {
                let e = zip.by_index(i).map_err(std::io::Error::other)?;
                if e.is_file() {
                    names.push(e.name().replace('\\', "/"));
                }
            }
        }
        Platform::LinuxPortable => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(package)?));
            for e in tar.entries()? {
                let e = e?;
                if e.header().entry_type().is_file() {
                    names.push(e.path()?.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        Platform::WindowsSetup => return Err(std::io::Error::other("setup has no file list")),
    }
    Ok(names
        .into_iter()
        .filter_map(|n| n.split_once('/').map(|(_, rest)| rest.to_string()))
        .filter(|rel| !rel.is_empty() && rel.split('/').all(|s| !s.is_empty() && s != "." && s != ".."))
        .collect())
}

fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to).map(|_| ())
}

fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        if skip(&path) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&path, &to.join(entry.file_name()), skip)?;
        } else if kind.is_file() {
            copy_file(&path, &to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Copy the running version's program files to `dest`. A portable copy keeps exactly the paths
/// the incoming package will overwrite (its config sits beside them and is left alone); a Setup
/// install keeps its whole program directory, which holds no config. The copy is staged and
/// renamed into place, so a partial copy is never mistaken for a complete one.
pub fn keep_previous(platform: Platform, install_dir: &Path, package: &Path, dest: &Path) -> std::io::Result<()> {
    let stage = dest.with_extension("staging");
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(&stage)?;
    let result = (|| -> std::io::Result<()> {
        if platform == Platform::WindowsSetup {
            copy_tree(install_dir, &stage, &|p: &Path| p.file_name().is_some_and(|n| n == ".neuron-update-backup"))
        } else {
            for rel in package_payload(platform, package)? {
                let src = install_dir.join(&rel);
                if src.is_file() {
                    copy_file(&src, &stage.join(&rel))?;
                }
            }
            Ok(())
        }
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    std::fs::rename(&stage, dest)
}

/// What this launch should do about a pending probation.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    /// This version is still proving itself.
    Probation,
    /// Exit now and let `Handoff` restore the previous version.
    RollBack(Handoff),
}

/// Run at the very start of a launch, before any device is touched.
pub fn on_launch(run_root: &Path, current: &Version, install_dir: &Path, pid: u32, crash_log: &Path, relaunch: bool) -> Verdict {
    let mut st = State::load(run_root);
    let Some(mut p) = st.probation.take() else { return Verdict::Continue };
    if Version::parse(&p.version).as_ref() != Some(current) {
        // the install did not land, or this is the restored version: nothing to prove
        if st.rolled_back.is_none() {
            let _ = std::fs::remove_dir_all(&p.kept);
        }
        let _ = st.save(run_root);
        return Verdict::Continue;
    }
    p.starts += 1;
    if p.starts <= MAX_UNPROVEN_STARTS {
        st.probation = Some(p);
        let _ = st.save(run_root);
        return Verdict::Probation;
    }
    let kept = PathBuf::from(&p.kept);
    if !kept.is_dir() {
        let _ = st.save(run_root);
        return Verdict::Continue;
    }
    let crash_copy = dir(run_root).join(format!("failed-{}-crash.log", p.version));
    let kept_log = std::fs::copy(crash_log, &crash_copy).is_ok();
    let work = dir(run_root).join("restore");
    let Ok(handoff) = restore_plan(&kept, install_dir, &work, pid, relaunch) else {
        st.probation = Some(p);
        let _ = st.save(run_root);
        return Verdict::Probation;
    };
    st.skip = Some(p.version.clone());
    st.rolled_back = Some(RolledBack {
        from: p.version.clone(),
        to: p.previous.clone(),
        crash_log: if kept_log { crash_copy.to_string_lossy().into_owned() } else { String::new() },
        kept: p.kept.clone(),
    });
    st.pending = None;
    st.fresh_install = None;
    if st.save(run_root).is_err() {
        return Verdict::Probation;
    }
    Verdict::RollBack(handoff)
}

/// A launch that exits cleanly before proving itself does not count against it.
pub fn clean_exit(run_root: &Path, current: &Version) {
    let mut st = State::load(run_root);
    if let Some(p) = st.probation.as_mut() {
        if Version::parse(&p.version).as_ref() == Some(current) {
            p.starts = p.starts.saturating_sub(1);
            let _ = st.save(run_root);
        }
    }
}

/// The running version has proved itself: end probation and delete the kept copy.
pub fn proved_healthy(run_root: &Path, current: &Version) -> bool {
    let mut st = State::load(run_root);
    let Some(p) = st.probation.as_ref() else { return false };
    if Version::parse(&p.version).as_ref() != Some(current) {
        return false;
    }
    let kept = PathBuf::from(&p.kept);
    st.probation = None;
    if st.save(run_root).is_err() {
        return false;
    }
    let _ = std::fs::remove_dir_all(kept);
    true
}

/// After a rollback has run: the line to show once, and the kept copy cleaned up.
pub fn take_rollback_notice(run_root: &Path, current: &Version) -> Option<RolledBack> {
    let mut st = State::load(run_root);
    let rb = st.rolled_back.clone()?;
    if Version::parse(&rb.to).as_ref() != Some(current) {
        return None;
    }
    st.rolled_back = None;
    let _ = st.save(run_root);
    let _ = std::fs::remove_dir_all(&rb.kept);
    Some(rb)
}

fn ps_literal(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "''"))
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// A detached process that waits for `pid` to exit, copies `kept` over `install_dir`, records its
/// exit code in `work/outcome.txt` and, with `relaunch`, starts the restored app.
pub fn restore_plan(kept: &Path, install_dir: &Path, work: &Path, pid: u32, relaunch: bool) -> std::io::Result<Handoff> {
    std::fs::create_dir_all(work)?;
    let outcome = super::install::outcome_path(work);
    let _ = std::fs::remove_file(&outcome);
    if cfg!(windows) {
        let exe = install_dir.join("neuron-app.exe");
        let body = [
            "$ErrorActionPreference='Stop'".to_string(),
            format!("try {{ Wait-Process -Id {pid} -Timeout 120 -ErrorAction SilentlyContinue }} catch {{ }}"),
            format!("$kept={}; $dir={}; $code=0", ps_literal(kept), ps_literal(install_dir)),
            "try { $root=[IO.Path]::GetFullPath($kept).TrimEnd('\\'); Get-ChildItem -LiteralPath $root -Recurse -File -Force | ForEach-Object { $rel=$_.FullName.Substring($root.Length).TrimStart('\\'); $dest=Join-Path $dir $rel; $parent=Split-Path -Parent $dest; if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }; $n=0; while ($true) { try { Copy-Item -LiteralPath $_.FullName -Destination $dest -Force; break } catch { $n++; if ($n -ge 20) { throw }; Start-Sleep -Milliseconds 250 } } } } catch { $code=1 }".into(),
            format!("[IO.File]::WriteAllText({}, [string]$code)", ps_literal(&outcome)),
            if relaunch { format!("Start-Process -FilePath {} -WorkingDirectory $dir", ps_literal(&exe)) } else { String::new() },
        ]
        .join("\n");
        let utf16: Vec<u8> = body.encode_utf16().flat_map(u16::to_le_bytes).collect();
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        Ok(Handoff {
            program: root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"),
            args: ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-EncodedCommand"]
                .into_iter()
                .map(OsString::from)
                .chain(std::iter::once(OsString::from(encoded)))
                .collect(),
            self_quit: true,
        })
    } else {
        let body = format!(
            "trap '' HUP; while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; cp -a {kept}/. {dir}/; echo $? > {out}; \
             if {relaunch}; then cd {dir} && nohup ./neuron-app >/dev/null 2>&1 & fi",
            kept = sh_quote(kept),
            dir = sh_quote(install_dir),
            out = sh_quote(&outcome),
            relaunch = if relaunch { "true" } else { "false" },
        );
        Ok(Handoff { program: PathBuf::from("/bin/sh"), args: vec!["-c".into(), body.into()], self_quit: true })
    }
}

/// Record probation for `to` before a handoff, with the kept copy of `from`.
pub fn begin_probation(st: &mut State, from: &Version, to: &Version, kept: &Path) {
    st.probation = Some(Probation { version: to.to_string(), previous: from.to_string(), kept: kept.to_string_lossy().into_owned(), starts: 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("neuron-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    /// An install dir running the "new" version, a kept copy of the old one, and config beside.
    fn installed(root: &Path) -> (PathBuf, PathBuf) {
        let install = root.join("app");
        std::fs::create_dir_all(install.join("docs")).unwrap();
        std::fs::write(install.join("neuron-app.exe"), "new").unwrap();
        std::fs::write(install.join("docs").join("x.md"), "new docs").unwrap();
        std::fs::write(install.join("app.toml"), "config = 1").unwrap();
        let kept = kept_dir(root, "0.1.7");
        std::fs::create_dir_all(kept.join("docs")).unwrap();
        std::fs::write(kept.join("neuron-app.exe"), "old").unwrap();
        std::fs::write(kept.join("docs").join("x.md"), "old docs").unwrap();
        (install, kept)
    }

    #[test]
    fn a_version_that_never_proves_itself_rolls_back_and_is_skipped() {
        let root = scratch("rollback");
        let (install, kept) = installed(&root);
        let mut st = State::default();
        begin_probation(&mut st, &v("0.1.7"), &v("0.1.8"), &kept);
        st.save(&root).unwrap();
        let crash = root.join("neuron-crash.log");
        std::fs::write(&crash, "[panic] boom").unwrap();
        let new = v("0.1.8");
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Probation, "first start");
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Probation, "phoenix restart");
        let Verdict::RollBack(handoff) = on_launch(&root, &new, &install, 1, &crash, false) else { panic!("third start rolls back") };
        assert!(handoff.self_quit);
        let st = State::load(&root);
        assert_eq!(st.skip.as_deref(), Some("0.1.8"));
        assert!(st.probation.is_none());
        let rb = st.rolled_back.clone().unwrap();
        assert_eq!((rb.from.as_str(), rb.to.as_str()), ("0.1.8", "0.1.7"));
        assert_eq!(std::fs::read_to_string(&rb.crash_log).unwrap(), "[panic] boom", "the failed version's crash dump is kept");
        assert!(!crate::update::policy::offer_unskipped(&new, &v("0.1.7"), st.skip.as_deref()), "skipped until a newer one exists");
        assert!(crate::update::policy::offer_unskipped(&v("0.1.9"), &v("0.1.7"), st.skip.as_deref()));

        let mut child = crate::update::install::spawn(&handoff).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(crate::update::install::outcome_path(&root.join("update").join("restore"))).unwrap().trim(), "0");
        assert_eq!(std::fs::read_to_string(install.join("neuron-app.exe")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(install.join("docs").join("x.md")).unwrap(), "old docs");
        assert_eq!(std::fs::read_to_string(install.join("app.toml")).unwrap(), "config = 1", "config is never touched");

        let old = v("0.1.7");
        assert_eq!(on_launch(&root, &old, &install, 1, &crash, false), Verdict::Continue);
        let notice = take_rollback_notice(&root, &old).unwrap();
        assert_eq!(notice.from, "0.1.8");
        assert!(!kept.exists(), "the kept copy goes once the restore is confirmed");
        assert!(take_rollback_notice(&root, &old).is_none(), "said once");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_healthy_version_commits_and_drops_the_kept_copy() {
        let root = scratch("healthy");
        let (install, kept) = installed(&root);
        let mut st = State::default();
        begin_probation(&mut st, &v("0.1.7"), &v("0.1.8"), &kept);
        st.save(&root).unwrap();
        let new = v("0.1.8");
        let crash = root.join("none.log");
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Probation);
        clean_exit(&root, &new);
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Probation);
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Probation, "a clean exit did not count");
        assert!(proved_healthy(&root, &new));
        assert!(!kept.exists());
        assert!(State::load(&root).probation.is_none());
        assert_eq!(on_launch(&root, &new, &install, 1, &crash, false), Verdict::Continue);
        assert_eq!(std::fs::read_to_string(install.join("neuron-app.exe")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_that_did_not_land_ends_probation_quietly() {
        let root = scratch("unlanded");
        let (install, kept) = installed(&root);
        let mut st = State::default();
        begin_probation(&mut st, &v("0.1.7"), &v("0.1.8"), &kept);
        st.save(&root).unwrap();
        assert_eq!(on_launch(&root, &v("0.1.7"), &install, 1, &root.join("x"), false), Verdict::Continue);
        assert!(!kept.exists());
        assert!(State::load(&root).probation.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_portable_copy_keeps_only_what_the_package_replaces() {
        let root = scratch("keep");
        let install = root.join("app");
        std::fs::create_dir_all(install.join("skills")).unwrap();
        std::fs::write(install.join("neuron-app.exe"), "old").unwrap();
        std::fs::write(install.join("skills").join("s.md"), "old s").unwrap();
        std::fs::write(install.join("app.toml"), "config").unwrap();
        let pkg = root.join("p.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&pkg).unwrap());
            for name in ["neuron-v0.1.8-windows-x86_64/neuron-app.exe", "neuron-v0.1.8-windows-x86_64/skills/s.md", "neuron-v0.1.8-windows-x86_64/new.txt"] {
                w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                std::io::Write::write_all(&mut w, b"x").unwrap();
            }
            w.finish().unwrap();
        }
        let dest = kept_dir(&root, "0.1.7");
        keep_previous(Platform::WindowsPortable, &install, &pkg, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("neuron-app.exe")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(dest.join("skills").join("s.md")).unwrap(), "old s");
        assert!(!dest.join("app.toml").exists(), "config is not part of the kept copy");
        assert!(!dest.join("new.txt").exists());
        let setup_dest = kept_dir(&root, "setup");
        keep_previous(Platform::WindowsSetup, &install, &pkg, &setup_dest).unwrap();
        assert!(setup_dest.join("app.toml").exists(), "a Setup directory is kept whole");
        let _ = std::fs::remove_dir_all(&root);
    }
}
