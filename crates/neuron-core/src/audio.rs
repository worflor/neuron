//! Core-Audio control for capture/render endpoints — the cross-device half of remapping.
//!
//! Synapse's own mic gain/mute path is `RSy3_WinAudio.dll`, a thin native wrapper over the
//! Windows **Core Audio** `IAudioEndpointVolume` (proven by its exported
//! `WinAudio_MicVol/MicMute Get/Set` + the `IAudioEndpointVolumeCallback` vtable). So the
//! genuinely-useful remaps — headset knob -> the *real* mic's gain, the never-used headset
//! mute toggle -> the *real* mic's mute — need **zero vendor-HID reverse engineering**: we
//! drive the exact same OS API Synapse does. This module is that driver.
//!
//! Hand-rolled COM (no `windows` crate, no extra deps): `windows-sys` ships the plain COM
//! *functions* (`CoCreateInstance`, ...) but no interface vtables, so we declare the five we
//! need ourselves. First-principles, and the binary stays tiny.
//!
//! ## Shape — the whole OS-audio-control surface behind a per-OS SEAM
//! The OS-audio-control surface — the stateful handles ([`VolumeCtl`], [`MeterCtl`]) AND the
//! free verbs ([`endpoints`], the resolvers, [`flip_output`], …) — is split the way `surface.rs`
//! / `overlay.rs` split theirs: the Win32 COM body lives in `mod imp` and an inert `mod stub`
//! stands in off-Windows, with `pub use imp::*` / `pub use stub::*` selecting one. The shared,
//! platform-neutral DATA ([`Endpoint`], [`Flow`]) stays at the top level so both backends speak
//! the same vocabulary. The Windows COM bodies are byte-for-byte the same — only relocated from
//! the old `mod win` into `mod imp`.
//!
//! Why this seam and not the `wm.rs` trait: `wm.rs` routes an OPAQUE handle (`isize`) through a
//! zero-sized backend, so a trait fits. Audio's handles are STATEFUL — `VolumeCtl`/`MeterCtl`
//! own a live COM pointer the caller holds across calls — so, exactly like `SpellOverlay` /
//! `LayeredSurface`, the cleanest seam is two modules each defining the handle TYPE, picked by
//! cfg. `mod stub` is ALWAYS compiled (not cfg-gated): its no-op bodies need no platform API, so
//! they're type-checked on every build (incl. Windows) — a stub body that fails to compile is
//! caught at once. CAVEAT vs the `wm.rs` trait: surface PARITY is by-convention here, not
//! compiler-enforced. A trait makes a backend implement an exact method set, so a missing verb
//! breaks the Windows build; these are two independent modules selected by `pub use`, so adding a
//! verb to `imp` WITHOUT a matching `stub` entry still compiles on Windows and surfaces only as a
//! missing symbol on a non-Windows build. MAINTENANCE: after changing this surface, run
//! `cargo check -p neuron --target x86_64-unknown-linux-gnu` to confirm `stub` still mirrors `imp`.
//! A real ALSA/Pulse (Linux) or CoreAudio (macOS) backend is then a single `mod` of the same names.

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
pub use stub::*;

/// One audio endpoint (a capture or render device) as the OS sees it.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub flow: Flow,
    pub volume: f32, // 0.0..=1.0 master scalar
    pub muted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Capture, // microphones / line-in
    Render,  // speakers / headphones
}

