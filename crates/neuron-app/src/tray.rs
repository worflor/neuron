// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The system tray — the 90% surface. A resident `TrayIcon` with a menu (active profile +
//! quick-switch, HyperShift toggle, brightness/DPI quick controls, effects quick-pick, Open,
//! Settings sub-toggles, Quit). Events arrive on tray-icon's + global-hotkey's static channels;
//! we drain them from the Slint event loop via a `Timer` (the loop stays alive with no window).
//!
//! The menu is NOT a boot-time snapshot: `sync` rebuilds it whenever the live truth (profiles /
//! effects / active / paused / hypershift) changes, so the quick-switch list, the active-profile
//! checkmark, and the Pause-writes check track reality. Hotkeys are registered once and survive
//! every menu rebuild (their ids live in a separate map).

#[cfg(windows)]
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
#[cfg(windows)]
use std::cell::RefCell;
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(windows)]
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

/// A decoded action the tray/hotkey raised, handed to the glue to execute on the UI thread.
#[derive(Clone, Debug)]
pub enum TrayAction {
    Open,
    Quit,
    GotoSettings,
    ToggleHyperShift,
    ToggleWritesPaused,
    ApplyProfile(String),
    SetEffect(String),
    BrightnessUp,
    BrightnessDown,
    DpiUp,
    DpiDown,
}

/// The truth the menu renders — kept to diff against so an idle 60ms tick costs one Vec compare.
#[derive(Clone, PartialEq, Default)]
pub struct TraySnapshot {
    pub profiles: Vec<String>,
    pub effects: Vec<String>,
    pub active: String,
    pub paused: bool,
    pub hyper: bool,
}


/// No tray on this platform yet: `tray-icon` and `global-hotkey` need GTK and X11 on Linux, and
/// neuron has no working Linux GUI to hang them off. The app still builds and runs headless —
/// `poll` simply never yields an action and `sync` has nothing to redraw.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn build(
        _profiles: &[String],
        _effects: &[String],
        _active: &str,
        _paused: bool,
        _hyper: bool,
    ) -> Self {
        Tray
    }

    pub fn sync(&self, _snap: &TraySnapshot, _force: bool) {}

    pub fn poll(&self) -> Vec<TrayAction> {
        Vec::new()
    }
}

/// The resident tray. Holds the icon + the hotkey manager alive for the process lifetime.
#[cfg(windows)]
pub struct Tray {
    icon: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    /// "hk:{id}" -> action — registered ONCE; never dropped by a menu rebuild.
    hotkey_map: HashMap<String, TrayAction>,
    /// menu-item id -> action — replaced wholesale on every rebuild.
    menu_map: RefCell<HashMap<String, TrayAction>>,
    /// what the current menu shows (the diff key for `sync`).
    snapshot: RefCell<TraySnapshot>,
}

#[cfg(windows)]
impl Tray {
    /// Build the tray with the given snapshot of profiles/effects/active/gates.
    pub fn build(
        profiles: &[String],
        effects: &[String],
        active: &str,
        paused: bool,
        hyper: bool,
    ) -> Self {
        let snap = TraySnapshot {
            profiles: profiles.to_vec(),
            effects: effects.to_vec(),
            active: active.to_string(),
            paused,
            hyper,
        };
        let (menu, map) = build_menu(&snap);

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Neuron: anti-Synapse")
            .with_icon(load_icon())
            .build()
            .expect("failed to build tray icon");

        // global hotkeys are best-effort: register a couple of muscle-memory shortcuts.
        let mut hotkey_map = HashMap::new();
        let hotkeys = register_hotkeys(&mut hotkey_map);

        Tray {
            icon,
            _hotkeys: hotkeys,
            hotkey_map,
            menu_map: RefCell::new(map),
            snapshot: RefCell::new(snap),
        }
    }

    /// Rebuild the menu if the live truth differs from what's shown (or `force`, which repairs
    /// muda's client-side check auto-toggle after a menu click that didn't change model state).
    /// Cheap when nothing changed: one snapshot compare, zero Win32 calls.
    pub fn sync(&self, snap: &TraySnapshot, force: bool) {
        if !force && *self.snapshot.borrow() == *snap {
            return;
        }
        let (menu, map) = build_menu(snap);
        self.icon.set_menu(Some(Box::new(menu)));
        *self.menu_map.borrow_mut() = map;
        *self.snapshot.borrow_mut() = snap.clone();
    }

    /// Drain pending tray menu + hotkey events, mapping them to `TrayAction`s.
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut out = Vec::new();
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(a) = self.menu_map.borrow().get(&ev.id.0) {
                out.push(a.clone());
            }
        }
        // left-click the tray icon -> open
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                ..
            } = ev
            {
                out.push(TrayAction::Open);
            }
        }
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state == global_hotkey::HotKeyState::Pressed {
                if let Some(a) = self.hotkey_map.get(&format!("hk:{}", ev.id)) {
                    out.push(a.clone());
                }
            }
        }
        out
    }
}

