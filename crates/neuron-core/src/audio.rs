// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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
//! A real ALSA/Pulse (Linux) or `CoreAudio` (macOS) backend is then a single `mod` of the same names.

#[cfg(windows)]
pub use imp::*;

#[cfg(target_os = "linux")]
#[path = "audio_linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(any(windows, target_os = "linux")))]
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
    /// `EDataFlow` value expected by `IMMDeviceEnumerator::EnumAudioEndpoints`.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn edata(self) -> i32 {
        match self {
            Flow::Render => 0,
            Flow::Capture => 1,
        }
    }
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Flow::Capture => "capture",
            Flow::Render => "render",
        }
    }
}

/// THE endpoint-identity predicate: does an OS audio endpoint's display name identify `product`
/// (a USB product string, a def `name`, or a user-config needle)? Case-insensitive containment —
/// an OS endpoint name WRAPS the product string (e.g. "Microphone (2- Razer Seiren V3 Mini)"
/// contains "Razer Seiren V3 Mini"), so containment in this one direction is the identity test.
///
/// This is the ONE place that owns the question. Every site that maps between a HID-side product
/// and a Core-Audio endpoint (`find_capture`/`find_render`, the app's hardware-mute capability
/// gates, the UI's source-matched mute notify) routes through here, so the scheme has a single
/// upgrade point when containment someday needs to become something stronger (a container-id
/// join, say) — and a single set of tests pinning it.
///
/// An EMPTY (or all-whitespace) `product` identifies NOTHING and returns false. An empty needle
/// is a substring of every name, so treating it as a match would resolve "unknown device" to
/// "whichever endpoint enumerates first" — exactly the wrong-device mute write a review caught.
/// That invariant is enforced HERE, at the root, not by guard comments at call sites.
#[must_use]
pub fn endpoint_matches_product(endpoint_name: &str, product: &str) -> bool {
    let p = product.trim();
    !p.is_empty() && endpoint_name.to_lowercase().contains(&p.to_lowercase())
}

/// Is `id` the CURRENT DEFAULT capture endpoint's id?
///
/// The default capture endpoint is the ONE mic the app speaks for globally: it is what the UI's
/// `mic-muted` pill represents and the only stream the dispatch mic-tap detector samples. So both
/// halves of the mute story must be scoped to it — a write to a SECONDARY mic must not arm the
/// (process-global) echo latch dispatch consumes, or it could swallow a real external edge on the
/// default mic whenever the two happen to agree on a value. Kept here beside
/// [`endpoint_matches_product`] so the "is this the mic we speak for?" question has ONE answer, at
/// the root, rather than a re-derived comparison at each call site.
#[must_use]
pub fn is_default_capture_id(id: &str) -> bool {
    resolve_capture(None).is_some_and(|ep| ep.id == id)
}

/// Does `product` identify the CURRENT DEFAULT capture endpoint (see [`is_default_capture_id`])?
/// The product-name form, for event sources that know a device by its product string rather than an
/// endpoint id — e.g. a hardware-mute push, which must only speak for the pill when the mic that
/// pushed it IS the default one.
#[must_use]
pub fn default_capture_matches_product(product: &str) -> bool {
    resolve_capture(None).is_some_and(|ep| endpoint_matches_product(&ep.name, product))
}

/// Normalize an OPTIONAL explicit device needle from config/CLI: a PRESENT-BUT-BLANK string
/// (`device = ""` in a binding, `--device ""` on the CLI) means "no explicit device", exactly like
/// an absent field — so the resolvers fall through to their preference order. Before this, a blank
/// needle rode `find_capture("")` straight to "whichever endpoint enumerates first": a silent
/// wrong-device pick reachable from every config-sourced `device:` field. Shared by
/// `resolve_capture`/`resolve_render` so the rule can't drift between flows.
pub fn explicit_needle(device: Option<&str>) -> Option<&str> {
    device.map(str::trim).filter(|n| !n.is_empty())
}