impl Flow {
    /// EDataFlow value expected by `IMMDeviceEnumerator::EnumAudioEndpoints`.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn edata(self) -> i32 {
        match self {
            Flow::Render => 0,
            Flow::Capture => 1,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Flow::Capture => "capture",
            Flow::Render => "render",
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{Endpoint, Flow};
    use std::ffi::c_void;
    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // --- GUIDs (textual order, via from_u128) -------------------------------------------
    const CLSID_MM_DEVICE_ENUMERATOR: GUID =
        GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
    const IID_IMM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xA95664D2_9614_4F35_A746_DE8DB63617E6);
    const IID_IAUDIO_ENDPOINT_VOLUME: GUID =
        GUID::from_u128(0x5CDF2C82_841E_4546_9722_0CF74078229A);
    const IID_IAUDIO_METER_INFORMATION: GUID =
        GUID::from_u128(0xC02216F6_8C67_4B5B_9D00_D008E73E0064);
    // PKEY_Device_FriendlyName = fmtid {a45c254e-df1c-4efd-8020-67d146a850e0}, pid 14.
    const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
        fmtid: GUID::from_u128(0xA45C254E_DF1C_4EFD_8020_67D146A850E0),
        pid: 14,
    };

    const DEVICE_STATE_ACTIVE: u32 = 0x1;
    const STGM_READ: u32 = 0x0;
    const VT_LPWSTR: u16 = 31;

    #[repr(C)]
    struct PropertyKey {
        fmtid: GUID,
        pid: u32,
    }

    // Minimal PROPVARIANT: 8-byte header + value union. On x64 sizeof is 24; we only ever
    // read VT_LPWSTR, whose pointer sits right after the header. The trailing pad keeps the
    // struct large enough that the OS never writes past our allocation.
    #[repr(C)]
    struct PropVariant {
        vt: u16,
        _r1: u16,
        _r2: u16,
        _r3: u16,
        val: *mut u16,
        _pad: usize,
    }
    impl PropVariant {
        fn zeroed() -> Self {
            PropVariant {
                vt: 0,
                _r1: 0,
                _r2: 0,
                _r3: 0,
                val: std::ptr::null_mut(),
                _pad: 0,
            }
        }
    }

    // --- Vtables. Unused slots are pointer-sized placeholders to preserve method offsets. --
    type Ph = *const c_void;

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface: Ph,
        add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct ImmDeviceEnumeratorVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        enum_audio_endpoints:
            unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> HRESULT,
        // GetDefaultAudioEndpoint(flow, role, ppDevice) — which device is "the default" now.
        get_default: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> HRESULT,
        get_device: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> HRESULT,
        _register: Ph,
        _unregister: Ph,
    }

    #[repr(C)]
    struct ImmDeviceCollectionVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct ImmDeviceVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            u32,
            *mut c_void,
            *mut *mut c_void,
        ) -> HRESULT,
        open_property_store:
            unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
        get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
        _get_state: Ph,
    }

    #[repr(C)]
    struct IPropertyStoreVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        _get_count: Ph,
        _get_at: Ph,
        get_value:
            unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT,
        _set_value: Ph,
        _commit: Ph,
    }

    #[repr(C)]
    struct IAudioEndpointVolumeVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        _register: Ph,
        _unregister: Ph,
        _get_channel_count: Ph,
        _set_master_db: Ph,
        set_master_scalar: unsafe extern "system" fn(*mut c_void, f32, *const GUID) -> HRESULT,
        _get_master_db: Ph,
        get_master_scalar: unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
        _set_chan_db: Ph,
        _set_chan_scalar: Ph,
        _get_chan_db: Ph,
        _get_chan_scalar: Ph,
        set_mute: unsafe extern "system" fn(*mut c_void, i32, *const GUID) -> HRESULT,
        get_mute: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
        _get_step_info: Ph,
        _step_up: Ph,
        _step_down: Ph,
        _query_hw: Ph,
        _get_range: Ph,
    }

    // IAudioMeterInformation — only GetPeakValue is called; it sits at the first vtable slot after
    // IUnknown (QI/AddRef/Release), so the others are pointer-sized placeholders for ABI offset.
    #[repr(C)]
    struct IAudioMeterInformationVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_peak_value: unsafe extern "system" fn(*mut c_void, *mut f32) -> HRESULT,
        _get_metering_channel_count: Ph,
        _get_channels_peak_values: Ph,
        _query_hardware_support: Ph,
    }

    unsafe fn vtbl<T>(obj: *mut c_void) -> *const T {
        *(obj as *const *const T)
    }
    unsafe fn release(obj: *mut c_void) {
        if !obj.is_null() {
            let vt = vtbl::<IUnknownVtbl>(obj);
            ((*vt).release)(obj);
        }
    }
    unsafe fn wide_to_string(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Initialise COM (MTA) for this thread. S_FALSE (already-init) is fine.
    fn com_init() {
        unsafe {
            let _ = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
        }
    }

    fn create_enumerator() -> *mut c_void {
        let mut p: *mut c_void = std::ptr::null_mut();
        unsafe {
            let hr = CoCreateInstance(
                &CLSID_MM_DEVICE_ENUMERATOR,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMM_DEVICE_ENUMERATOR,
                &mut p,
            );
            if hr < 0 {
                return std::ptr::null_mut();
            }
        }
        p
    }

    unsafe fn device_id(dev: *mut c_void) -> String {
        let vt = vtbl::<ImmDeviceVtbl>(dev);
        let mut idp: *mut u16 = std::ptr::null_mut();
        if ((*vt).get_id)(dev, &mut idp) < 0 {
            return String::new();
        }
        let s = wide_to_string(idp);
        CoTaskMemFree(idp as *const c_void);
        s
    }

    unsafe fn device_name(dev: *mut c_void) -> String {
        let vt = vtbl::<ImmDeviceVtbl>(dev);
        let mut store: *mut c_void = std::ptr::null_mut();
        if ((*vt).open_property_store)(dev, STGM_READ, &mut store) < 0 || store.is_null() {
            return String::new();
        }
        let svt = vtbl::<IPropertyStoreVtbl>(store);
        let mut pv = PropVariant::zeroed();
        let mut name = String::new();
        if ((*svt).get_value)(store, &PKEY_DEVICE_FRIENDLY_NAME, &mut pv) >= 0 && pv.vt == VT_LPWSTR
        {
            name = wide_to_string(pv.val);
            CoTaskMemFree(pv.val as *const c_void); // == PropVariantClear for VT_LPWSTR
        }
        release(store);
        name
    }

    /// Activate the endpoint-volume interface for a device, reading its current state.
    unsafe fn activate_volume(dev: *mut c_void) -> (*mut c_void, f32, bool) {
        let vt = vtbl::<ImmDeviceVtbl>(dev);
        let mut vol: *mut c_void = std::ptr::null_mut();
        let hr = ((*vt).activate)(
            dev,
            &IID_IAUDIO_ENDPOINT_VOLUME,
            CLSCTX_ALL,
            std::ptr::null_mut(),
            &mut vol,
        );
        if hr < 0 || vol.is_null() {
            return (std::ptr::null_mut(), 0.0, false);
        }
        let vvt = vtbl::<IAudioEndpointVolumeVtbl>(vol);
        let mut scalar = 0f32;
        let _ = ((*vvt).get_master_scalar)(vol, &mut scalar);
        let mut m = 0i32;
        let _ = ((*vvt).get_mute)(vol, &mut m);
        (vol, scalar, m != 0)
    }

    /// Enumerate active endpoints of one flow. `with_volume` activates each endpoint's volume
    /// interface to read gain/mute; skip it (name-resolution path) to avoid that per-endpoint
    /// COM cost on every control event.
    fn collect(flow: Flow, with_volume: bool) -> Vec<Endpoint> {
        com_init();
        let mut out = Vec::new();
        let en = create_enumerator();
        if en.is_null() {
            return out;
        }
        unsafe {
            let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
            let mut coll: *mut c_void = std::ptr::null_mut();
            if ((*evt).enum_audio_endpoints)(en, flow.edata(), DEVICE_STATE_ACTIVE, &mut coll) >= 0
                && !coll.is_null()
            {
                let cvt = vtbl::<ImmDeviceCollectionVtbl>(coll);
                let mut n = 0u32;
                let _ = ((*cvt).get_count)(coll, &mut n);
                for i in 0..n {
                    let mut dev: *mut c_void = std::ptr::null_mut();
                    if ((*cvt).item)(coll, i, &mut dev) < 0 || dev.is_null() {
                        continue;
                    }
                    let id = device_id(dev);
                    let name = device_name(dev);
                    let (volume, muted) = if with_volume {
                        let (vol, scalar, muted) = activate_volume(dev);
                        release(vol);
                        (scalar, muted)
                    } else {
                        (0.0, false)
                    };
                    out.push(Endpoint {
                        id,
                        name,
                        flow,
                        volume,
                        muted,
                    });
                    release(dev);
                }
                release(coll);
            }
            release(en);
        }
        out
    }

    /// Enumerate active endpoints of one flow with their current volume/mute.
    pub fn endpoints(flow: Flow) -> Vec<Endpoint> {
        collect(flow, true)
    }

    /// A live handle to one endpoint's volume control. Releases the COM object on drop.
    pub struct VolumeCtl {
        vol: *mut c_void,
    }

    impl VolumeCtl {
        /// Open by exact endpoint id (as returned in `Endpoint::id`).
        pub fn open(id: &str) -> Option<Self> {
            com_init();
            let en = create_enumerator();
            if en.is_null() {
                return None;
            }
            unsafe {
                let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
                let wid = to_wide(id);
                let mut dev: *mut c_void = std::ptr::null_mut();
                let hr = ((*evt).get_device)(en, wid.as_ptr(), &mut dev);
                release(en);
                if hr < 0 || dev.is_null() {
                    return None;
                }
                let (vol, _, _) = activate_volume(dev);
                release(dev);
                if vol.is_null() {
                    None
                } else {
                    Some(VolumeCtl { vol })
                }
            }
        }

        pub fn get_volume(&self) -> f32 {
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                let mut s = 0f32;
                let _ = ((*vt).get_master_scalar)(self.vol, &mut s);
                s
            }
        }

        /// Set master volume (0.0..=1.0), clamped.
        pub fn set_volume(&self, scalar: f32) -> bool {
            let s = scalar.clamp(0.0, 1.0);
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                ((*vt).set_master_scalar)(self.vol, s, std::ptr::null()) >= 0
            }
        }

        pub fn nudge(&self, delta: f32) -> f32 {
            let v = (self.get_volume() + delta).clamp(0.0, 1.0);
            self.set_volume(v);
            v
        }

        pub fn get_mute(&self) -> bool {
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                let mut m = 0i32;
                let _ = ((*vt).get_mute)(self.vol, &mut m);
                m != 0
            }
        }
        pub fn set_mute(&self, mute: bool) -> bool {
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                ((*vt).set_mute)(self.vol, mute as i32, std::ptr::null()) >= 0
            }
        }
        pub fn toggle_mute(&self) -> bool {
            let next = !self.get_mute();
            self.set_mute(next);
            next
        }
    }

    impl Drop for VolumeCtl {
        fn drop(&mut self) {
            unsafe { release(self.vol) }
        }
    }

    /// Activate the peak-meter interface on a device (mirror of [`activate_volume`]).
    unsafe fn activate_meter(dev: *mut c_void) -> *mut c_void {
        let vt = vtbl::<ImmDeviceVtbl>(dev);
        let mut meter: *mut c_void = std::ptr::null_mut();
        let hr = ((*vt).activate)(
            dev,
            &IID_IAUDIO_METER_INFORMATION,
            CLSCTX_ALL,
            std::ptr::null_mut(),
            &mut meter,
        );
        if hr < 0 {
            std::ptr::null_mut()
        } else {
            meter
        }
    }

    /// A live handle to one endpoint's signal-level meter. Reads the OS peak-sample value of
    /// whatever is currently playing — the real audio signal, the `audiometer` effect's input.
    /// Releases the COM object on drop.
    pub struct MeterCtl {
        meter: *mut c_void,
    }

    impl MeterCtl {
        /// Open the meter on the current DEFAULT render endpoint (the speakers/headset the OSD
        /// shows) — so the effect follows "the sound I actually hear". `None` if nothing resolves.
        pub fn open_default_render() -> Option<Self> {
            com_init();
            let en = create_enumerator();
            if en.is_null() {
                return None;
            }
            unsafe {
                let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
                let mut dev: *mut c_void = std::ptr::null_mut();
                // (Render = 0, eConsole = 0) — the current default output.
                let hr = ((*evt).get_default)(en, 0, 0, &mut dev);
                release(en);
                if hr < 0 || dev.is_null() {
                    return None;
                }
                let meter = activate_meter(dev);
                release(dev);
                if meter.is_null() {
                    None
                } else {
                    Some(MeterCtl { meter })
                }
            }
        }

        /// The current peak sample value (0.0..=1.0) since the last read — the live loudness.
        pub fn peak(&self) -> f32 {
            unsafe {
                let vt = vtbl::<IAudioMeterInformationVtbl>(self.meter);
                let mut p = 0f32;
                let _ = ((*vt).get_peak_value)(self.meter, &mut p);
                p.clamp(0.0, 1.0)
            }
        }
    }

    impl Drop for MeterCtl {
        fn drop(&mut self) {
            unsafe { release(self.meter) }
        }
    }

    /// Find the first capture endpoint whose name contains `needle` (case-insensitive).
    /// This is how a binding resolves "my real mic" to a concrete endpoint id.
    pub fn find_capture(needle: &str) -> Option<Endpoint> {
        let n = needle.to_lowercase();
        // light path: we only need name+id to resolve, not every endpoint's volume.
        collect(Flow::Capture, false)
            .into_iter()
            .find(|e| e.name.to_lowercase().contains(&n))
    }

    /// Resolve the capture endpoint to act on: explicit needle, else the user's Razer/Seiren
    /// mic, else the system's first capture endpoint. Shared by the CLI and the run daemon.
    pub fn resolve_capture(device: Option<&str>) -> Option<Endpoint> {
        if let Some(n) = device {
            return find_capture(n);
        }
        for needle in ["seiren", "razer"] {
            if let Some(e) = find_capture(needle) {
                return Some(e);
            }
        }
        collect(Flow::Capture, false).into_iter().next()
    }

    /// Find the first RENDER endpoint (speakers / headphones / sound card) whose name contains
    /// `needle` (case-insensitive) — the output mirror of [`find_capture`].
    pub fn find_render(needle: &str) -> Option<Endpoint> {
        let n = needle.to_lowercase();
        collect(Flow::Render, false)
            .into_iter()
            .find(|e| e.name.to_lowercase().contains(&n))
    }

    /// Resolve the RENDER endpoint to act on (headset / sound card / speakers): explicit needle,
    /// else the actual system DEFAULT output (the device whose volume the OSD shows), falling back
    /// to the first active render endpoint. Generic — no hardcoded device; works for any
    /// headphone/sound-card the OS exposes.
    pub fn resolve_render(device: Option<&str>) -> Option<Endpoint> {
        if let Some(n) = device {
            return find_render(n);
        }
        let all = collect(Flow::Render, false);
        // "the current output" = the default endpoint, NOT whatever enumerates first. Turning the
        // wrong endpoint's master volume is silent (the OSD never moves) — match the default by id.
        if let Some(def) = default_render_id() {
            if let Some(e) = all.iter().find(|e| e.id == def) {
                return Some(e.clone());
            }
        }
        all.into_iter().next()
    }

    /// The id of the current DEFAULT render endpoint (the eConsole role) — the "current output".
    pub fn default_render_id() -> Option<String> {
        com_init();
        let en = create_enumerator();
        if en.is_null() {
            return None;
        }
        unsafe {
            let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
            let mut dev: *mut c_void = std::ptr::null_mut();
            // (Render = 0, eConsole = 0)
            let hr = ((*evt).get_default)(en, 0, 0, &mut dev);
            release(en);
            if hr < 0 || dev.is_null() {
                return None;
            }
            let dvt = vtbl::<ImmDeviceVtbl>(dev);
            let mut idp: *mut u16 = std::ptr::null_mut();
            let id = if ((*dvt).get_id)(dev, &mut idp) >= 0 && !idp.is_null() {
                let s = wide_to_string(idp);
                CoTaskMemFree(idp as *mut _);
                s
            } else {
                String::new()
            };
            release(dev);
            (!id.is_empty()).then_some(id)
        }
    }

    // ── OUTPUT FLIP via the (undocumented but well-known) IPolicyConfig ──
    const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870AF99C_171D_4F9E_AF0D_E63DF40C2BC9);
    const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xF8679F50_850A_41CF_9C72_430F290290C8);

    /// IPolicyConfig — only `set_default_endpoint` is called; the earlier slots are declared (in
    /// interface order) purely so its vtable offset (13) lands correctly. ABI padding.
    #[repr(C)]
    struct PolicyConfigVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        _get_mix_format: Ph,
        _get_device_format: Ph,
        _reset_device_format: Ph,
        _set_device_format: Ph,
        _get_processing_period: Ph,
        _set_processing_period: Ph,
        _get_share_mode: Ph,
        _set_share_mode: Ph,
        _get_property_value: Ph,
        _set_property_value: Ph,
        set_default_endpoint: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT,
        _set_endpoint_visibility: Ph,
    }

    /// Make `id` the default render endpoint for every role (console, multimedia, comms) — exactly
    /// what flipping the device in Sound settings does. Returns true on success.
    fn set_default_endpoint(id: &str) -> bool {
        com_init();
        unsafe {
            let mut pc: *mut c_void = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_POLICY_CONFIG,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_POLICY_CONFIG,
                &mut pc,
            );
            if hr < 0 || pc.is_null() {
                return false;
            }
            let vt = vtbl::<PolicyConfigVtbl>(pc);
            let wid = to_wide(id);
            // eConsole=0, eMultimedia=1, eCommunications=2 — set all so it's the default everywhere
            let mut ok = true;
            for role in 0..3 {
                ok &= ((*vt).set_default_endpoint)(pc, wid.as_ptr(), role) >= 0;
            }
            release(pc);
            ok
        }
    }

    /// The ordered set of render endpoints OUTPUT FLIP / its fan offers: the named ones that are
    /// actually present (in the order given — the user's intended cycle), else everything
    /// connected. Disconnected names are simply absent (remembered by name, skipped when away).
    pub fn flip_candidates(names: &[String]) -> Vec<Endpoint> {
        let all = collect(Flow::Render, false);
        if names.is_empty() {
            all
        } else {
            names
                .iter()
                .filter_map(|n| {
                    let nl = n.to_lowercase();
                    all.iter()
                        .find(|e| e.name.to_lowercase().contains(&nl))
                        .cloned()
                })
                .collect()
        }
    }

    /// Make `id` the default render endpoint (all roles) — the fan's pick commits through here.
    pub fn set_default(id: &str) -> bool {
        set_default_endpoint(id)
    }

    /// OUTPUT FLIP: step the default render endpoint to the NEXT in `names` (substring matches,
    /// disconnected ones simply absent so they're skipped); empty `names` = cycle every connected
    /// render endpoint. Returns the status line for the readout.
    pub fn flip_output(names: &[String]) -> String {
        let candidates = flip_candidates(names);
        if candidates.is_empty() {
            return "no output devices connected".into();
        }
        if candidates.len() == 1 {
            let e = &candidates[0];
            return if set_default_endpoint(&e.id) {
                format!("output \u{2192} {}", e.name)
            } else {
                format!("output flip failed ({})", e.name)
            };
        }
        // step from the current default to the next candidate (wrapping)
        let cur = default_render_id();
        let idx = cur
            .as_deref()
            .and_then(|c| candidates.iter().position(|e| e.id == c))
            .unwrap_or(usize::MAX);
        let next = &candidates[(idx.wrapping_add(1)) % candidates.len()];
        if set_default_endpoint(&next.id) {
            format!("output \u{2192} {}", next.name)
        } else {
            format!("output flip failed ({})", next.name)
        }
    }
}