/// Construct the menu + its id->action map from a snapshot (shared by build and sync).
#[cfg(windows)]
fn build_menu(snap: &TraySnapshot) -> (Menu, HashMap<String, TrayAction>) {
    let mut map = HashMap::new();
    let menu = Menu::new();

    let open = MenuItem::new("Open Neuron", true, None);
    map.insert(open.id().0.clone(), TrayAction::Open);
    let _ = menu.append(&open);
    let _ = menu.append(&PredefinedMenuItem::separator());

    // profile quick-switch submenu
    let prof_sub = Submenu::new("Profile", true);
    for p in &snap.profiles {
        let it = CheckMenuItem::new(p, true, *p == snap.active, None);
        map.insert(it.id().0.clone(), TrayAction::ApplyProfile(p.clone()));
        let _ = prof_sub.append(&it);
    }
    if snap.profiles.is_empty() {
        let _ = prof_sub.append(&MenuItem::new("(none saved)", false, None));
    }
    let _ = menu.append(&prof_sub);

    // effects quick-pick submenu
    let fx_sub = Submenu::new("Effect", true);
    for e in &snap.effects {
        let it = MenuItem::new(e, true, None);
        map.insert(it.id().0.clone(), TrayAction::SetEffect(e.clone()));
        let _ = fx_sub.append(&it);
    }
    if snap.effects.is_empty() {
        let _ = fx_sub.append(&MenuItem::new("(no device)", false, None));
    }
    let _ = menu.append(&fx_sub);

    // HyperShift toggle
    let hs = CheckMenuItem::new("HyperShift", true, snap.hyper, None);
    map.insert(hs.id().0.clone(), TrayAction::ToggleHyperShift);
    let _ = menu.append(&hs);
    let _ = menu.append(&PredefinedMenuItem::separator());

    // brightness + DPI quick controls
    let b_up = MenuItem::new("Brightness +", true, None);
    let b_dn = MenuItem::new("Brightness −", true, None);
    let d_up = MenuItem::new("DPI +", true, None);
    let d_dn = MenuItem::new("DPI −", true, None);
    map.insert(b_up.id().0.clone(), TrayAction::BrightnessUp);
    map.insert(b_dn.id().0.clone(), TrayAction::BrightnessDown);
    map.insert(d_up.id().0.clone(), TrayAction::DpiUp);
    map.insert(d_dn.id().0.clone(), TrayAction::DpiDown);
    let _ = menu.append(&b_up);
    let _ = menu.append(&b_dn);
    let _ = menu.append(&d_up);
    let _ = menu.append(&d_dn);
    let _ = menu.append(&PredefinedMenuItem::separator());

    // settings (pause-writes), open settings page, quit
    let pause = CheckMenuItem::new("Pause writes", true, snap.paused, None);
    map.insert(pause.id().0.clone(), TrayAction::ToggleWritesPaused);
    let _ = menu.append(&pause);
    let settings = MenuItem::new("Settings…", true, None);
    map.insert(settings.id().0.clone(), TrayAction::GotoSettings);
    let _ = menu.append(&settings);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let quit = MenuItem::new("Quit", true, None);
    map.insert(quit.id().0.clone(), TrayAction::Quit);
    let _ = menu.append(&quit);

    (menu, map)
}

/// Register OS-wide hotkeys (best-effort; failures are non-fatal — the tray menu still works).
/// Ctrl+Alt+H toggles HyperShift, Ctrl+Alt+P pauses writes, Ctrl+Alt+N opens the window.
#[cfg(windows)]
fn register_hotkeys(map: &mut HashMap<String, TrayAction>) -> Option<GlobalHotKeyManager> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    let mgr = GlobalHotKeyManager::new().ok()?;
    let mods = Modifiers::CONTROL | Modifiers::ALT;
    let binds = [
        (
            HotKey::new(Some(mods), Code::KeyH),
            TrayAction::ToggleHyperShift,
        ),
        (
            HotKey::new(Some(mods), Code::KeyP),
            TrayAction::ToggleWritesPaused,
        ),
        (HotKey::new(Some(mods), Code::KeyN), TrayAction::Open),
    ];
    for (hk, action) in binds {
        if mgr.register(hk).is_ok() {
            map.insert(format!("hk:{}", hk.id()), action);
        }
    }
    Some(mgr)
}

/// The tray icon bitmap — a small generated neuron mark (no asset file needed). A 32×32 RGBA: a true
/// near-black tile carrying a single phosphor diamond, matching the instrument accent (#4af2b0).
#[cfg(windows)]
fn load_icon() -> tray_icon::Icon {
    const W: u32 = 32;
    let mut rgba = vec![0u8; (W * W * 4) as usize];
    let cx = (W as f32 - 1.0) / 2.0;
    for y in 0..W {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            // rounded-square void ground
            let inside = (2..W - 2).contains(&x) && (2..W - 2).contains(&y);
            // diamond distance (Manhattan) from center -> the phosphor accent mark
            let d = (x as f32 - cx).abs() + (y as f32 - cx).abs();
            if d < 9.0 {
                rgba[i] = 0x4a;
                rgba[i + 1] = 0xf2;
                rgba[i + 2] = 0xb0;
                rgba[i + 3] = 0xff;
            } else if inside {
                rgba[i] = 0x07;
                rgba[i + 1] = 0x08;
                rgba[i + 2] = 0x09;
                rgba[i + 3] = 0xff;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, W, W).expect("icon")
}
