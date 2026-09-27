// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

#[cfg(not(test))]
mod live {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use std::ffi::OsString;
    use std::io::Write;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_FAILED};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    static REQUESTED: AtomicBool = AtomicBool::new(false);
    const INSTALLER: &[u8] = include_bytes!("../../../packaging/windows/install-chroma-broker.ps1");

    struct TempScript(PathBuf);

    impl Drop for TempScript {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub fn request() {
        if std::env::args().any(|arg| arg == "--safe") { return; }
        if REQUESTED.swap(true, Ordering::Relaxed) { return; }
        if !crate::worker::spawn_detached("chroma-broker-setup", || {
            // A broker started by Task Scheduler may still be creating its sections.
            std::thread::sleep(Duration::from_secs(2));
            if neuron_host::adapters::chroma_shm::server::seeded_sections_present() { return; }
            if let Err(err) = request_elevation() {
                eprintln!("neuron-host: native Chroma broker setup unavailable: {err}");
                // A declined UAC prompt can be retried by toggling Connections later.
                REQUESTED.store(false, Ordering::Relaxed);
            }
        }) {
            REQUESTED.store(false, Ordering::Relaxed);
        }
    }

    fn request_elevation() -> Result<(), String> {
        let embedded_hash = format!("{:x}", Sha256::digest(INSTALLER));
        let expected_installer = option_env!("NEURON_BROKER_INSTALLER_SHA256")
            .ok_or("this app build has no pinned Chroma installer hash")?;
        if !embedded_hash.eq_ignore_ascii_case(expected_installer) {
            return Err("embedded Chroma installer differs from the app build pin".into());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = exe.parent().ok_or("no executable directory")?;
        let broker = dir.join("neuron-chroma-broker.exe");
        if !broker.is_file() {
            return Err("packaged Chroma broker files are missing".into());
        }

        let broker_bytes = std::fs::read(&broker).map_err(|e| e.to_string())?;
        let hash = format!("{:x}", Sha256::digest(&broker_bytes));
        let expected = option_env!("NEURON_BROKER_SHA256")
            .ok_or("this app build has no pinned Chroma broker hash")?;
        if !hash.eq_ignore_ascii_case(expected) {
            return Err("packaged broker differs from the app build".into());
        }
        let owner_sid = crate::autostart::current_user_sid()
            .ok_or("cannot resolve the current Windows user SID")?;

        // The elevated process reads the temporary file once, verifies those exact bytes, and
        // executes the in-memory copy. This keeps the command well below Windows' length limit
        // without reopening a user-writable script after it has been verified.
        let temp_path = std::env::temp_dir().join(format!(
            "neuron-chroma-{}-{}.ps1",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        ));
        let mut temp_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| e.to_string())?;
        temp_file.write_all(INSTALLER).map_err(|e| e.to_string())?;
        temp_file.flush().map_err(|e| e.to_string())?;
        drop(temp_file);
        let _temp_script = TempScript(temp_path.clone());
        let script_utf16: Vec<u8> = temp_path.as_os_str().encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        let script_b64 = base64::engine::general_purpose::STANDARD.encode(script_utf16);
        let broker_utf16: Vec<u8> = broker.as_os_str().encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        let broker_b64 = base64::engine::general_purpose::STANDARD.encode(broker_utf16);
        let command = format!(
            "$p=[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{script_b64}'));\
             $x=[IO.File]::ReadAllBytes($p);\
             $h=[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($x)).Replace('-','');\
             if($h -ine '{expected_installer}'){{exit 86}};\
             $s=[Text.Encoding]::UTF8.GetString($x);\
             $b=[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{broker_b64}'));\
             &([ScriptBlock]::Create($s)) -BinaryPath $b -ExpectedSha256 {hash} -OwnerSid '{owner_sid}'"
        );
        let command_utf16: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(command_utf16);

        let mut system_dir = vec![0u16; 512];
        // SAFETY: system_dir is a writable UTF-16 buffer and its length matches the capacity passed.
        let n = unsafe { GetSystemDirectoryW(system_dir.as_mut_ptr(), system_dir.len() as u32) } as usize;
        if n == 0 || n >= system_dir.len() { return Err("cannot resolve the Windows system directory".into()); }
        let powershell = PathBuf::from(OsString::from_wide(&system_dir[..n]))
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let params = format!(
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded}"
        );
        let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let verb = wide(std::ffi::OsStr::new("runas"));
        let ps = wide(powershell.as_os_str());
        let args = wide(std::ffi::OsStr::new(&params));
        // SAFETY: COM is initialized only for this short-lived worker thread.
        let com = unsafe {
            CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32)
        };
        if com < 0 { return Err(format!("cannot initialize the Windows shell ({com:#x})")); }
        // SAFETY: zero is the documented initial state; all pointers remain live through the call.
        let mut launch: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        launch.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        launch.fMask = SEE_MASK_NOCLOSEPROCESS;
        launch.lpVerb = verb.as_ptr();
        launch.lpFile = ps.as_ptr();
        launch.lpParameters = args.as_ptr();
        launch.nShow = SW_HIDE;
        // SAFETY: launch is fully initialized and points to live NUL-terminated UTF-16 buffers.
        let launched = unsafe { ShellExecuteExW(&raw mut launch) };
        let outcome = if launched == 0 || launch.hProcess.is_null() {
            Err(format!("Windows elevation failed: {}", std::io::Error::last_os_error()))
        } else {
            // SAFETY: hProcess is owned by this worker because SEE_MASK_NOCLOSEPROCESS succeeded.
            let waited = unsafe { WaitForSingleObject(launch.hProcess, INFINITE) };
            let mut code = 1u32;
            // SAFETY: code is writable and hProcess remains open until the query completes.
            let queried = if waited == WAIT_FAILED {
                0
            } else {
                unsafe { GetExitCodeProcess(launch.hProcess, &raw mut code) }
            };
            let query_error = (queried == 0).then(std::io::Error::last_os_error);
            // SAFETY: balances the process handle returned by ShellExecuteExW.
            unsafe { CloseHandle(launch.hProcess) };
            if let Some(error) = query_error {
                Err(format!("cannot read broker setup result: {error}"))
            } else if code != 0 {
                Err(format!("broker setup failed with exit code {code}"))
            } else {
                Ok(())
            }
        };
        // SAFETY: this balances the successful CoInitializeEx on this worker thread.
        unsafe { CoUninitialize() };
        outcome
    }
}