/// The inert OS-audio-control backend — ALWAYS compiled (NOT cfg-gated), exactly like
/// `wm.rs`'s `stub::Null`: the no-op bodies touch no platform API, so compiling them on every
/// target costs nothing and keeps the off-Windows surface type-checked on each build (drift in
/// the inert form can't hide until someone cross-compiles). Off-Windows there is no Core-Audio /
/// ALSA / CoreAudio backend wired yet, so every read answers empty/None/0.0/false and every act
/// is a no-op — an HONEST silent surface, not a fake. A real Linux/macOS backend replaces this
/// `mod` with one that implements the same names. `#![allow(dead_code)]`: on Windows nothing in
/// this module is reached (the live surface is `mod imp`), so its items would otherwise warn.
mod stub {
    #![allow(dead_code)]
    use super::{Endpoint, Flow};

    /// Enumerate active endpoints of one flow — none, with no audio backend on this platform.
    pub fn endpoints(_flow: Flow) -> Vec<Endpoint> {
        Vec::new()
    }

    /// A live handle to one endpoint's volume control. Inert off-Windows: there is no Core-Audio
    /// here, so `open` never resolves a handle and every verb no-ops on the reads' resting values.
    pub struct VolumeCtl;

    impl VolumeCtl {
        /// No audio backend → no handle to open.
        pub fn open(_id: &str) -> Option<Self> {
            None
        }
        pub fn get_volume(&self) -> f32 {
            0.0
        }
        pub fn set_volume(&self, _scalar: f32) -> bool {
            false
        }
        pub fn nudge(&self, _delta: f32) -> f32 {
            0.0
        }
        pub fn get_mute(&self) -> bool {
            false
        }
        pub fn set_mute(&self, _mute: bool) -> bool {
            false
        }
        pub fn toggle_mute(&self) -> bool {
            false
        }
    }