/// THE decision behind `resolve_capture` (and the capture half of every other by-name resolver):
/// given an already-enumerated candidate list (in enumeration order — the same order the OS
/// handed back, never re-sorted) and an optional EXPLICIT needle, pick which endpoint to act on.
///
/// Order of precedent, pinned here so a COM-side caller never has to re-derive it:
///   1. an explicit needle (already passed through [`explicit_needle`]) wins outright — the first
///      candidate it identifies via [`endpoint_matches_product`], enumeration order breaking ties;
///   2. failing that, each of `fallback_needles` is tried IN THE ORDER GIVEN — the first needle
///      with any match wins, even if a LATER needle would have matched an EARLIER-enumerated
///      candidate (needle priority beats enumeration order — pin: this is what lets a user's
///      "seiren" preference beat a "razer"-branded device that happens to enumerate first);
///   3. failing every needle, the first enumerated candidate — "something connected" beats "no
///      device", the same "arbitrary but deterministic" fallback `resolve_capture` always used;
///   4. an empty candidate list has nothing to pick — `None`.
///
/// Split out of `imp::resolve_capture` so the preference order is unit-testable without a live
/// COM enumerator (the COM side now does exactly one `collect()` and calls this).
#[must_use]
pub fn pick_by_needles<'a>(
    candidates: &'a [Endpoint],
    explicit: Option<&str>,
    fallback_needles: &[&str],
) -> Option<&'a Endpoint> {
    if let Some(n) = explicit {
        return candidates.iter().find(|e| endpoint_matches_product(&e.name, n));
    }
    for needle in fallback_needles {
        if let Some(e) = candidates.iter().find(|e| endpoint_matches_product(&e.name, needle)) {
            return Some(e);
        }
    }
    candidates.first()
}

/// THE decision behind `resolve_render`: the output mirror of [`pick_by_needles`], but the
/// no-explicit-needle fallback is "the current system DEFAULT endpoint" (matched by id) rather
/// than a fixed needle list — turning the wrong endpoint's master volume is silent (the OSD never
/// moves), so matching the real default is what makes "the current output" mean anything.
/// Precedent: explicit needle wins outright; else the candidate whose `id` equals `default_id`
/// (when one is given and it actually matches a candidate); else the first enumerated candidate;
/// else `None`. Split out for the same reason as `pick_by_needles` — one place, one set of tests,
/// pinning the order without a live COM enumerator or a live default-endpoint call.
#[must_use]
pub fn pick_render<'a>(
    candidates: &'a [Endpoint],
    explicit: Option<&str>,
    default_id: Option<&str>,
) -> Option<&'a Endpoint> {
    if let Some(n) = explicit {
        return candidates.iter().find(|e| endpoint_matches_product(&e.name, n));
    }
    if let Some(def) = default_id {
        if let Some(e) = candidates.iter().find(|e| e.id == def) {
            return Some(e);
        }
    }
    candidates.first()
}

#[cfg(test)]
mod identity_tests {
    use super::endpoint_matches_product;

    #[test]
    fn os_wrapped_product_names_match_case_insensitively() {
        // the real shape: Windows wraps the USB product string in a form-factor prefix + a
        // duplicate-ordinal ("2-") — containment must see through both, and through case.
        assert!(endpoint_matches_product(
            "Microphone (2- Razer Seiren V3 Mini)",
            "Razer Seiren V3 Mini"
        ));
        assert!(endpoint_matches_product("Microphone (RAZER SEIREN V3 MINI)", "razer seiren v3 mini"));
        // identity is per-DEVICE: a different mic's endpoint must never claim this product.
        assert!(!endpoint_matches_product("Microphone (HyperX QuadCast)", "Razer Seiren V3 Mini"));
        // and a bare fragment still resolves (user-config needles are partial by design).
        assert!(endpoint_matches_product("Microphone (2- Razer Seiren V3 Mini)", "seiren"));
    }

    #[test]
    fn empty_product_identifies_nothing() {
        // the empty-needle hazard, pinned: "" (and whitespace) is a substring of EVERY endpoint
        // name, so it must identify NO endpoint — else an unidentified device's mute event would
        // land on whichever mic enumerates first (the wrong-device write a review caught).
        assert!(!endpoint_matches_product("Microphone (2- Razer Seiren V3 Mini)", ""));
        assert!(!endpoint_matches_product("Microphone (2- Razer Seiren V3 Mini)", "   "));
        assert!(!endpoint_matches_product("", ""));
    }

