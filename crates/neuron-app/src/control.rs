//! THE CONTROL CENTER — a radial PRIME that GLANCES system state and offers a quick toggle.
//!
//! Primed by [`Action::Control`](neuron::action::Action::Control), the next hold of the cast
//! trigger opens a small glance: "wtf is my internet rn? am i on ethernet? what's my output?" —
//! the three things you actually crane your neck for, read at one look. A flick commits a quick
//! action; a peek (sub-deadzone release) just closes it.
//!
//! Honest about reach: NETWORK (ethernet vs wifi + SSID + whether you're actually online) and
//! BLUETOOTH presence are GLANCED via plain Win32 (IP Helper / WLAN / the Bluetooth radio find) —
//! no elevation, instant. The OUTPUT side reuses the audio layer's flip. The Bluetooth RADIO
//! on/off is the one thing Win32 can't flip without WinRT (`Windows.Devices.Radios`); rather than
//! pretend, the bluetooth flick opens the Settings page that does — an honest one-tap seam.

#![cfg(windows)]

use windows_sys::Win32::Foundation::HANDLE;

/// What the glance found about the live system — pre-resolved to the few facts worth a look, so
/// the overlay render (and the weave thread) never touch a blocking API.
#[derive(Clone, Default)]
pub struct Glance {
    /// the active route's medium: `Ethernet`, `WiFi`, or `None` (no default route = offline-ish).
    pub link: Link,
    /// the wifi SSID when on wifi (else empty) — "am i on the right network?".
    pub ssid: String,
    /// the connected interface's friendly name ("Ethernet", "Wi-Fi") — the adapter you're using.
    pub iface: String,
    /// is there a real default gateway? (a route to the world — "am i actually online?").
    pub online: bool,
    /// a bluetooth radio exists on this machine (so the toggle seam is worth offering).
    pub bt_present: bool,
}

/// The medium of the live default route.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    Ethernet,
    WiFi,
    #[default]
    None,
}

impl Link {
    /// One short word for the wedge value.
    pub fn label(self) -> &'static str {
        match self {
            Link::Ethernet => "ethernet",
            Link::WiFi => "wifi",
            Link::None => "offline",
        }
    }
}

// IfType codes (IP Helper) — declared locally; the windows-sys consts live behind a heavier
// feature and these two never change.
const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_WIFI: u32 = 71; // IF_TYPE_IEEE80211
const IF_TYPE_LOOPBACK: u32 = 24;

/// Take a fresh reading of the system: which adapter carries the default route, its medium + name,
/// whether we're online, the wifi SSID, and whether a bluetooth radio is present. All instant,
/// non-blocking Win32 — safe to call on the weave thread at instrument-open.
pub fn glance() -> Glance {
    let mut g = net_glance();
    if g.link == Link::WiFi {
        g.ssid = wifi_ssid();
    }
    g.bt_present = bluetooth_present();
    g
}