    /// A live signal-level meter on an audio endpoint — the **peak sample value** (0.0..=1.0) the
    /// OS computes for whatever is currently playing/recording. This is the real audio signal (not
    /// the volume *setting* `VolumeCtl` reads), so it drives the `audiometer` lighting effect: the
    /// keyboard dances to the sound coming out of the speakers. A no-op stub off-Windows — it never
    /// opens, so the effect idles near-dark (an honest silent meter).
    pub struct MeterCtl;

    impl MeterCtl {
        pub fn open_default_render() -> Option<Self> {
            None
        }
        pub fn peak(&self) -> f32 {
            0.0
        }
    }

    pub fn find_capture(_needle: &str) -> Option<Endpoint> {
        None
    }

    pub fn resolve_capture(_device: Option<&str>) -> Option<Endpoint> {
        None
    }

    pub fn find_render(_needle: &str) -> Option<Endpoint> {
        None
    }

    pub fn resolve_render(_device: Option<&str>) -> Option<Endpoint> {
        None
    }

    pub fn default_render_id() -> Option<String> {
        None
    }

    pub fn flip_candidates(_names: &[String]) -> Vec<Endpoint> {
        Vec::new()
    }

    pub fn set_default(_id: &str) -> bool {
        false
    }

    /// OUTPUT FLIP off-Windows: nothing to flip — say so plainly.
    pub fn flip_output(_names: &[String]) -> String {
        "no audio backend on this platform".into()
    }
}