    #[test]
    fn blank_explicit_needle_counts_as_absent() {
        use super::explicit_needle;
        // a config field that EXISTS but is blank (`device = ""`) is "no explicit device" — the
        // resolvers must fall to their preference order, never resolve "" to the first endpoint.
        assert_eq!(explicit_needle(None), None);
        assert_eq!(explicit_needle(Some("")), None);
        assert_eq!(explicit_needle(Some("   ")), None);
        // a real needle passes through, trimmed (a stray space in config still resolves).
        assert_eq!(explicit_needle(Some(" seiren ")), Some("seiren"));
        assert_eq!(explicit_needle(Some("Razer Seiren V3 Mini")), Some("Razer Seiren V3 Mini"));
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::{pick_by_needles, pick_render, Endpoint, Flow};

    fn ep(id: &str, name: &str, flow: Flow) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: name.into(),
            flow,
            volume: 0.0,
            muted: false,
        }
    }

    // ── pick_by_needles (resolve_capture's decision) ────────────────────────────────────────

    #[test]
    fn explicit_needle_wins_even_when_a_fallback_needle_would_also_match() {
        let cands = vec![
            ep("1", "Microphone (Razer Seiren V3 Mini)", Flow::Capture),
            ep("2", "HyperX QuadCast", Flow::Capture),
        ];
        // explicit "quadcast" must win outright — the fallback list is never consulted.
        let picked = pick_by_needles(&cands, Some("quadcast"), &["seiren", "razer"]).unwrap();
        assert_eq!(picked.id, "2");
    }

    #[test]
    fn explicit_needle_matches_case_insensitively_via_the_shared_predicate() {
        let cands = vec![ep("1", "Microphone (RAZER SEIREN V3 MINI)", Flow::Capture)];
        let picked = pick_by_needles(&cands, Some("razer seiren v3 mini"), &[]).unwrap();
        assert_eq!(picked.id, "1");
    }

    #[test]
    fn no_explicit_needle_falls_to_first_fallback_that_matches_anything() {
        // "seiren" matches nothing here; "razer" is the first fallback that does.
        let cands = vec![
            ep("1", "Microphone (HyperX QuadCast)", Flow::Capture),
            ep("2", "Microphone (Razer Barracuda)", Flow::Capture),
        ];
        let picked = pick_by_needles(&cands, None, &["seiren", "razer"]).unwrap();
        assert_eq!(picked.id, "2");
    }

    #[test]
    fn fallback_needle_priority_beats_enumeration_order() {
        // "razer" enumerates FIRST, "seiren" second — but fallback order is ["seiren", "razer"],
        // so the LATER-enumerated "seiren" endpoint wins: needle preference order, not scan order.
        let cands = vec![
            ep("1", "Microphone (Razer Barracuda)", Flow::Capture),
            ep("2", "Microphone (Razer Seiren V3 Mini)", Flow::Capture),
        ];
        let picked = pick_by_needles(&cands, None, &["seiren", "razer"]).unwrap();
        assert_eq!(picked.id, "2");
    }

    #[test]
    fn no_needle_matches_anything_falls_to_first_enumerated_candidate() {
        let cands = vec![
            ep("1", "Microphone (Built-in)", Flow::Capture),
            ep("2", "Microphone (Webcam)", Flow::Capture),
        ];
        let picked = pick_by_needles(&cands, None, &["seiren", "razer"]).unwrap();
        assert_eq!(picked.id, "1"); // "something connected" — first enumerated, no needle involved
    }

    #[test]
    fn empty_candidate_list_resolves_to_none_regardless_of_needles() {
        assert!(pick_by_needles(&[], Some("seiren"), &["razer"]).is_none());
        assert!(pick_by_needles(&[], None, &["seiren", "razer"]).is_none());
    }

    // ── pick_render (resolve_render's decision) ─────────────────────────────────────────────

    #[test]
    fn render_explicit_needle_wins_over_the_default_id() {
        let cands = vec![
            ep("dev-1", "Speakers (Razer)", Flow::Render),
            ep("dev-2", "Headset (Generic)", Flow::Render),
        ];
        // dev-1 is the system default, but an explicit needle for "headset" must still win.
        let picked = pick_render(&cands, Some("headset"), Some("dev-1")).unwrap();
        assert_eq!(picked.id, "dev-2");
    }

    #[test]
    fn render_no_explicit_needle_falls_to_the_default_id_match() {
        let cands = vec![
            ep("dev-1", "Speakers", Flow::Render),
            ep("dev-2", "Headset", Flow::Render),
        ];
        let picked = pick_render(&cands, None, Some("dev-2")).unwrap();
        assert_eq!(picked.id, "dev-2");
    }

    #[test]
    fn render_default_id_that_matches_no_candidate_falls_to_first_enumerated() {
        // the "current default" id came back stale/disconnected — never silently pick nothing.
        let cands = vec![
            ep("dev-1", "Speakers", Flow::Render),
            ep("dev-2", "Headset", Flow::Render),
        ];
        let picked = pick_render(&cands, None, Some("dev-unplugged")).unwrap();
        assert_eq!(picked.id, "dev-1");
    }

    #[test]
    fn render_empty_candidates_resolves_to_none() {
        assert!(pick_render(&[], Some("headset"), Some("dev-1")).is_none());
        assert!(pick_render(&[], None, None).is_none());
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

    /// Initialise COM (MTA) for this thread. `S_FALSE` (already-init) is fine.
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
                &raw mut p,
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
        if ((*vt).get_id)(dev, &raw mut idp) < 0 {
            return String::new();
        }
        let s = wide_to_string(idp);
        CoTaskMemFree(idp as *const c_void);
        s
    }

    unsafe fn device_name(dev: *mut c_void) -> String {
        let vt = vtbl::<ImmDeviceVtbl>(dev);
        let mut store: *mut c_void = std::ptr::null_mut();
        if ((*vt).open_property_store)(dev, STGM_READ, &raw mut store) < 0 || store.is_null() {
            return String::new();
        }
        let svt = vtbl::<IPropertyStoreVtbl>(store);
        let mut pv = PropVariant::zeroed();
        let mut name = String::new();
        if ((*svt).get_value)(store, &PKEY_DEVICE_FRIENDLY_NAME, &raw mut pv) >= 0 && pv.vt == VT_LPWSTR
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
            &raw mut vol,
        );
        if hr < 0 || vol.is_null() {
            return (std::ptr::null_mut(), 0.0, false);
        }
        let vvt = vtbl::<IAudioEndpointVolumeVtbl>(vol);
        let mut scalar = 0f32;
        let _ = ((*vvt).get_master_scalar)(vol, &raw mut scalar);
        let mut m = 0i32;
        let _ = ((*vvt).get_mute)(vol, &raw mut m);
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
            if ((*evt).enum_audio_endpoints)(en, flow.edata(), DEVICE_STATE_ACTIVE, &raw mut coll) >= 0
                && !coll.is_null()
            {
                let cvt = vtbl::<ImmDeviceCollectionVtbl>(coll);
                let mut n = 0u32;
                let _ = ((*cvt).get_count)(coll, &raw mut n);
                for i in 0..n {
                    let mut dev: *mut c_void = std::ptr::null_mut();
                    if ((*cvt).item)(coll, i, &raw mut dev) < 0 || dev.is_null() {
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
    #[must_use]
    pub fn endpoints(flow: Flow) -> Vec<Endpoint> {
        collect(flow, true)
    }

    /// A live handle to one endpoint's volume control. Releases the COM object on drop.
    pub struct VolumeCtl {
        vol: *mut c_void,
    }

    impl VolumeCtl {
        /// Open by exact endpoint id (as returned in `Endpoint::id`).
        #[must_use]
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
                let hr = ((*evt).get_device)(en, wid.as_ptr(), &raw mut dev);
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

        #[must_use]
        pub fn get_volume(&self) -> f32 {
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                let mut s = 0f32;
                let _ = ((*vt).get_master_scalar)(self.vol, &raw mut s);
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

        /// The current OS mute state, or `None` if the `GetMute` call FAILED — a failed read must
        /// not masquerade as a definite `false`, or a caller could skip a needed write (see
        /// `set_mute`). The public [`get_mute`](Self::get_mute) keeps the historical infallible
        /// `bool` (failure → `false`) for callers that only display it.
        fn try_get_mute(&self) -> Option<bool> {
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                let mut m = 0i32;
                if ((*vt).get_mute)(self.vol, &raw mut m) < 0 {
                    return None; // GetMute failed — we do NOT know the state
                }
                Some(m != 0)
            }
        }
        #[must_use]
        pub fn get_mute(&self) -> bool {
            self.try_get_mute().unwrap_or(false)
        }
        /// Set the mute state. Returns `true` only if the OS state ACTUALLY CHANGED — i.e. the write
        /// both was needed (the endpoint didn't already hold `mute`) AND succeeded (`SetMute`'s
        /// HRESULT is checked). Callers use that to arm the mic-tap self-write window ONLY on a real
        /// transition: a redundant OR failed write produces no edge for the dispatch poll to see, so
        /// it must not open a window that would shadow a genuine tap, nor publish a value we didn't
        /// actually set.
        ///
        /// The redundant-write skip fires ONLY on a TRUSTED read (`try_get_mute` returned `Some`).
        /// If the read failed we do not know the state, so we attempt the write rather than assume
        /// it's already correct — the safe direction, since a needless write is harmless but a
        /// skipped needed one leaves the mic wrong.
        pub fn set_mute(&self, mute: bool) -> bool {
            if self.try_get_mute() == Some(mute) {
                return false; // trusted read says already there — no transition
            }
            unsafe {
                let vt = vtbl::<IAudioEndpointVolumeVtbl>(self.vol);
                ((*vt).set_mute)(self.vol, i32::from(mute), std::ptr::null()) >= 0 // true only if it landed
            }
        }
        pub fn toggle_mute(&self) -> bool {
            // Flip relative to the current state (best-effort read; `get_mute` → `false` on a failed
            // read, so a toggle from an unknown state still moves it). Returns the value we AIMED to
            // set; `set_mute` reports whether it actually landed for the self-write-window callers.
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
            &raw mut meter,
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
        #[must_use]
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
                let hr = ((*evt).get_default)(en, 0, 0, &raw mut dev);
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

        /// Open the meter on an EXACT endpoint id (capture OR render) — so a channel strip can show
        /// the live level of the specific device it controls, not just the default output. Mirror of
        /// [`VolumeCtl::open`], activating the meter interface instead of the volume one.
        #[must_use]
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
                let hr = ((*evt).get_device)(en, wid.as_ptr(), &raw mut dev);
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

        /// The current peak sample value (0.0..=1.0) since the last read — the live loudness, or
        /// `None` if the OS rejected the call. CAPTURING the HRESULT is the point: when the endpoint
        /// is invalidated (the default device changed, the endpoint went to sleep) `GetPeakValue`
        /// returns a failure HRESULT (e.g. `AUDCLNT_E_DEVICE_INVALIDATED`); a caller that discards it
        /// reads 0.0 forever with a dead handle. The sampler uses this to DROP and re-open on failure
        /// so a device change self-heals instead of zeroing the meter permanently.
        #[must_use]
        pub fn try_peak(&self) -> Option<f32> {
            unsafe {
                let vt = vtbl::<IAudioMeterInformationVtbl>(self.meter);
                let mut p = 0f32;
                let hr = ((*vt).get_peak_value)(self.meter, &raw mut p);
                if hr < 0 {
                    None
                } else {
                    Some(p.clamp(0.0, 1.0))
                }
            }
        }

        /// The current peak sample value (0.0..=1.0), or 0.0 on failure — the lenient read kept for
        /// callers that don't distinguish "silent" from "handle dead". The live sampler uses the
        /// error-aware [`try_peak`](Self::try_peak) instead so it can re-open an invalidated handle.
        #[must_use]
        pub fn peak(&self) -> f32 {
            self.try_peak().unwrap_or(0.0)
        }
    }

    impl Drop for MeterCtl {
        fn drop(&mut self) {
            unsafe { release(self.meter) }
        }
    }

    // ── WASAPI PCM capture (the spectrum analyser's input) ──────────────────────────────────
    //
    // `MeterCtl` reads ONE number (the OS peak) — enough for a VU bar, useless for a spectrum.
    // `CaptureCtl` opens a real WASAPI shared-mode capture stream: LOOPBACK on the default render
    // endpoint ("what my speakers are playing", exactly what Synapse's Audio Meter taps) or a plain
    // capture stream on a mic endpoint. Same hand-rolled COM discipline as everything above.

    const IID_IAUDIO_CLIENT: GUID = GUID::from_u128(0x1CB9AD4C_DBFA_4C32_B178_C2F568A703B2);
    const IID_IAUDIO_CAPTURE_CLIENT: GUID = GUID::from_u128(0xC8ADBD64_E71E_48A0_A4DE_185C395CD317);

    const AUDCLNT_SHAREMODE_SHARED: i32 = 0;
    const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
    /// `AUDCLNT_BUFFERFLAGS_SILENT` — the packet's data is to be TREATED as zeros.
    const BUFFERFLAGS_SILENT: u32 = 0x2;
    /// 200ms shared buffer — deep enough that a ~60Hz drain never overruns.
    const CAPTURE_BUF_HNS: i64 = 2_000_000;

    // IAudioClient — Initialize/GetMixFormat/Start/Stop/GetService; the rest are ABI placeholders.
    #[repr(C)]
    struct IAudioClientVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        initialize: unsafe extern "system" fn(
            *mut c_void,
            i32,        // share mode
            u32,        // stream flags
            i64,        // buffer duration (hns)
            i64,        // periodicity (hns)
            *const u8,  // WAVEFORMATEX*
            *const GUID,
        ) -> HRESULT,
        _get_buffer_size: Ph,
        _get_stream_latency: Ph,
        _get_current_padding: Ph,
        _is_format_supported: Ph,
        get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut u8) -> HRESULT,
        _get_device_period: Ph,
        start: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        stop: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        _reset: Ph,
        _set_event_handle: Ph,
        get_service:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IAudioCaptureClientVtbl {
        _qi: Ph,
        _add_ref: Ph,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_buffer: unsafe extern "system" fn(
            *mut c_void,
            *mut *mut u8, // data
            *mut u32,     // frames read
            *mut u32,     // flags
            *mut u64,     // device position (unused)
            *mut u64,     // QPC position (unused)
        ) -> HRESULT,
        release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
        get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    }

    /// WAVEFORMATEX header, byte-exact (`packed(2)` matches the Win32 layout; 18 bytes). Only ever
    /// READ from the pointer `GetMixFormat` returns — never constructed or passed by value.
    #[repr(C, packed(2))]
    struct WaveFormatEx {
        tag: u16,
        channels: u16,
        rate: u32,
        _avg_bytes: u32,
        _block_align: u16,
        bits: u16,
        cb_size: u16,
    }

    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    /// The two shared-mode sample layouts worth decoding (the mixer hands out float32 in practice;
    /// int16 kept for completeness). Anything else fails the open honestly.
    #[derive(Clone, Copy, PartialEq)]
    enum SampleFmt {
        F32,
        I16,
    }

    /// Parse the (rate, channels, sample format) out of a `GetMixFormat` result. For an EXTENSIBLE
    /// format the tag lives in `SubFormat.Data1` (byte offset 24: the 18-byte header + wValidBits u16
    /// and dwChannelMask u32) — the standard KSDATAFORMAT subtypes share their GUID tail, so `Data1`
    /// alone disambiguates float vs PCM.
    unsafe fn parse_mix_format(p: *const u8) -> Option<(u32, u16, SampleFmt)> {
        let f = &*p.cast::<WaveFormatEx>();
        let (tag, bits, cb) = (f.tag, f.bits, f.cb_size);
        let eff_tag = if tag == WAVE_FORMAT_EXTENSIBLE && cb >= 22 {
            std::ptr::read_unaligned(p.add(24).cast::<u32>()) as u16
        } else {
            tag
        };
        let fmt = match (eff_tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFmt::F32,
            (WAVE_FORMAT_PCM, 16) => SampleFmt::I16,
            _ => return None,
        };
        let (rate, channels) = (f.rate, f.channels);
        (rate > 0 && channels > 0).then_some((rate, channels, fmt))
    }

    /// A live shared-mode PCM capture stream on one endpoint — loopback (the sound the speakers are
    /// playing) or a mic. Drained by [`read_into`](Self::read_into) as mono f32; releases the COM
    /// objects (stopping the stream) on drop. Created and used on ONE thread (the spectrum sampler),
    /// like every other handle in this module.
    pub struct CaptureCtl {
        client: *mut c_void,
        capture: *mut c_void,
        rate: u32,
        channels: u16,
        fmt: SampleFmt,
    }

    impl CaptureCtl {
        /// Open a LOOPBACK capture on the current default render endpoint — the analyser's
        /// "speakers" source. `None` if anything in the chain fails to resolve.
        #[must_use]
        pub fn open_loopback_default() -> Option<Self> {
            com_init();
            let en = create_enumerator();
            if en.is_null() {
                return None;
            }
            unsafe {
                let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
                let mut dev: *mut c_void = std::ptr::null_mut();
                // (Render = 0, eConsole = 0) — the output the user actually hears.
                let hr = ((*evt).get_default)(en, 0, 0, &raw mut dev);
                release(en);
                if hr < 0 || dev.is_null() {
                    return None;
                }
                let ctl = Self::from_device(dev, true);
                release(dev);
                ctl
            }
        }

        /// Open a plain capture stream on an EXACT endpoint id (a mic) — the analyser's "mic" source.
        #[must_use]
        pub fn open_capture(id: &str) -> Option<Self> {
            com_init();
            let en = create_enumerator();
            if en.is_null() {
                return None;
            }
            unsafe {
                let evt = vtbl::<ImmDeviceEnumeratorVtbl>(en);
                let wid = to_wide(id);
                let mut dev: *mut c_void = std::ptr::null_mut();
                let hr = ((*evt).get_device)(en, wid.as_ptr(), &raw mut dev);
                release(en);
                if hr < 0 || dev.is_null() {
                    return None;
                }
                let ctl = Self::from_device(dev, false);
                release(dev);
                ctl
            }
        }

        /// Activate + initialize + start the capture chain on an already-resolved device. Any failure
        /// releases whatever was acquired and answers `None` — a half-open stream never escapes.
        unsafe fn from_device(dev: *mut c_void, loopback: bool) -> Option<Self> {
            let dvt = vtbl::<ImmDeviceVtbl>(dev);
            let mut client: *mut c_void = std::ptr::null_mut();
            if ((*dvt).activate)(dev, &IID_IAUDIO_CLIENT, CLSCTX_ALL, std::ptr::null_mut(), &raw mut client) < 0
                || client.is_null()
            {
                return None;
            }
            let cvt = vtbl::<IAudioClientVtbl>(client);
            let mut fmt_ptr: *mut u8 = std::ptr::null_mut();
            if ((*cvt).get_mix_format)(client, &raw mut fmt_ptr) < 0 || fmt_ptr.is_null() {
                release(client);
                return None;
            }
            let parsed = parse_mix_format(fmt_ptr);
            let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            let hr_init = match parsed {
                Some(_) => ((*cvt).initialize)(
                    client,
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    CAPTURE_BUF_HNS,
                    0,
                    fmt_ptr,
                    std::ptr::null(),
                ),
                None => -1,
            };
            CoTaskMemFree(fmt_ptr as *const c_void);
            let Some((rate, channels, fmt)) = parsed else {
                release(client);
                return None;
            };
            if hr_init < 0 {
                release(client);
                return None;
            }
            let mut capture: *mut c_void = std::ptr::null_mut();
            if ((*cvt).get_service)(client, &IID_IAUDIO_CAPTURE_CLIENT, &raw mut capture) < 0
                || capture.is_null()
            {
                release(client);
                return None;
            }
            if ((*cvt).start)(client) < 0 {
                release(capture);
                release(client);
                return None;
            }
            Some(CaptureCtl {
                client,
                capture,
                rate,
                channels,
                fmt,
            })
        }

        /// The stream's sample rate (Hz) — the mix rate the mono samples arrive at.
        #[must_use]
        pub fn rate(&self) -> u32 {
            self.rate
        }

        /// Drain every packet currently buffered, appending each frame DOWNMIXED to mono f32
        /// (channel average, ±1.0 range) onto `out`. Returns the number of frames appended — `0` is
        /// normal (loopback delivers nothing while no stream plays) — or `None` when the endpoint
        /// died (invalidated/slept), so the caller drops this handle and re-opens, exactly like the
        /// peak sampler's self-heal.
        pub fn read_into(&self, out: &mut Vec<f32>) -> Option<usize> {
            let ch = self.channels as usize;
            let mut appended = 0usize;
            unsafe {
                let vt = vtbl::<IAudioCaptureClientVtbl>(self.capture);
                loop {
                    let mut next = 0u32;
                    if ((*vt).get_next_packet_size)(self.capture, &raw mut next) < 0 {
                        return None;
                    }
                    if next == 0 {
                        return Some(appended);
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    if ((*vt).get_buffer)(
                        self.capture,
                        &raw mut data,
                        &raw mut frames,
                        &raw mut flags,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    ) < 0
                    {
                        return None;
                    }
                    let n = frames as usize;
                    if flags & BUFFERFLAGS_SILENT != 0 || data.is_null() {
                        out.extend(std::iter::repeat_n(0.0, n));
                    } else {
                        match self.fmt {
                            SampleFmt::F32 => {
                                let s = std::slice::from_raw_parts(data as *const f32, n * ch);
                                for f in s.chunks_exact(ch) {
                                    out.push(f.iter().sum::<f32>() / ch as f32);
                                }
                            }
                            SampleFmt::I16 => {
                                let s = std::slice::from_raw_parts(data as *const i16, n * ch);
                                for f in s.chunks_exact(ch) {
                                    let sum: f32 = f.iter().map(|&v| f32::from(v)).sum();
                                    out.push(sum / (ch as f32 * 32768.0));
                                }
                            }
                        }
                    }
                    if ((*vt).release_buffer)(self.capture, frames) < 0 {
                        return None;
                    }
                    appended += n;
                }
            }
        }
    }

    impl Drop for CaptureCtl {
        fn drop(&mut self) {
            unsafe {
                let cvt = vtbl::<IAudioClientVtbl>(self.client);
                let _ = ((*cvt).stop)(self.client);
                release(self.capture);
                release(self.client);
            }
        }
    }

    /// Find the first capture endpoint that IDENTIFIES as `needle` (the shared
    /// [`super::endpoint_matches_product`] predicate — case-insensitive containment, and an EMPTY
    /// needle identifies NOTHING, so this returns `None` rather than an arbitrary first mic).
    /// This is how a binding resolves "my real mic" to a concrete endpoint id.
    #[must_use]
    pub fn find_capture(needle: &str) -> Option<Endpoint> {
        // light path: we only need name+id to resolve, not every endpoint's volume.
        collect(Flow::Capture, false)
            .into_iter()
            .find(|e| super::endpoint_matches_product(&e.name, needle))
    }

    /// Resolve the capture endpoint to act on: explicit needle, else the user's Razer/Seiren
    /// mic, else the system's first capture endpoint. Shared by the CLI and the run daemon.
    /// A blank explicit needle counts as absent ([`super::explicit_needle`]) — it falls to the
    /// preference order, never to "first endpoint". The decision itself (which candidate wins) is
    /// [`super::pick_by_needles`] — pure, unit-tested — so this is just ONE enumeration handed to
    /// it (previously up to three: `find_capture` per fallback needle, then a bare `collect`).
    #[must_use]
    pub fn resolve_capture(device: Option<&str>) -> Option<Endpoint> {
        let candidates = collect(Flow::Capture, false);
        super::pick_by_needles(&candidates, super::explicit_needle(device), &["seiren", "razer"])
            .cloned()
    }

    /// Find the first RENDER endpoint (speakers / headphones / sound card) that identifies as
    /// `needle` — the output mirror of [`find_capture`], same shared identity predicate (empty
    /// needle → `None`).
    #[must_use]
    pub fn find_render(needle: &str) -> Option<Endpoint> {
        collect(Flow::Render, false)
            .into_iter()
            .find(|e| super::endpoint_matches_product(&e.name, needle))
    }

    /// Resolve the RENDER endpoint to act on (headset / sound card / speakers): explicit needle,
    /// else the actual system DEFAULT output (the device whose volume the OSD shows), falling back
    /// to the first active render endpoint. Generic — no hardcoded device; works for any
    /// headphone/sound-card the OS exposes. A blank explicit needle counts as absent, same as
    /// [`resolve_capture`]. The decision is [`super::pick_render`] — pure, unit-tested; the default
    /// endpoint id is only fetched when there's no explicit needle to short-circuit on (the same
    /// COM call the original inline version made, just relocated).
    #[must_use]
    pub fn resolve_render(device: Option<&str>) -> Option<Endpoint> {
        let explicit = super::explicit_needle(device);
        let candidates = collect(Flow::Render, false);
        let default_id = if explicit.is_none() { default_render_id() } else { None };
        super::pick_render(&candidates, explicit, default_id.as_deref()).cloned()
    }

    /// The id of the current DEFAULT render endpoint (the eConsole role) — the "current output".
    #[must_use]
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
            let hr = ((*evt).get_default)(en, 0, 0, &raw mut dev);
            release(en);
            if hr < 0 || dev.is_null() {
                return None;
            }
            let dvt = vtbl::<ImmDeviceVtbl>(dev);
            let mut idp: *mut u16 = std::ptr::null_mut();
            let id = if ((*dvt).get_id)(dev, &raw mut idp) >= 0 && !idp.is_null() {
                let s = wide_to_string(idp);
                CoTaskMemFree(idp.cast());
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

    /// `IPolicyConfig` — only `set_default_endpoint` is called; the earlier slots are declared (in
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
                &raw mut pc,
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
    #[must_use]
    pub fn flip_candidates(names: &[String]) -> Vec<Endpoint> {
        let all = collect(Flow::Render, false);
        if names.is_empty() {
            all
        } else {
            names
                .iter()
                .filter_map(|n| {
                    // the shared identity predicate: a BLANK config entry identifies nothing (it
                    // is skipped like a disconnected device), never "the first endpoint".
                    all.iter()
                        .find(|e| super::endpoint_matches_product(&e.name, n))
                        .cloned()
                })
                .collect()
        }
    }

    /// Make `id` the default render endpoint (all roles) — the fan's pick commits through here.
    #[must_use]
    pub fn set_default(id: &str) -> bool {
        set_default_endpoint(id)
    }

    /// OUTPUT FLIP: step the default render endpoint to the NEXT in `names` (substring matches,
    /// disconnected ones simply absent so they're skipped); empty `names` = cycle every connected
    /// render endpoint. Returns the status line for the readout.
    #[must_use]
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
/// ALSA / `CoreAudio` backend wired yet, so every read answers empty/None/0.0/false and every act
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
        pub fn open(_id: &str) -> Option<Self> {
            None
        }
        /// Error-aware read mirror of `imp` — never opens here, so it's never actually called, but
        /// the surface must match (the maintenance contract in this file's header).
        pub fn try_peak(&self) -> Option<f32> {
            None
        }
        pub fn peak(&self) -> f32 {
            0.0
        }
    }

    /// A live PCM capture stream (loopback / mic) — the spectrum analyser's input. Inert
    /// off-Windows: it never opens, so the analyser reports "not live" and the meter falls back to
    /// the (equally inert) peak provider — the board idles honestly dark.
    pub struct CaptureCtl;

    impl CaptureCtl {
        pub fn open_loopback_default() -> Option<Self> {
            None
        }
        pub fn open_capture(_id: &str) -> Option<Self> {
            None
        }
        pub fn rate(&self) -> u32 {
            0
        }
        /// Surface mirror of `imp` — unreachable (nothing ever opens), kept for the parity contract.
        pub fn read_into(&self, _out: &mut Vec<f32>) -> Option<usize> {
            None
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
