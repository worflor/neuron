// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only: does GameInput see controllers from a background process? Enumerates controller
//! devices and prints each one's generic controller state (axes 0..1, buttons, switches) whenever
//! it changes. Run: gameinput_probe [secs]
#[cfg(windows)]
fn main() {
    use std::ffi::c_void;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    const KIND_CONTROLLER: u32 = 0x0E;
    const STATUS_CONNECTED: u32 = 0x01;
    const BLOCKING_ENUMERATION: u32 = 2;

    // SAFETY (whole block): COM calls through vtables whose slot order and signatures are taken from
    // the Windows SDK 10.0.26100 GameInput.h; every object pointer comes from GameInput itself.
    unsafe {
        let dll: Vec<u16> = "GameInput.dll\0".encode_utf16().collect();
        let lib = LoadLibraryW(dll.as_ptr());
        if lib.is_null() {
            println!("GameInput.dll not available");
            return;
        }
        let Some(create) = GetProcAddress(lib, c"GameInputCreate".as_ptr().cast()) else {
            println!("GameInputCreate missing");
            return;
        };
        let create: extern "system" fn(*mut *mut c_void) -> i32 = std::mem::transmute(create);
        let mut gi: *mut c_void = std::ptr::null_mut();
        let hr = create(&mut gi);
        println!("GameInputCreate hr={hr:#010x}");
        if hr < 0 || gi.is_null() {
            return;
        }
        let slot = |obj: *mut c_void, i: usize| -> usize { *(*(obj as *const *const usize)).add(i) };

        // Blocking enumeration: the callback runs for every connected controller before this returns.
        static DEVICES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
        extern "system" fn on_device(_t: u64, _ctx: *mut c_void, device: *mut c_void, _ts: u64, cur: u32, _prev: u32) {
            if cur & STATUS_CONNECTED != 0 {
                // SAFETY: AddRef keeps the device alive past the callback.
                unsafe {
                    let add_ref: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*(*(device as *const *const usize)).add(1));
                    add_ref(device);
                }
                DEVICES.lock().unwrap().push(device as usize);
            }
        }
        type RegisterDevice = extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32, *mut c_void, extern "system" fn(u64, *mut c_void, *mut c_void, u64, u32, u32), *mut u64) -> i32;
        let register: RegisterDevice = std::mem::transmute(slot(gi, 9));
        let mut token = 0u64;
        let enum_kind: u32 = std::env::var("GI_KIND").ok().and_then(|s| u32::from_str_radix(&s, 16).ok()).unwrap_or(KIND_CONTROLLER);
        let hr = register(gi, std::ptr::null_mut(), enum_kind, STATUS_CONNECTED, BLOCKING_ENUMERATION, std::ptr::null_mut(), on_device, &mut token);
        let devices = DEVICES.lock().unwrap().clone();
        println!("RegisterDeviceCallback hr={hr:#010x}, {} controller(s)", devices.len());
        for &d in &devices {
            let info: extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(slot(d as *mut c_void, 3));
            let p = info(d as *mut c_void);
            let vid = u16::from_le_bytes([*p.add(4), *p.add(5)]);
            let pid = u16::from_le_bytes([*p.add(6), *p.add(7)]);
            let status: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(d as *mut c_void, 4));
            let motors = u32::from_le_bytes([*p.add(108), *p.add(109), *p.add(110), *p.add(111)]);
            let supported = u32::from_le_bytes([*p.add(104), *p.add(105), *p.add(106), *p.add(107)]);
            println!(
                "  device {d:#x}: {vid:04x}:{pid:04x} status {:#x} supportedInput {supported:#x} rumbleMotors {motors:#x}",
                status(d as *mut c_void)
            );
            if std::env::var_os("GI_RUMBLE").is_some() {
                let rumble: extern "system" fn(*mut c_void, *const [f32; 4]) = std::mem::transmute(slot(d as *mut c_void, 10));
                rumble(d as *mut c_void, &[1.0, 1.0, 1.0, 1.0]);
                std::thread::sleep(std::time::Duration::from_millis(1000));
                rumble(d as *mut c_void, &[0.0; 4]);
                println!("    rumbled 1s at full");
            }
        }

        // Can GameInput map a Windows HID interface path to its device? (IGameInput slot 19.)
        if let Ok(infos) = neuron::transport::enumerate() {
            type FromString = extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32;
            let find: FromString = std::mem::transmute(slot(gi, 19));
            for i in infos.iter().filter(|i| i.vid != 0x1532) {
                use std::os::windows::ffi::OsStrExt;
                let w: Vec<u16> = i.path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
                let mut dev: *mut c_void = std::ptr::null_mut();
                let hr = find(gi, w.as_ptr(), &mut dev);
                if hr >= 0 && !dev.is_null() {
                    let info: extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(slot(dev, 3));
                    let p = info(dev);
                    let id: String = (0..8).map(|k| format!("{:02x}", *p.add(32 + k))).collect();
                    println!(
                        "  path {:04x}:{:04x} {} -> gameinput {:04x}:{:04x}#{id}",
                        i.vid, i.pid, i.path.as_os_str().to_string_lossy(),
                        u16::from_le_bytes([*p.add(4), *p.add(5)]), u16::from_le_bytes([*p.add(6), *p.add(7)])
                    );
                }
            }
        }
        type GetReading = extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> i32;
        let get_reading: GetReading = std::mem::transmute(slot(gi, 4));
        let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut last: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        while std::time::Instant::now() < until {
            for &d in &devices {
                let mut r: *mut c_void = std::ptr::null_mut();
                if get_reading(gi, KIND_CONTROLLER, d as *mut c_void, &mut r) < 0 || r.is_null() {
                    continue;
                }
                let n_axes: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(r, 8));
                let axes: extern "system" fn(*mut c_void, u32, *mut f32) -> u32 = std::mem::transmute(slot(r, 9));
                let n_btn: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(r, 10));
                let btns: extern "system" fn(*mut c_void, u32, *mut bool) -> u32 = std::mem::transmute(slot(r, 11));
                let n_sw: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(r, 12));
                let sws: extern "system" fn(*mut c_void, u32, *mut i32) -> u32 = std::mem::transmute(slot(r, 13));
                let mut a = vec![0f32; n_axes(r) as usize];
                axes(r, a.len() as u32, a.as_mut_ptr());
                let mut b = vec![false; n_btn(r) as usize];
                btns(r, b.len() as u32, b.as_mut_ptr());
                let mut s = vec![0i32; n_sw(r) as usize];
                sws(r, s.len() as u32, s.as_mut_ptr());
                let release: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(r, 2));
                release(r);
                let line = format!(
                    "axes {:?} buttons {} switches {:?}",
                    a.iter().map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>(),
                    b.iter().map(|x| if *x { '1' } else { '.' }).collect::<String>(),
                    s
                );
                if last.get(&d) != Some(&line) {
                    println!("  {d:#x}: {line}");
                    last.insert(d, line);
                }
                // The typed gamepad reading, for devices GameInput classes as gamepads.
                let mut g: *mut c_void = std::ptr::null_mut();
                if get_reading(gi, 0x40000, d as *mut c_void, &mut g) >= 0 && !g.is_null() {
                    let state: extern "system" fn(*mut c_void, *mut [u8; 28]) -> bool = std::mem::transmute(slot(g, 22));
                    let mut st = [0u8; 28];
                    if state(g, &mut st) {
                        let f = |i: usize| f32::from_le_bytes([st[i], st[i + 1], st[i + 2], st[i + 3]]);
                        let pad = format!(
                            "PAD buttons {:04x} LT {:.2} RT {:.2} L ({:.2},{:.2}) R ({:.2},{:.2})",
                            u32::from_le_bytes([st[0], st[1], st[2], st[3]]), f(4), f(8), f(12), f(16), f(20), f(24)
                        );
                        let key = d + 1;
                        if last.get(&key) != Some(&pad) {
                            println!("  {d:#x}: {pad}");
                            last.insert(key, pad);
                        }
                    }
                    let release: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(g, 2));
                    release(g);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
    }
}

#[cfg(not(windows))]
fn main() {}
