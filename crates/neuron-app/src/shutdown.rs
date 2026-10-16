// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Graceful local shutdown for installers and updaters.

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, ERROR_NO_MORE_FILES,
    };
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenEventW, OpenProcess, QueryFullProcessImageNameW, SetEvent,
        WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const EVENT_PREFIX: &str = r"Local\WofloLabs.Neuron.Shutdown";
    const PRESENCE_EVENT_NAME: &str = r"Local\WofloLabs.Neuron.Present";
    const APP_IMAGE: &str = "neuron-app.exe";

    fn wide_name(name: &str) -> Vec<u16> {
        OsStr::new(name).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn event_name(pid: u32) -> String {
        format!("{EVENT_PREFIX}.{pid}")
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns one live Win32 handle and closes it once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Listener {
        shutdown: OwnedHandle,
        _presence: OwnedHandle,
    }

    impl Listener {
        pub fn new() -> Result<Self, std::io::Error> {
            Self::with_names(&event_name(std::process::id()), PRESENCE_EVENT_NAME)
        }

        fn with_names(shutdown_name: &str, presence_name: &str) -> Result<Self, std::io::Error> {
            let shutdown = Self::create_event(shutdown_name)?;
            let presence = Self::create_event(presence_name)?;
            Ok(Self { shutdown, _presence: presence })
        }

        fn create_event(name: &str) -> Result<OwnedHandle, std::io::Error> {
            let name = wide_name(name);
            // SAFETY: null security attributes request the caller's default DACL; name is live,
            // NUL-terminated UTF-16. Shutdown events are per-process; the separate presence
            // event is session-wide and remains present while at least one listener holds it.
            let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
            if handle.is_null() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(OwnedHandle(handle))
            }
        }

        pub fn requested(&self) -> bool {
            // SAFETY: self owns a live per-process shutdown event for the process lifetime.
            unsafe { WaitForSingleObject(self.shutdown.0, 0) == WAIT_OBJECT_0 }
        }
    }

    struct ProcessImage {
        pid: u32,
        session_id: Option<u32>,
        is_app_image: bool,
        path: Option<std::ffi::OsString>,
    }

    fn ascii_prefix_matches(path: &[u16], prefix: &str) -> bool {
        let prefix = prefix.encode_utf16().collect::<Vec<_>>();
        path.len() >= prefix.len()
            && path[..prefix.len()].iter().zip(prefix).all(|(left, right)| {
                let fold_ascii = |unit: u16| if (b'A' as u16..=b'Z' as u16).contains(&unit) { unit + 32 } else { unit };
                fold_ascii(*left) == fold_ascii(right)
            })
    }

    fn normalize_path(path: &OsStr) -> Vec<u16> {
        let mut path = path.encode_wide().map(|unit| if unit == b'/' as u16 { b'\\' as u16 } else { unit }).collect::<Vec<_>>();
        if ascii_prefix_matches(&path, r"\\?\UNC\") {
            let suffix = path.split_off(8);
            path.clear();
            path.extend([b'\\' as u16, b'\\' as u16]);
            path.extend(suffix);
        } else if ascii_prefix_matches(&path, r"\\?\") {
            path.drain(..4);
        }
        while path.len() > 1 && path.last() == Some(&(b'\\' as u16)) {
            path.pop();
        }
        path
    }

    fn equal_utf16_ignore_case(left: &[u16], right: &[u16]) -> bool {
        if left.is_empty() || right.is_empty() || left.len() > i32::MAX as usize || right.len() > i32::MAX as usize {
            return false;
        }
        // SAFETY: both buffers contain live UTF-16 code units and the supplied lengths stay within them.
        unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            ) == CSTR_EQUAL
        }
    }

    fn is_app_image(name: &[u16]) -> bool {
        let app = APP_IMAGE.encode_utf16().collect::<Vec<_>>();
        equal_utf16_ignore_case(name, &app)
    }

    fn same_image_path(left: &OsStr, right: &OsStr) -> bool {
        let left = normalize_path(left);
        let right = normalize_path(right);
        equal_utf16_ignore_case(&left, &right)
    }

    fn select_target_pids(
        processes: &[ProcessImage],
        own_pid: u32,
        current_session: u32,
        current_exe: &OsStr,
    ) -> Option<Vec<u32>> {
        let mut targets = Vec::new();
        for process in processes {
            if process.pid == own_pid || !process.is_app_image {
                continue;
            }
            let session_id = process.session_id?;
            if session_id != current_session {
                continue;
            }
            // An unreadable candidate could be another copy. Do not signal any process until all
            // same-name candidates have been identified by their full executable path.
            let path = process.path.as_deref()?;
            if same_image_path(path, current_exe) {
                targets.push(process.pid);
            }
        }
        Some(targets)
    }

    fn process_image_path(pid: u32) -> Option<std::ffi::OsString> {
        // SAFETY: OpenProcess only queries the requested process; no device or process state is changed.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let _handle = OwnedHandle(handle);
        let mut buffer = [0u16; 32768];
        let mut length = buffer.len() as u32;
        // SAFETY: handle is live, buffer is writable and length is its capacity in UTF-16 code units.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length) };
        if ok == 0 || length == 0 || length as usize > buffer.len() {
            None
        } else {
            Some(std::ffi::OsString::from_wide(&buffer[..length as usize]))
        }
    }

    fn process_session_id(pid: u32) -> Option<u32> {
        let mut session_id = 0u32;
        // SAFETY: the output pointer addresses a live writable u32; this only queries process metadata.
        if unsafe { ProcessIdToSessionId(pid, &raw mut session_id) } == 0 {
            None
        } else {
            Some(session_id)
        }
    }

    fn process_images(current_session: u32) -> Option<Vec<ProcessImage>> {
        // SAFETY: this takes a read-only snapshot of process metadata.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let _snapshot = OwnedHandle(snapshot);
        // SAFETY: PROCESSENTRY32W is a plain C output structure; Process32FirstW fills it before use.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: snapshot is live and entry points to writable storage with its size initialized.
        if unsafe { Process32FirstW(snapshot, &raw mut entry) } == 0 {
            return None;
        }

        let own_pid = std::process::id();
        let mut processes = Vec::new();
        loop {
            let end = entry.szExeFile.iter().position(|&unit| unit == 0).unwrap_or(entry.szExeFile.len());
            let app_image = is_app_image(&entry.szExeFile[..end]);
            let pid = entry.th32ProcessID;
            if pid != own_pid && app_image {
                let session_id = process_session_id(pid);
                match session_id {
                    Some(session_id) if session_id != current_session => continue,
                    None => return None,
                    _ => {}
                }
                processes.push(ProcessImage {
                    pid,
                    session_id,
                    is_app_image: app_image,
                    path: process_image_path(pid),
                });
            }

            // SAFETY: snapshot is live and entry remains writable for the next process record.
            if unsafe { Process32NextW(snapshot, &raw mut entry) } == 0 {
                // SAFETY: GetLastError reads the error from the immediately preceding Toolhelp call.
                if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
                    return None;
                }
                break;
            }
        }
        Some(processes)
    }

    fn signal_named(name: &str) -> bool {
        let name = wide_name(name);
        // SAFETY: name is live and NUL-terminated; access is limited to setting the event.
        let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle was opened with EVENT_MODIFY_STATE and remains live for both calls.
        let signaled = unsafe { SetEvent(handle) != 0 };
        // SAFETY: this closes the event handle opened above exactly once.
        unsafe { CloseHandle(handle) };
        signaled
    }

    pub fn signal_existing() -> bool {
        let Ok(current_exe) = std::env::current_exe() else { return false; };
        let Some(current_session) = process_session_id(std::process::id()) else { return false; };
        let Some(processes) = process_images(current_session) else { return false; };
        let Some(targets) = select_target_pids(
            &processes,
            std::process::id(),
            current_session,
            current_exe.as_os_str(),
        ) else {
            return false;
        };
        if targets.is_empty() {
            return false;
        }
        let mut all_signaled = true;
        for pid in targets {
            all_signaled &= signal_named(&event_name(pid));
        }
        all_signaled
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[test]
        fn selection_requires_the_same_image_and_excludes_self() {
            let current = OsStr::new(r"C:\Program Files\Neuron\neuron-app.exe");
            let processes = vec![
                ProcessImage { pid: 11, session_id: Some(1), is_app_image: true, path: Some(current.to_os_string()) },
                ProcessImage { pid: 12, session_id: Some(1), is_app_image: true, path: Some(OsStr::new(r"C:\repo\target\release\neuron-app.exe").to_os_string()) },
                ProcessImage { pid: 13, session_id: Some(1), is_app_image: false, path: None },
                ProcessImage { pid: 14, session_id: Some(1), is_app_image: true, path: Some(OsStr::new(r"c:/PROGRAM FILES/NEURON/neuron-app.exe").to_os_string()) },
            ];
            assert_eq!(select_target_pids(&processes, 14, 1, current), Some(vec![11]));
            assert_eq!(event_name(11), format!("{EVENT_PREFIX}.11"));
            assert_ne!(event_name(11), event_name(12));
        }

        #[test]
        fn remote_unreadable_copy_does_not_veto_a_local_target() {
            let processes = vec![
                ProcessImage {
                    pid: 11,
                    session_id: Some(1),
                    is_app_image: true,
                    path: Some(OsStr::new(r"C:\app\neuron-app.exe").to_os_string()),
                },
                ProcessImage { pid: 12, session_id: Some(2), is_app_image: true, path: None },
            ];
            assert_eq!(select_target_pids(&processes, 99, 1, OsStr::new(r"C:\app\neuron-app.exe")), Some(vec![11]));
        }

        #[test]
        fn current_session_unknown_or_unreadable_candidate_fails_closed() {
            let current = OsStr::new(r"C:\app\neuron-app.exe");
            let unknown_session = vec![ProcessImage { pid: 11, session_id: None, is_app_image: true, path: None }];
            assert_eq!(select_target_pids(&unknown_session, 99, 1, current), None);
            let unreadable_path = vec![ProcessImage { pid: 12, session_id: Some(1), is_app_image: true, path: None }];
            assert_eq!(select_target_pids(&unreadable_path, 99, 1, current), None);
        }

        #[test]
        fn signaling_one_owned_event_does_not_request_another_copy() {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            let first_name = format!(r"{EVENT_PREFIX}.Test.{}.{}.first", std::process::id(), nonce);
            let second_name = format!(r"{EVENT_PREFIX}.Test.{}.{}.second", std::process::id(), nonce);
            let first_presence = format!(r"{EVENT_PREFIX}.Test.{}.{}.first.present", std::process::id(), nonce);
            let second_presence = format!(r"{EVENT_PREFIX}.Test.{}.{}.second.present", std::process::id(), nonce);
            let first = Listener::with_names(&first_name, &first_presence).expect("create isolated first test events");
            let second = Listener::with_names(&second_name, &second_presence).expect("create isolated second test events");

            assert!(event_exists(&first_presence));
            assert!(event_exists(&second_presence));
            assert!(!first.requested());
            assert!(!second.requested());
            assert!(signal_named(&first_name));
            assert!(first.requested());
            assert!(!first.requested(), "auto-reset test event is consumed once");
            assert!(!second.requested(), "another copy's event remains untouched");
            drop(first);
            assert!(!event_exists(&first_presence), "presence ends when its copy releases the event");
            assert!(event_exists(&second_presence), "the other copy keeps its own presence");
        }

        fn event_exists(name: &str) -> bool {
            let name = wide_name(name);
            // SAFETY: name is live and NUL-terminated; this only opens an isolated test event.
            let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
            if handle.is_null() {
                return false;
            }
            // SAFETY: this closes the event handle opened above exactly once.
            unsafe { CloseHandle(handle) };
            true
        }

        #[test]
        fn path_comparison_preserves_utf16_and_handles_unicode_case() {
            let upper = OsStr::new(r"C:\NÖ\Neuron\neuron-app.exe");
            let lower = OsStr::new(r"c:\nö\NEURON\NEURON-APP.EXE");
            assert!(same_image_path(upper, lower));

            let prefix = r"C:\app\".encode_utf16().collect::<Vec<_>>();
            let suffix = r"\neuron-app.exe".encode_utf16().collect::<Vec<_>>();
            let mut unpaired = prefix.clone();
            unpaired.push(0xD800);
            unpaired.extend_from_slice(&suffix);
            let mut replacement = prefix;
            replacement.push(0xFFFD);
            replacement.extend_from_slice(&suffix);
            let unpaired = OsString::from_wide(&unpaired);
            let replacement = OsString::from_wide(&replacement);
            assert_ne!(normalize_path(&unpaired), normalize_path(&replacement));
            assert!(!same_image_path(&unpaired, &replacement));
        }
    }
}

#[cfg(windows)]
pub use platform::{signal_existing, Listener};

#[cfg(not(windows))]
pub struct Listener;

#[cfg(not(windows))]
impl Listener {
    pub fn new() -> Self {
        Self
    }
    pub fn requested(&self) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub fn signal_existing() -> bool {
    false
}
