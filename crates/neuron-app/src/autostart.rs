//! Start-with-Windows — via the Scheduled Task "Neuron (elevated tray)" (RunLevel Highest),
//! NOT an HKCU Run-key entry. Elevation is load-bearing: the native Chroma SHM server creates
//! `Global\` objects (SeCreateGlobalPrivilege), and an unelevated autostart silently degrades
//! every boot to REST-only — the exact failure the Run-key launcher used to cause. The task is
//! also the launch vehicle release.ps1 and manual relaunches go through (`schtasks /run`), so
//! MANUAL re-registers it WITHOUT the logon trigger — never disabled (a disabled task can't
//! be `/run`), never deleted.
//!
//! Managing a HighestAvailable task needs an elevated caller. The resident instance IS
//! elevated (it was launched by this very task); an unelevated instance gets an honest
//! "access denied" status line and the selector snaps back via `launch_mode_now()`.

#[cfg(windows)]
const TASK: &str = "Neuron (elevated tray)";
#[cfg(windows)]
const LEGACY_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const LEGACY_VALUE: &str = "Neuron";

/// Minimal XML text escaping — Windows paths and account names can legally contain `&`
/// (and quotes in odd corners); raw interpolation would make the task XML unparseable.
#[cfg(windows)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The full task definition, regenerated for the CURRENT exe. XML (not `schtasks /create`
/// flags) because the flags cannot express `ExecutionTimeLimit PT0S` — the default there is
/// 72 HOURS, after which the scheduler kills the resident app mid-session. `with_logon`
/// carries the autostart intent: BOOT modes register the logon trigger, MANUAL registers an
/// empty trigger set (the task stays runnable on demand as the elevation vehicle).
#[cfg(windows)]
fn task_xml(with_logon: bool) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let workdir = xml_escape(&exe.parent()?.to_string_lossy());
    let exe = xml_escape(&exe.to_string_lossy());
    let user = xml_escape(&format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").ok()?,
        std::env::var("USERNAME").ok()?
    ));
    let triggers = if with_logon {
        format!(
            "<Triggers>\n    <LogonTrigger>\n      <Delay>PT15S</Delay>\n      <UserId>{user}</UserId>\n    </LogonTrigger>\n  </Triggers>"
        )
    } else {
        "<Triggers />".into()
    };
    Some(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <URI>\{TASK}</URI>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <StartWhenAvailable>true</StartWhenAvailable>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
  </Settings>
  {triggers}
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--tray</Arguments>
      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

/// Register (or re-register) the task with or without its logon trigger.
#[cfg(windows)]
fn register_task(with_logon: bool) -> Result<(), String> {
    let Some(xml) = task_xml(with_logon) else {
        return Err("could not resolve exe/user for the task".into());
    };
    let tmp = std::env::temp_dir().join("neuron-autostart-task.xml");
    // UTF-16LE with BOM, matching the XML declaration.
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        return Err(format!("task xml write failed: {e}"));
    }
    let (ok, err) = schtasks(&["/create", "/tn", TASK, "/xml", &tmp.to_string_lossy(), "/f"]);
    let _ = std::fs::remove_file(&tmp);
    if ok {
        Ok(())
    } else {
        Err(err)
    }
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> (bool, String) {
    match std::process::Command::new("schtasks").args(args).output() {
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            (o.status.success(), err)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Remove the RETIRED HKCU Run-key launcher if it still exists. It launched UNELEVATED (native
/// Chroma dead every boot) and beside the task it is a DUPLICATE startup. Ours to delete: the
/// value name is our own. `None` = nothing to reap; `Some(deleted)` reports the DELETE's truth,
/// not the attempt's — a surviving key is the exact failure mode this module exists to
/// eliminate, so it must read as failure everywhere up the chain.
#[cfg(windows)]
pub fn reap_legacy_run_key() -> Option<bool> {
    let present = std::process::Command::new("reg")
        .args(["query", LEGACY_RUN_KEY, "/v", LEGACY_VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !present {
        return None;
    }
    let deleted = std::process::Command::new("reg")
        .args(["delete", LEGACY_RUN_KEY, "/v", LEGACY_VALUE, "/f"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if deleted {
        eprintln!("neuron: removed the retired unelevated Run-key launcher (the task owns startup)");
    } else {
        eprintln!(
            "neuron: FAILED to remove the retired Run-key launcher — startup will \
             double-launch (unelevated) until HKCU\\...\\Run value 'Neuron' is deleted"
        );
    }
    Some(deleted)
}

/// Migrate a legacy Run-key install to the task, PRESERVING the autostart intent:
/// replace-then-remove. Deleting the key first would strand the user with NO autostart when
/// task registration needs elevation this process doesn't have (and a Run-key launch is
/// exactly the unelevated case). If the task can't be registered, the key STAYS (no duplicate
/// exists — the task is absent/unhooked) and the migration retries on a later, elevated run.
#[cfg(windows)]
pub fn migrate_legacy_run_key() {
    let present = std::process::Command::new("reg")
        .args(["query", LEGACY_RUN_KEY, "/v", LEGACY_VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !present {
        return;
    }
    // The task counts as already-holding-the-intent only when its logon trigger is live AND
    // it targets THIS binary — a trigger on a stale/debug target would let the migration
    // delete the working fallback while boots launch the wrong exe. Anything less: re-register
    // (idempotent, also self-heals the stale target) and reap only on success.
    let healthy = is_enabled()
        && task_command()
            .zip(std::env::current_exe().ok())
            .map(|(cmd, exe)| {
                cmd.eq_ignore_ascii_case(&exe.to_string_lossy())
            })
            .unwrap_or(false);
    if healthy || register_task(true).is_ok() {
        // reap prints its own failure detail; only a confirmed delete is a completed migration.
        if reap_legacy_run_key() == Some(true) {
            eprintln!("neuron: migrated autostart from the Run key to the elevated task");
        }
    } else {
        eprintln!(
            "neuron: legacy Run-key autostart found but the elevated task couldn't be \
             registered from this (unelevated) instance — keeping the key so autostart \
             survives; toggle start-with-Windows from an elevated Neuron to finish migrating"
        );
    }
}

/// Enable or disable autostart (the task's logon trigger). Returns a status line. Both
/// directions RE-REGISTER the task for the current exe — which also self-heals a task
/// pointing at a moved binary.
#[cfg(windows)]
pub fn set(enabled: bool) -> String {
    match register_task(enabled) {
        Ok(()) => {
            // reap only AFTER the task holds the user's intent — deleting the legacy launcher
            // on a FAILED registration would erase their autostart instead of migrating it.
            let mut msg: String = if enabled {
                "start-with-Windows enabled (elevated task)".into()
            } else {
                "start-with-Windows off (task kept for manual/elevated launches)".into()
            };
            if reap_legacy_run_key() == Some(false) {
                msg.push_str(
                    " — WARNING: the old Run-key launcher survived and will double-launch \
                     unelevated; delete HKCU\\...\\Run value 'Neuron' by hand",
                );
            }
            msg
        }
        Err(err) if err.to_lowercase().contains("denied") => {
            "autostart needs the elevated instance — start Neuron via the tray task and retry"
                .into()
        }
        Err(err) => format!("failed to update the startup task: {err}"),
    }
}

/// The registered task's XML, decoded. schtasks emits UTF-16 on some hosts and codepage
/// bytes on others; decode both ways and take whichever parsed.
#[cfg(windows)]
fn query_task_xml() -> Option<String> {
    let o = std::process::Command::new("schtasks")
        .args(["/query", "/tn", TASK, "/xml"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let as_utf8 = String::from_utf8_lossy(&o.stdout).to_string();
    Some(if as_utf8.contains("<Task") {
        as_utf8
    } else {
        let wide: Vec<u16> = o
            .stdout
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&wide)
    })
}

/// The exe the task actually launches (its `<Command>`), entities decoded.
#[cfg(windows)]
fn task_command() -> Option<String> {
    let xml = query_task_xml()?;
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = xml[start..].find("</Command>")? + start;
    let cmd = xml[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&"); // last — the others may decode INTO an ampersand
    Some(cmd.trim().to_string())
}

/// Whether autostart is on: the task exists AND it carries the logon trigger. Parsed from the
/// task XML (locale-stable — `/fo list` prints localized field names).
#[cfg(windows)]
pub fn is_enabled() -> bool {
    match query_task_xml() {
        Some(xml) => xml.contains("<LogonTrigger>") && !xml.contains("<Enabled>false</Enabled>"),
        None => false,
    }
}

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> String {
    "autostart: Windows-only".into()
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn reap_legacy_run_key() -> bool {
    false
}
