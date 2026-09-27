// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Graceful local shutdown for installers and updaters.

#[cfg(windows)]
mod platform {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    };

    const NAME: &str = r"Local\WofloLabs.Neuron.Shutdown";

    fn wide_name() -> Vec<u16> {
        std::ffi::OsStr::new(NAME)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub struct Listener(HANDLE);

    impl Listener {
        pub fn new() -> Result<Self, std::io::Error> {
            let name = wide_name();
            // SAFETY: null security attributes request the caller's default DACL; name is live,
            // NUL-terminated UTF-16. An auto-reset event wakes exactly one resident instance.
            let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
            if handle.is_null() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        pub fn requested(&self) -> bool {
            // SAFETY: self owns a live event handle for the process lifetime.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: this closes the handle created by Listener::new exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn signal_existing() -> bool {
        let name = wide_name();
        // SAFETY: name is live, NUL-terminated UTF-16. The handle is closed below.
        let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle was opened with EVENT_MODIFY_STATE and remains live for both calls.
        let signaled = unsafe { SetEvent(handle) != 0 };
        unsafe { CloseHandle(handle) };
        signaled
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