/// Walk the adapters and pick the one carrying the default route (a non-loopback adapter that is
/// UP and has a gateway). Reads its IfType (ethernet vs wifi) + friendly name. The presence of a
/// gateway on an up adapter is our "online" signal — honest without an active probe.
fn net_glance() -> Glance {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    const AF_UNSPEC: u32 = 0;
    // GAA_FLAG_INCLUDE_GATEWAYS (0x0080) so FirstGatewayAddress is populated; skip the costly bits.
    const GAA_FLAG_INCLUDE_GATEWAYS: u32 = 0x0080;
    const GAA_FLAG_SKIP_ANYCAST: u32 = 0x0002;
    const GAA_FLAG_SKIP_MULTICAST: u32 = 0x0004;
    const GAA_FLAG_SKIP_DNS_SERVER: u32 = 0x0008;
    const IF_OPER_STATUS_UP: i32 = 1; // IfOperStatusUp
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut out = Glance::default();
    unsafe {
        // size the buffer (it asks for the length, then we fill); 15 KB is the usual first guess.
        let mut size: u32 = 15 * 1024;
        let mut buf = vec![0u8; size as usize];
        let mut rc = GetAdaptersAddresses(
            AF_UNSPEC,
            flags,
            std::ptr::null(),
            buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        );
        if rc == 111 {
            // ERROR_BUFFER_OVERFLOW — grow to the size it told us and retry once.
            buf = vec![0u8; size as usize];
            rc = GetAdaptersAddresses(
                AF_UNSPEC,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            );
        }
        if rc != 0 {
            return out; // no adapters readable — report offline honestly
        }
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        // prefer an adapter that is UP AND has a gateway (the real online route); remember the
        // first up-with-gateway we see — adapters enumerate in the OS's binding order (best first).
        while !p.is_null() {
            let a = &*p;
            let up = a.OperStatus == IF_OPER_STATUS_UP;
            let has_gw = !a.FirstGatewayAddress.is_null();
            if up && has_gw && a.IfType != IF_TYPE_LOOPBACK {
                out.link = match a.IfType {
                    IF_TYPE_WIFI => Link::WiFi,
                    IF_TYPE_ETHERNET => Link::Ethernet,
                    // a VPN/cellular/other still counts as a live route — name it, call it online.
                    _ => Link::Ethernet,
                };
                out.iface = pwstr(a.FriendlyName);
                out.online = true;
                break;
            }
            p = a.Next;
        }
    }
    out
}

/// The SSID of the connected wifi interface (empty if not on wifi / unreadable). Opens a WLAN
/// client handle, finds a connected interface, queries its current-connection attributes.
fn wifi_ssid() -> String {
    use windows_sys::Win32::NetworkManagement::WiFi::{
        wlan_interface_state_connected, wlan_intf_opcode_current_connection, WlanCloseHandle,
        WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface,
        WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST,
    };
    unsafe {
        let mut neg: u32 = 0;
        let mut h: HANDLE = std::ptr::null_mut();
        // client version 2 — the modern WLAN API. A failure here = no WLAN service (no wifi).
        if WlanOpenHandle(2, std::ptr::null(), &mut neg, &mut h) != 0 || h.is_null() {
            return String::new();
        }
        let mut ssid = String::new();
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(h, std::ptr::null(), &mut list) == 0 && !list.is_null() {
            let n = (*list).dwNumberOfItems as usize;
            let items = (*list).InterfaceInfo.as_ptr();
            for i in 0..n {
                let info = &*items.add(i);
                if info.isState != wlan_interface_state_connected {
                    continue;
                }
                let mut data_size: u32 = 0;
                let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
                if WlanQueryInterface(
                    h,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    std::ptr::null(),
                    &mut data_size,
                    &mut data,
                    std::ptr::null_mut(),
                ) == 0
                    && !data.is_null()
                {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    let dot11 = &attrs.wlanAssociationAttributes.dot11Ssid;
                    let len = (dot11.uSSIDLength as usize).min(dot11.ucSSID.len());
                    ssid = String::from_utf8_lossy(&dot11.ucSSID[..len]).into_owned();
                    WlanFreeMemory(data);
                }
                if !ssid.is_empty() {
                    break;
                }
            }
            WlanFreeMemory(list as *const _);
        }
        WlanCloseHandle(h, std::ptr::null());
        ssid
    }
}

/// Is a Bluetooth radio installed? (We never toggle it from here — Win32 can't without WinRT — but
/// a radio's presence decides whether the toggle SEAM is worth offering.)
fn bluetooth_present() -> bool {
    use windows_sys::Win32::Devices::Bluetooth::{
        BluetoothFindFirstRadio, BluetoothFindRadioClose, BLUETOOTH_FIND_RADIO_PARAMS,
    };
    use windows_sys::Win32::Foundation::CloseHandle;
    unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
        };
        let mut radio: HANDLE = std::ptr::null_mut();
        let find = BluetoothFindFirstRadio(&params, &mut radio);
        if find.is_null() {
            return false;
        }
        if !radio.is_null() {
            CloseHandle(radio);
        }
        BluetoothFindRadioClose(find);
        true
    }
}

