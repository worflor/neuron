// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only: poll XInput user slots for N seconds and print every state change (buttons mask,
//! sticks, triggers) — does a background process see an Xbox-compatible pad? Run: xinput_probe [secs]
#[cfg(windows)]
fn main() {
    use windows_sys::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last: [Option<u32>; 4] = [None; 4];
    println!("polling XInput slots 0..3 for {secs}s");
    while std::time::Instant::now() < until {
        for slot in 0..4u32 {
            // SAFETY: a plain out-parameter read; the struct is zero-initialized POD.
            let mut st: XINPUT_STATE = unsafe { std::mem::zeroed() };
            let rc = unsafe { XInputGetState(slot, &mut st) };
            if rc != 0 {
                continue; // not connected
            }
            if last[slot as usize] != Some(st.dwPacketNumber) {
                last[slot as usize] = Some(st.dwPacketNumber);
                let g = st.Gamepad;
                println!(
                    "slot {slot} pkt {:>6} buttons {:04x} LT {:>3} RT {:>3} L ({:>6},{:>6}) R ({:>6},{:>6})",
                    st.dwPacketNumber, g.wButtons, g.bLeftTrigger, g.bRightTrigger, g.sThumbLX, g.sThumbLY, g.sThumbRX, g.sThumbRY
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

#[cfg(not(windows))]
fn main() {}
