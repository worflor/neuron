// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Handing a verified package to the install lifecycle that already owns it.
//!
//! Neuron never copies its own files. Windows Setup installs through the setup program, which
//! signals the running tray's shutdown event, installs (including the protected Chroma broker)
//! and restarts it. A portable copy runs the release's own `neuron-update` script from inside the
//! verified archive, after this process has exited; a small detached wrapper waits for that exit,
//! runs the script with `-NoRelaunch`, records its exit code and relaunches whichever version is
//! then in place.

use super::manifest::Platform;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const WINDOWS_SCRIPT: &str = "skills/neuron-lazy-update/scripts/neuron-update.ps1";
pub const LINUX_SCRIPT: &str = "skills/neuron-lazy-update/scripts/neuron-update.sh";
const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

/// A process to start detached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Whether Neuron quits by itself after starting it (the portable wrapper waits for that);
    /// Windows Setup instead signals the shutdown event when it is ready.
    pub self_quit: bool,
}

/// Paths a handoff is built from.
#[derive(Clone, Debug)]
pub struct Inputs<'a> {
    pub platform: Platform,
    /// The verified package on disk.
    pub package: &'a Path,
    /// Directory of the running executable.
    pub install_dir: &'a Path,
    /// Scratch directory for the extracted script, the sums file, the log and the outcome.
    pub work: &'a Path,
    pub package_sha256: &'a str,
    pub pid: u32,
    /// Start the app again once the portable script has finished.
    pub relaunch: bool,
}

pub fn outcome_path(work: &Path) -> PathBuf {
    work.join("outcome.txt")
}

pub fn log_path(work: &Path) -> PathBuf {
    work.join("apply.log")
}

fn ps_literal(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "''"))
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

fn windows_powershell() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe")
}

/// Copy the update script out of the verified archive. The archive's hash is signed, so its
/// script is the release's own.
pub fn extract_script(platform: Platform, package: &Path, work: &Path) -> std::io::Result<PathBuf> {
    let (suffix, leaf) = match platform {
        Platform::WindowsPortable => (WINDOWS_SCRIPT, "neuron-update.ps1"),
        Platform::LinuxPortable => (LINUX_SCRIPT, "neuron-update.sh"),
        Platform::WindowsSetup => return Err(std::io::Error::other("setup carries no update script")),
    };
    let mut bytes = Vec::new();
    let mut found = 0;
    let mut take = |name: &str, reader: &mut dyn Read| -> std::io::Result<()> {
        let name = name.replace('\\', "/");
        if name.ends_with(suffix) && !name.split('/').any(|s| s == "..") {
            found += 1;
            bytes.clear();
            reader.take(MAX_SCRIPT_BYTES).read_to_end(&mut bytes)?;
        }
        Ok(())
    };
    if platform == Platform::WindowsPortable {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(package)?).map_err(std::io::Error::other)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(std::io::Error::other)?;
            let name = entry.name().to_string();
            take(&name, &mut entry)?;
        }
    } else {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(package)?);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            take(&name, &mut entry)?;
        }
    }
    if found != 1 || bytes.is_empty() {
        return Err(std::io::Error::other("the package does not carry exactly one update script"));
    }
    std::fs::create_dir_all(work)?;
    let out = work.join(leaf);
    std::fs::write(&out, &bytes)?;
    Ok(out)
}