#[cfg(not(test))]
pub use live::request;

// The suite must never launch a privileged helper, even if a host test hits the OS error path.
#[cfg(test)]
pub fn request() {}

#[cfg(test)]
mod tests {
    const INSTALLER: &str = include_str!("../../../packaging/windows/install-chroma-broker.ps1");
    const UNINSTALLER: &str = include_str!("../../../packaging/windows/uninstall-chroma-broker.ps1");
    const SETUP: &str = include_str!("../../../packaging/windows/neuron.iss");

    #[test]
    fn broker_first_install_has_a_recoverable_ownership_transaction() {
        let pending = INSTALLER
            .find("Write-ProtectedRecord $stagePending")
            .expect("installer writes a protected pending owner record");
        let binary = INSTALLER
            .find("Copy-Item -LiteralPath $source -Destination $stageExe")
            .expect("installer stages the broker binary");
        let publish = INSTALLER
            .find("[IO.Directory]::Move($stage, $dir)")
            .expect("installer publishes with a no-replace directory rename");
        let receipt = INSTALLER
            .rfind("Write-ProtectedRecord $receipt")
            .expect("installer commits the final ownership receipt");
        let finish = INSTALLER
            .rfind("Remove-Item -LiteralPath $pending")
            .expect("installer removes pending state only after commit");
        assert!(pending < binary, "ownership must be recoverable before the first fallible mutation");
        assert!(
            binary < publish && publish < receipt && receipt < finish,
            "publication must carry pending ownership through final receipt commit"
        );
        assert!(UNINSTALLER.contains("$pending = Join-Path $dir 'broker-install.pending'"));
        let unpublish = UNINSTALLER
            .find("[IO.Directory]::Move($dir, $tombstone)")
            .expect("uninstall atomically removes the authenticated final path");
        let delete_receipt = UNINSTALLER
            .rfind("Remove-Item -LiteralPath $path")
            .expect("uninstall removes records only inside the tombstone");
        assert!(unpublish < delete_receipt, "uninstall must unpublish before deleting ownership state");
        assert!(SETUP.contains("broker-install.pending"), "setup preflight must admit a repair retry");
    }

    #[test]
    fn broker_helpers_serialize_recovery_and_publication() {
        const LOCK: &str = "Global\\NeuronChromaBroker.Transaction.v1";
        for (name, script) in [("install", INSTALLER), ("uninstall", UNINSTALLER)] {
            assert!(script.contains(LOCK), "{name} must use the shared machine transaction lock");
            assert!(script.contains("MutexSecurity"), "{name} lock needs an explicit protected ACL");
            let acquire = script
                .find("$transactionMutex.WaitOne")
                .expect("helper acquires the transaction lock");
            let recovery = script
                .find("foreach ($stale")
                .expect("helper performs interrupted-transaction recovery");
            assert!(acquire < recovery, "{name} must lock before inspecting stale transactions");
        }
        assert!(
            INSTALLER.contains("[IO.Directory]::Move($stage, $dir)"),
            "install publication must fail when the destination already exists"
        );
    }

    #[test]
    fn setup_resolves_privilege_before_mutating_the_app_directory() {
        let preflight = SETUP
            .find("Result := EnsureProtectedChromaBroker();")
            .expect("setup provisions the broker from PrepareToInstall");
        let post_install = SETUP
            .find("if CurStep <> ssPostInstall then Exit;")
            .expect("setup has a post-install phase");
        assert!(
            preflight < post_install,
            "broker approval must happen before app files are committed"
        );
        assert!(SETUP.contains("Flags: dontcopy"), "broker payload must be extractable before install");
        assert!(SETUP.contains("procedure DeinitializeSetup();"));
        assert!(SETUP.contains("function GetCustomSetupExitCode(): Integer;"));
        assert!(
            !SETUP.contains("Native Chroma setup was skipped"),
            "an installer must never report success after dropping native Chroma"
        );
    }
}