/// THE BLUETOOTH SEAM — open the Settings page that toggles the radio. The actual on/off lives in
/// WinRT (`Windows.Devices.Radios`, not in windows-sys); rather than fake a toggle we can't do,
/// land the user exactly one tap from it. Honest, no elevation. Returns the status line.
pub fn open_bluetooth() -> String {
    if open_settings("ms-settings:bluetooth") {
        "bluetooth settings \u{2014} flip the radio there".into()
    } else {
        "couldn't open bluetooth settings".into()
    }
}

/// Toggle the Bluetooth RADIO in place (item 18 — "lemme toggle Bluetooth real quick", not a seam).
/// The radio power lives in WinRT (`Windows.Devices.Radios`), which windows-sys can't reach without
/// pulling in the heavy `windows` crate, so we drive it through a hidden PowerShell that flips the
/// first Bluetooth radio's state. If WinRT access is denied (or there's no radio), the script lands
/// on the Settings page instead — so a press is never a no-op. Arm-gated like every process spawn;
/// disarmed → just the (harmless) seam.
pub fn bluetooth_toggle() -> String {
    use std::os::windows::process::CommandExt;
    if !neuron::action::process_spawn_armed() {
        return open_bluetooth();
    }
    // the canonical PS 5.1 WinRT-await pattern (AsTask reflection), with the settings seam as the
    // in-script fallback. Single-quoted PS strings keep the literal backtick in `IAsyncOperation`1`.
    const SCRIPT: &str = r#"
try {
  $g = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
  function Await($op,$t){ $n = $g.MakeGenericMethod($t).Invoke($null,@($op)); $n.Wait(-1) | Out-Null; $n.Result }
  [void][Windows.Devices.Radios.Radio,Windows.System.Devices,ContentType=WindowsRuntime]
  [void][Windows.Devices.Radios.RadioAccessStatus,Windows.System.Devices,ContentType=WindowsRuntime]
  [void](Await ([Windows.Devices.Radios.Radio]::RequestAccessAsync()) ([Windows.Devices.Radios.RadioAccessStatus]))
  $rs = Await ([Windows.Devices.Radios.Radio]::GetRadiosAsync()) ([System.Collections.Generic.IReadOnlyList[Windows.Devices.Radios.Radio]])
  $bt = $rs | Where-Object { $_.Kind -eq 'Bluetooth' } | Select-Object -First 1
  if ($bt) { $s = if ($bt.State -eq 'On') { 'Off' } else { 'On' }; [void](Await ($bt.SetStateAsync($s)) ([Windows.Devices.Radios.RadioAccessStatus])) } else { throw 'no radio' }
} catch { Start-Process 'ms-settings:bluetooth' }
"#;
    match std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            SCRIPT,
        ])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW — no console flash
        .spawn()
    {
        Ok(_) => "\u{1f4f6} bluetooth \u{2014} toggling".into(),
        Err(_) => open_bluetooth(),
    }
}

/// Open the network status Settings page (the "am i on the right thing / connect elsewhere" seam).
pub fn open_network() -> String {
    if open_settings("ms-settings:network-status") {
        "network settings".into()
    } else {
        "couldn't open network settings".into()
    }
}

/// Launch an `ms-settings:` URI via the shell (the OS resolves it to the Settings app). No
/// arm-gate: opening a read-only Settings page is not a synthesized input or a device write.
fn open_settings(uri: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = to_wide("open");
    let file = to_wide(uri);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32 // ShellExecute's success sentinel
}

/// Read a NUL-terminated wide (PWSTR) string into a Rust String. Empty on null.
unsafe fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