/// Build the handoff for `inputs`. For a portable copy this extracts the script and writes the
/// checksum file the script verifies the archive against.
pub fn plan(inputs: &Inputs<'_>) -> std::io::Result<Handoff> {
    std::fs::create_dir_all(inputs.work)?;
    let _ = std::fs::remove_file(outcome_path(inputs.work));
    match inputs.platform {
        Platform::WindowsSetup => {
            let mut dir = OsString::from("/DIR=");
            dir.push(inputs.install_dir.as_os_str());
            let mut log = OsString::from("/LOG=");
            log.push(log_path(inputs.work).as_os_str());
            Ok(Handoff {
                program: inputs.package.to_path_buf(),
                args: vec!["/VERYSILENT".into(), "/SUPPRESSMSGBOXES".into(), "/NORESTART".into(), dir, log],
                self_quit: false,
            })
        }
        Platform::WindowsPortable => {
            let script = extract_script(inputs.platform, inputs.package, inputs.work)?;
            let sums = write_sums(inputs)?;
            let exe = inputs.install_dir.join("neuron-app.exe");
            let body = [
                "$ErrorActionPreference='Continue'".to_string(),
                format!("try {{ Wait-Process -Id {} -Timeout 120 -ErrorAction SilentlyContinue }} catch {{ }}", inputs.pid),
                "$code=1".into(),
                format!(
                    "try {{ & {} -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {} -Action apply -InstallDir {} -ZipPath {} -SumsPath {} -NoRelaunch *> {}; $code=$LASTEXITCODE }} catch {{ $code=1 }}",
                    ps_literal(&windows_powershell()),
                    ps_literal(&script),
                    ps_literal(inputs.install_dir),
                    ps_literal(inputs.package),
                    ps_literal(&sums),
                    ps_literal(&log_path(inputs.work)),
                ),
                format!("[IO.File]::WriteAllText({}, [string]$code)", ps_literal(&outcome_path(inputs.work))),
                format!("$exe={}", ps_literal(&exe)),
                "$alive=@(Get-Process -Name neuron-app -ErrorAction SilentlyContinue | Where-Object { try { $_.Path -ieq $exe } catch { $false } })".into(),
                format!("if ($alive.Count -eq 0) {{ Start-Process -FilePath $exe -WorkingDirectory {} }}", ps_literal(inputs.install_dir)),
            ]
            .join("\n");
            let utf16: Vec<u8> = body.encode_utf16().flat_map(u16::to_le_bytes).collect();
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
            Ok(Handoff {
                program: windows_powershell(),
                args: ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-EncodedCommand"]
                    .into_iter()
                    .map(OsString::from)
                    .chain(std::iter::once(OsString::from(encoded)))
                    .collect(),
                self_quit: true,
            })
        }
        Platform::LinuxPortable => {
            let script = extract_script(inputs.platform, inputs.package, inputs.work)?;
            let sums = write_sums(inputs)?;
            let app = inputs.install_dir.join("neuron-app");
            let body = format!(
                "trap '' HUP; while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; \
                 bash {script} --action apply --install-dir {dir} --archive {pkg} --sums {sums} > {log} 2>&1; \
                 echo $? > {out}; \
                 if {relaunch} && ! pgrep -x neuron-app >/dev/null 2>&1; then cd {dir} && nohup {app} >/dev/null 2>&1 & fi",
                pid = inputs.pid,
                script = sh_quote(&script),
                dir = sh_quote(inputs.install_dir),
                pkg = sh_quote(inputs.package),
                sums = sh_quote(&sums),
                log = sh_quote(&log_path(inputs.work)),
                out = sh_quote(&outcome_path(inputs.work)),
                app = sh_quote(&app),
                relaunch = if inputs.relaunch { "true" } else { "false" },
            );
            Ok(Handoff { program: PathBuf::from("/bin/sh"), args: vec!["-c".into(), body.into()], self_quit: true })
        }
    }
}

fn write_sums(inputs: &Inputs<'_>) -> std::io::Result<PathBuf> {
    let name = inputs.package.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sums = inputs.work.join("SHA256SUMS.txt");
    std::fs::write(&sums, format!("{}  {name}\n", inputs.package_sha256))?;
    Ok(sums)
}

/// Start `h` so it outlives this process.
pub fn spawn(h: &Handoff) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(&h.program);
    cmd.args(&h.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = cmd.spawn() {
            return Ok(child);
        }
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

/// What a previous handoff came to, read at the next launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landed {
    Updated(String),
    Failed { to: String, reason: String },
}

/// Compare the running version with a recorded handoff.
pub fn landed(pending: &super::state::Pending, running: &super::version::Version) -> Landed {
    let to = super::version::Version::parse(&pending.to);
    if to.as_ref() == Some(running) {
        return Landed::Updated(pending.to.clone());
    }
    let work = Path::new(&pending.outcome).parent().map(Path::to_path_buf);
    let code = std::fs::read_to_string(&pending.outcome).ok().and_then(|s| s.trim().parse::<i32>().ok());
    let flag = work.as_deref().and_then(|w| std::fs::read(log_path(w)).ok()).and_then(|b| last_flag(&decode_log(&b)));
    let reason = match (flag, code) {
        (Some(f), _) => f,
        (None, Some(c)) => setup_exit_reason(c),
        (None, None) => "install did not take".into(),
    };
    Landed::Failed { to: pending.to.clone(), reason }
}

