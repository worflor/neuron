// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Start-with-Windows uses a least-privilege scheduled task. The historical task name is kept
//! while existing installs migrate, but its `RunLevel` must never be HighestAvailable: the app
//! executable and RAW macros are user-writable. Manual mode deletes the task.

#[cfg(windows)]
const TASK: &str = "Neuron (elevated tray)";
#[cfg(windows)]
const LEGACY_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const LEGACY_VALUE: &str = "Neuron";

/// Register the current executable without a writable intermediate task XML file. An elevated
/// app reading task XML from the user's temp directory would itself create a code-execution race.
#[cfg(windows)]
fn register_task() -> Result<(), String> {
    const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $env:NEURON_TASK_EXE -Argument '--tray' -WorkingDirectory $env:NEURON_TASK_DIR
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$trigger.Delay = 'PT15S'
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName 'Neuron (elevated tray)' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
";
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("exe has no parent directory")?;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("NEURON_TASK_EXE", &exe)
        .env("NEURON_TASK_DIR", dir)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(windows)]
fn delete_task() -> Result<(), String> {
    const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
try {
    Get-ScheduledTask -TaskName 'Neuron (elevated tray)' -ErrorAction Stop | Out-Null
} catch {
    if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { exit 0 }
    throw
}
Unregister-ScheduledTask -TaskName 'Neuron (elevated tray)' -Confirm:$false
";
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Retire an already-installed HighestAvailable task while its current instance still has the
/// rights to replace it. A disabled manual task is removed outright.
#[cfg(windows)]
pub fn retire_elevated_task() {
    if !task_is_elevated() {
        return;
    }
    let result = if is_enabled() { register_task() } else { delete_task() };
    match result {
        Ok(()) => eprintln!("neuron: retired elevated autostart task"),
        Err(err) => eprintln!("neuron: could not retire elevated autostart task: {err}"),
    }
}

/// Remove the RETIRED HKCU Run-key launcher if it still exists. Beside the task it is a duplicate
/// startup. Ours to delete: the
/// value name is our own. `None` = nothing to reap; `Some(deleted)` reports the DELETE's truth,
/// not the attempt's — a surviving key is the exact failure mode this module exists to
/// eliminate, so it must read as failure everywhere up the chain.
#[cfg(windows)]
pub fn reap_legacy_run_key() -> Option<bool> {
    let present = std::process::Command::new("reg")
        .args(["query", LEGACY_RUN_KEY, "/v", LEGACY_VALUE])
        .output()
        .is_ok_and(|o| o.status.success());
    if !present {
        return None;
    }
    let deleted = std::process::Command::new("reg")
        .args(["delete", LEGACY_RUN_KEY, "/v", LEGACY_VALUE, "/f"])
        .output()
        .is_ok_and(|o| o.status.success());
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

/// Migrate a legacy Run-key install while preserving autostart intent: register the limited task,
/// then remove the key. If task registration fails, the key stays.
#[cfg(windows)]
pub fn migrate_legacy_run_key() {
    let present = std::process::Command::new("reg")
        .args(["query", LEGACY_RUN_KEY, "/v", LEGACY_VALUE])
        .output()
        .is_ok_and(|o| o.status.success());
    if !present {
        return;
    }
    // The task counts as already-holding-the-intent only when its logon trigger is live AND
    // it targets THIS binary — a trigger on a stale/debug target would let the migration
    // delete the working fallback while boots launch the wrong exe. Anything less: re-register
    // (idempotent, also self-heals the stale target) and reap only on success.
    let healthy = query_task_xml()
        .zip(std::env::current_exe().ok())
        .and_then(|(xml, exe)| Some((xml, exe, current_user_sid()?)))
        .is_some_and(|(xml, exe, sid)| {
            task_is_healthy(&xml, &exe.to_string_lossy(), &sid)
        });
    if healthy || register_task().is_ok() {
        // reap prints its own failure detail; only a confirmed delete is a completed migration.
        if reap_legacy_run_key() == Some(true) {
            eprintln!("neuron: migrated autostart from the Run key to a limited task");
        }
    } else {
        eprintln!(
            "neuron: legacy Run-key autostart found but the limited task could not be \
             registered — keeping the key so autostart survives"
        );
    }
}

/// Enable or disable autostart. Manual mode deletes the task so it cannot remain an elevation
/// vehicle; boot mode registers the current executable with least privilege.
#[cfg(windows)]
pub fn set(enabled: bool) -> String {
    let result = if enabled { register_task() } else { delete_task() };
    match result {
        Ok(()) => {
            // reap only AFTER the task holds the user's intent — deleting the legacy launcher
            // on a FAILED registration would erase their autostart instead of migrating it.
            let mut msg: String = if enabled {
                "start-with-Windows enabled (limited task)".into()
            } else {
                "start-with-Windows off (task removed)".into()
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
            "task change denied; remove the old Neuron task with administrator permission, then retry"
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

#[cfg(windows)]
fn task_is_elevated() -> bool {
    query_task_xml().is_some_and(|xml| task_xml_is_elevated(&xml))
}

fn current_user_sid() -> Option<String> {
    const SCRIPT: &str = "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value";
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|sid| !sid.is_empty())
}

fn task_is_healthy(xml: &str, expected_command: &str, current_sid: &str) -> bool {
    task_xml_is_enabled(xml)
        && !task_xml_is_elevated(xml)
        && task_xml_command(xml)
            .is_some_and(|command| command.eq_ignore_ascii_case(expected_command))
        && task_principal_is_current_interactive(xml, current_sid)
}

fn task_xml_is_enabled(xml: &str) -> bool {
    xml.contains("<LogonTrigger>") && !xml.contains("<Enabled>false</Enabled>")
}

fn task_xml_is_elevated(xml: &str) -> bool {
    xml.contains("<RunLevel>HighestAvailable</RunLevel>")
}

fn task_xml_command(xml: &str) -> Option<String> {
    let cmd = xml_element(xml, "Command")?
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&"); // last — the others may decode INTO an ampersand
    Some(cmd.trim().to_string())
}

fn task_principal_is_current_interactive(xml: &str, current_sid: &str) -> bool {
    let Some(principal) = xml_block(xml, "Principal") else {
        return false;
    };
    xml_element(principal, "LogonType").is_some_and(|kind| kind.trim() == "InteractiveToken")
        && xml_element(principal, "UserId")
            .is_some_and(|user| user.trim().eq_ignore_ascii_case(current_sid))
}

fn xml_element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn xml_block<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("<{name}");
    let start = xml.match_indices(&prefix).find_map(|(i, _)| {
        let after = xml.as_bytes().get(i + prefix.len()).copied()?;
        (after.is_ascii_whitespace() || after == b'>').then_some(i)
    })?;
    let body_start = xml[start..].find('>')? + start + 1;
    let close = format!("</{name}>");
    let body_end = xml[body_start..].find(&close)? + body_start;
    Some(&xml[body_start..body_end])
}

/// Whether autostart is on: the task exists AND it carries the logon trigger. Parsed from the
/// task XML (locale-stable — `/fo list` prints localized field names).
#[cfg(windows)]
pub fn is_enabled() -> bool {
    query_task_xml().is_some_and(|xml| task_xml_is_enabled(&xml))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-100-200-300-1001";
    const HEALTHY_TASK: &str = r#"<Task>
  <Principals><Principal id="Author"><UserId>S-1-5-21-100-200-300-1001</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><Enabled>true</Enabled></Settings>
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Actions><Exec><Command>C:\Neuron\neuron-app.exe</Command></Exec></Actions>
</Task>"#;

    #[test]
    fn migration_accepts_current_users_interactive_limited_task() {
        assert!(task_is_healthy(
            HEALTHY_TASK,
            r"C:\Neuron\neuron-app.exe",
            SID
        ));
    }

    #[test]
    fn migration_rejects_wrong_user_and_noninteractive_principals() {
        let wrong_user = HEALTHY_TASK.replace(SID, "S-1-5-21-999-999-999-1002");
        assert!(!task_is_healthy(
            &wrong_user,
            r"C:\Neuron\neuron-app.exe",
            SID
        ));

        let noninteractive = HEALTHY_TASK.replace("InteractiveToken", "S4U");
        assert!(!task_is_healthy(
            &noninteractive,
            r"C:\Neuron\neuron-app.exe",
            SID
        ));
    }

    #[test]
    fn migration_still_rejects_disabled_elevated_or_stale_tasks() {
        assert!(!task_is_healthy(
            &HEALTHY_TASK.replace("<Enabled>true</Enabled>", "<Enabled>false</Enabled>"),
            r"C:\Neuron\neuron-app.exe",
            SID
        ));
        assert!(!task_is_healthy(
            &HEALTHY_TASK.replace("LeastPrivilege", "HighestAvailable"),
            r"C:\Neuron\neuron-app.exe",
            SID
        ));
        assert!(!task_is_healthy(
            HEALTHY_TASK,
            r"C:\Other\neuron-app.exe",
            SID
        ));
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