/// PowerShell's `*>` writes UTF-16; bash writes UTF-8.
fn decode_log(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The last `FLAG: CODE - text` an update script printed, as `code` in a few words.
pub fn last_flag(log: &str) -> Option<String> {
    log.lines().rev().find_map(|l| l.trim().strip_prefix("FLAG: ")).and_then(|rest| rest.split_whitespace().next()).map(|code| code.to_ascii_lowercase().replace('_', " "))
}

/// Inno Setup exit codes in a few words.
pub fn setup_exit_reason(code: i32) -> String {
    match code {
        0 => "installed".into(),
        2 | 5 => "setup cancelled".into(),
        7 | 8 => "setup stopped before install".into(),
        other => format!("setup failed ({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::state::Pending;
    use crate::update::version::Version;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("neuron-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut w, body).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn setup_handoff_uses_the_silent_inno_flags_for_this_directory() {
        let work = scratch("setup");
        let dir = Path::new(r"C:\Users\Some One\AppData\Local\Programs\Neuron");
        let pkg = work.join("neuron-v0.1.8-windows-x86_64-setup.exe");
        let h = plan(&Inputs { platform: Platform::WindowsSetup, package: &pkg, install_dir: dir, work: &work, package_sha256: "", pid: 1, relaunch: false }).unwrap();
        assert_eq!(h.program, pkg);
        assert_eq!(&h.args[..3], &["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"].map(OsString::from));
        assert_eq!(h.args[3], OsString::from(format!("/DIR={}", dir.display())));
        assert!(!h.self_quit, "setup signals the running tray itself");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn portable_handoff_runs_the_archives_own_script_after_exit() {
        let work = scratch("portable");
        let pkg = work.join("neuron-v0.1.8-windows-x86_64.zip");
        zip_with(&pkg, &[
            ("neuron-v0.1.8-windows-x86_64/neuron-app.exe", b"MZ"),
            ("neuron-v0.1.8-windows-x86_64/skills/neuron-lazy-update/scripts/neuron-update.ps1", b"# the release script"),
        ]);
        let dir = work.join("it's installed");
        let h = plan(&Inputs { platform: Platform::WindowsPortable, package: &pkg, install_dir: &dir, work: &work.join("w"), package_sha256: &"d".repeat(64), pid: 4242, relaunch: true }).unwrap();
        assert!(h.self_quit);
        assert_eq!(std::fs::read(work.join("w").join("neuron-update.ps1")).unwrap(), b"# the release script");
        let sums = std::fs::read_to_string(work.join("w").join("SHA256SUMS.txt")).unwrap();
        assert_eq!(sums, format!("{}  neuron-v0.1.8-windows-x86_64.zip\n", "d".repeat(64)));
        use base64::Engine as _;
        let encoded = h.args.last().unwrap().to_string_lossy().into_owned();
        let raw = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(script.contains("Wait-Process -Id 4242"));
        assert!(script.contains("-Action apply") && script.contains("-NoRelaunch") && script.contains("-SumsPath"));
        assert!(script.contains("it''s installed"), "paths are PowerShell literals");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn an_archive_without_exactly_one_script_is_refused() {
        let work = scratch("noscript");
        let pkg = work.join("p.zip");
        zip_with(&pkg, &[("x/neuron-app.exe", b"MZ")]);
        assert!(extract_script(Platform::WindowsPortable, &pkg, &work).is_err());
        zip_with(&pkg, &[("a/skills/neuron-lazy-update/scripts/neuron-update.ps1", b"1"), ("b/skills/neuron-lazy-update/scripts/neuron-update.ps1", b"2")]);
        assert!(extract_script(Platform::WindowsPortable, &pkg, &work).is_err());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn linux_handoff_extracts_from_the_tarball() {
        let work = scratch("linux");
        let pkg = work.join("neuron-v0.1.8-linux-x86_64.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(std::fs::File::create(&pkg).unwrap(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let body = b"#!/bin/bash\necho apply\n";
            let mut hdr = tar::Header::new_gnu();
            hdr.set_size(body.len() as u64);
            hdr.set_mode(0o755);
            hdr.set_cksum();
            tar.append_data(&mut hdr, "neuron-v0.1.8-linux-x86_64/skills/neuron-lazy-update/scripts/neuron-update.sh", &body[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dir = work.join("app");
        let h = plan(&Inputs { platform: Platform::LinuxPortable, package: &pkg, install_dir: &dir, work: &work.join("w"), package_sha256: &"e".repeat(64), pid: 77, relaunch: true }).unwrap();
        assert_eq!(h.program, PathBuf::from("/bin/sh"));
        let body = h.args[1].to_string_lossy();
        assert!(body.contains("kill -0 77") && body.contains("--action apply") && body.contains("--archive"));
        assert_eq!(std::fs::read(work.join("w").join("neuron-update.sh")).unwrap(), b"#!/bin/bash\necho apply\n");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn the_next_launch_learns_whether_the_install_took() {
        let work = scratch("landed");
        let pending = Pending { from: "0.1.7".into(), to: "0.1.8".into(), started: 1, outcome: outcome_path(&work).to_string_lossy().into_owned() };
        assert_eq!(landed(&pending, &Version::parse("0.1.8").unwrap()), Landed::Updated("0.1.8".into()));
        let old = Version::parse("0.1.7").unwrap();
        assert_eq!(landed(&pending, &old), Landed::Failed { to: "0.1.8".into(), reason: "install did not take".into() });
        std::fs::write(outcome_path(&work), "7").unwrap();
        assert_eq!(landed(&pending, &old), Landed::Failed { to: "0.1.8".into(), reason: "setup stopped before install".into() });
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "install_dir: x\r\nFLAG: HASH_MISMATCH - the zip does not match\r\nRESULT: blocked\r\n".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(log_path(&work), utf16).unwrap();
        assert_eq!(landed(&pending, &old), Landed::Failed { to: "0.1.8".into(), reason: "hash mismatch".into() });
        let _ = std::fs::remove_dir_all(&work);
    }
}
