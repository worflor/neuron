// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The system tray — the 90% surface. A resident `TrayIcon` whose menu reads as neuron's state:
//! a status line, then profile, lighting, brightness and DPI each labelled with what they are now,
//! HyperShift, the write gate, and the way out. The rows keep one order whatever is plugged in, so
//! the menu is learnable; a row with nothing to act on stays in place, disabled, saying why. The
//! global hotkeys are shown beside the rows they trigger. Events arrive on tray-icon's and
//! global-hotkey's static channels; we drain them from the Slint event loop via a `Timer`.
//!
//! `sync` rebuilds the menu whenever the state it shows changes. Hotkeys are registered once and
//! survive every rebuild (their ids live in a separate map).

#[cfg(any(windows, target_os = "linux"))]
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
#[cfg(any(windows, target_os = "linux"))]
use std::cell::{Cell, RefCell};
#[cfg(any(windows, target_os = "linux"))]
use std::sync::mpsc::{self, Receiver};
#[cfg(any(windows, target_os = "linux"))]
use std::collections::HashMap;
#[cfg(any(windows, target_os = "linux"))]
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
#[cfg(any(windows, target_os = "linux"))]
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
    SetBrightness(f32),
    SetDpi(f32),
    ReportBug,
    /// Download the offered release if needed, install it and restart.
    UpdateInstall,
    /// Open the window on the update's notes.
    UpdateNotes,
}

#[cfg(any(windows, target_os = "linux"))]
enum QueuedEvent {
    Menu(MenuEvent),
    Icon,
    Hotkey(GlobalHotKeyEvent),
}

#[cfg(any(windows, target_os = "linux"))]
thread_local! {
    static EVENT_WAKE: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
}

#[cfg(any(windows, target_os = "linux"))]
fn wake_event_loop() {
    let _ = slint::invoke_from_event_loop(|| {
        EVENT_WAKE.with(|slot| {
            if let Some(wake) = slot.borrow().as_ref() {
                wake();
            }
        });
    });
}

/// The tray menu state, compared before applying refreshes so unchanged values stay untouched.
#[derive(Clone, PartialEq, Default)]
pub struct TraySnapshot {
    pub profiles: Vec<String>,
    pub effects: Vec<String>,
    pub active: String,
    /// the effect the device is running, by index into `effects`
    pub effect: Option<usize>,
    pub paused: bool,
    pub armed: bool,
    pub hyper: bool,
    /// the selected device and its brightness (percent), when it has lighting
    pub light: Option<(String, i32)>,
    /// the selected device, its DPI and its stages, when it has DPI
    pub dpi: Option<(String, i32, Vec<i32>)>,
    /// the updater's phase, the version it concerns, and download progress in tenths
    pub update: Option<(String, String, u8)>,
}

/// The tray's update line for the updater's `phase`, and what clicking it does (`None`: a status
/// line, not a button).
pub(crate) fn update_line(phase: &str, version: &str, tenths: u8) -> Option<(String, Option<TrayAction>)> {
    Some(match phase {
        "available" | "ready" => (format!("Install v{version} and restart"), Some(TrayAction::UpdateInstall)),
        "downloading" => (format!("Downloading v{version} · {}%", u32::from(tenths) * 10), None),
        "installing" => (format!("Installing v{version}…"), None),
        "error" => (format!("Update to v{version} stopped · try again"), Some(TrayAction::UpdateInstall)),
        "updated" => (format!("Updated to v{version} · what's new"), Some(TrayAction::UpdateNotes)),
        "rolled-back" => (format!("v{version} rolled back · details"), Some(TrayAction::UpdateNotes)),
        _ => return None,
    })
}


/// Other platforms have no tray backend yet.
#[cfg(not(any(windows, target_os = "linux")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "linux")))]
impl Tray {
    pub fn build(
        _profiles: &[String],
        _effects: &[String],
        _active: &str,
        _paused: bool,
        _hyper: bool,
    ) -> anyhow::Result<Self> {
        Ok(Tray)
    }

    pub fn sync(&self, _snap: &TraySnapshot, _force: bool) {}
    pub fn install_wake(&self, _wake: impl Fn() + 'static) {}
    pub fn uninstall_wake(&self) {}

    pub fn poll(&self) -> Vec<TrayAction> {
        Vec::new()
    }
}

/// The resident tray. Holds the icon + the hotkey manager alive for the process lifetime.
#[cfg(any(windows, target_os = "linux"))]
pub struct Tray {
    icon: TrayIcon,
    _hotkeys: Option<GlobalHotKeyManager>,
    /// "hk:{id}" -> action — registered ONCE; never dropped by a menu rebuild.
    hotkey_map: HashMap<String, TrayAction>,
    /// menu-item id -> action — replaced wholesale on every rebuild.
    menu_map: RefCell<HashMap<String, TrayAction>>,
    /// what the current menu shows (the diff key for `sync`).
    snapshot: RefCell<TraySnapshot>,
    events: RefCell<Option<Receiver<QueuedEvent>>>,
    menu_clicked: Cell<bool>,
}

#[cfg(any(windows, target_os = "linux"))]
impl Tray {
    /// Build the tray with the given snapshot of profiles/effects/active/gates.
    pub fn build(
        profiles: &[String],
        effects: &[String],
        active: &str,
        paused: bool,
        hyper: bool,
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        gtk::init()?;

        let snap = TraySnapshot {
            profiles: profiles.to_vec(),
            effects: effects.to_vec(),
            active: active.to_string(),
            paused,
            hyper,
            ..TraySnapshot::default()
        };
        let (menu, map) = build_menu(&snap);

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Neuron: anti-Synapse")
            .with_icon(load_icon()?)
            .build()?;

        // global hotkeys are best-effort: register a couple of muscle-memory shortcuts.
        let mut hotkey_map = HashMap::new();
        let hotkeys = register_hotkeys(&mut hotkey_map);

        Ok(Tray {
            icon,
            _hotkeys: hotkeys,
            hotkey_map,
            menu_map: RefCell::new(map),
            snapshot: RefCell::new(snap),
            events: RefCell::new(None),
            menu_clicked: Cell::new(false),
        })
    }

    /// Forward tray and hotkey events to the UI loop as they arrive, so resident status polling
    /// can run slowly without delaying the controls people use from the tray.
    pub fn install_wake(&self, wake: impl Fn() + 'static) {
        EVENT_WAKE.with(|slot| *slot.borrow_mut() = Some(Box::new(wake)));
        let (tx, rx) = mpsc::sync_channel(64);
        *self.events.borrow_mut() = Some(rx);
        let menu_tx = tx.clone();
        MenuEvent::set_event_handler(Some(move |ev| {
            if menu_tx.try_send(QueuedEvent::Menu(ev)).is_ok() {
                wake_event_loop();
            }
        }));
        let icon_tx = tx.clone();
        TrayIconEvent::set_event_handler(Some(move |ev| {
            if matches!(ev, TrayIconEvent::Click { button: tray_icon::MouseButton::Left, .. })
                && icon_tx.try_send(QueuedEvent::Icon).is_ok()
            {
                wake_event_loop();
            }
        }));
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            if ev.state == global_hotkey::HotKeyState::Pressed
                && tx.try_send(QueuedEvent::Hotkey(ev)).is_ok()
            {
                wake_event_loop();
            }
        }));
    }

    /// Drop the installed wake closure. It can own runtime state whose Drop joins worker threads;
    /// left in the thread-local, that Drop runs during process teardown after Windows has already
    /// killed those threads, and the join panics.
    pub fn uninstall_wake(&self) {
        EVENT_WAKE.with(|slot| slot.borrow_mut().take());
    }

    /// Rebuild the menu if the live truth differs from what's shown (or `force`, which repairs
    /// muda's client-side check auto-toggle after a menu click that didn't change model state).
    /// Cheap when nothing changed: one snapshot compare, zero Win32 calls.
    pub fn sync(&self, snap: &TraySnapshot, force: bool) {
        let force = force | self.menu_clicked.replace(false);
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
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }

        let mut out = Vec::new();
        if let Some(rx) = self.events.borrow().as_ref() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    QueuedEvent::Menu(ev) => {
                        self.menu_clicked.set(true);
                        if let Some(a) = self.menu_map.borrow().get(&ev.id.0) {
                            out.push(a.clone());
                        }
                    }
                    QueuedEvent::Icon => out.push(TrayAction::Open),
                    QueuedEvent::Hotkey(ev) => {
                        if let Some(a) = self.hotkey_map.get(&format!("hk:{}", ev.id)) {
                            out.push(a.clone());
                        }
                    }
                }
            }
            return out;
        }
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

/// The menu's first line: what neuron is doing right now.
pub(crate) fn status_line(snap: &TraySnapshot) -> String {
    let state = if snap.paused {
        "device writes paused"
    } else if !snap.armed {
        "input off (safe mode)"
    } else {
        "live"
    };
    if snap.active.is_empty() { format!("Neuron · {state}") } else { format!("Neuron · {state} · {}", snap.active) }
}

/// Construct the menu + its id->action map from a snapshot (shared by build and sync).
#[cfg(any(windows, target_os = "linux"))]
fn build_menu(snap: &TraySnapshot) -> (Menu, HashMap<String, TrayAction>) {
    use tray_icon::menu::accelerator::Accelerator;
    let key = |s: &str| s.parse::<Accelerator>().ok();
    let mut map = HashMap::new();
    let menu = Menu::new();
    let mut item = |text: &str, enabled: bool, accel: Option<Accelerator>, act: Option<TrayAction>| {
        let it = MenuItem::new(text, enabled, accel);
        if let Some(a) = act { map.insert(it.id().0.clone(), a); }
        let _ = menu.append(&it);
    };
    item(&status_line(snap), false, None, None);
    if let Some((text, act)) = snap.update.as_ref().and_then(|(phase, version, tenths)| update_line(phase, version, *tenths)) {
        let on = act.is_some();
        item(&text, on, None, act);
    }
    item("Open Neuron", true, key("Ctrl+Alt+N"), Some(TrayAction::Open));
    let _ = menu.append(&PredefinedMenuItem::separator());

    let pick = |sub: &Submenu, map: &mut HashMap<String, TrayAction>, text: &str, on: bool, act: TrayAction| {
        let it = CheckMenuItem::new(text, true, on, None);
        map.insert(it.id().0.clone(), act);
        let _ = sub.append(&it);
    };
    let note = |sub: &Submenu, text: &str| { let _ = sub.append(&MenuItem::new(text, false, None)); };

    let label = if snap.active.is_empty() { "Profile".to_string() } else { format!("Profile: {}", snap.active) };
    let prof = Submenu::new(label, !snap.profiles.is_empty());
    for p in &snap.profiles {
        pick(&prof, &mut map, p, *p == snap.active, TrayAction::ApplyProfile(p.clone()));
    }
    let _ = menu.append(&prof);

    let running = snap.effect.and_then(|i| snap.effects.get(i));
    let label = match running {
        Some(e) => format!("Lighting: {e}"),
        None if snap.effects.is_empty() => "Lighting: no device".into(),
        None => "Lighting".into(),
    };
    let fx = Submenu::new(label, !snap.effects.is_empty());
    for (i, e) in snap.effects.iter().enumerate() {
        pick(&fx, &mut map, e, snap.effect == Some(i), TrayAction::SetEffect(e.clone()));
    }
    let _ = menu.append(&fx);

    match &snap.light {
        Some((device, now)) => {
            let sub = Submenu::new(format!("Brightness: {now}%"), true);
            note(&sub, device);
            for v in [100, 75, 50, 25, 0] {
                let text = if v == 0 { "Off".to_string() } else { format!("{v}%") };
                pick(&sub, &mut map, &text, *now == v, TrayAction::SetBrightness(v as f32));
            }
            let _ = menu.append(&sub);
        }
        None => { let _ = menu.append(&Submenu::new("Brightness: no lighting selected", false)); }
    }
    match &snap.dpi {
        Some((device, now, stages)) => {
            let sub = Submenu::new(format!("DPI: {now}"), true);
            note(&sub, device);
            for &v in stages {
                pick(&sub, &mut map, &v.to_string(), *now == v, TrayAction::SetDpi(v as f32));
            }
            let _ = menu.append(&sub);
        }
        None => { let _ = menu.append(&Submenu::new("DPI: no mouse selected", false)); }
    }

    let hs = CheckMenuItem::new("HyperShift", true, snap.hyper, key("Ctrl+Alt+H"));
    map.insert(hs.id().0.clone(), TrayAction::ToggleHyperShift);
    let _ = menu.append(&hs);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let pause = CheckMenuItem::new("Pause device writes", true, snap.paused, key("Ctrl+Alt+P"));
    map.insert(pause.id().0.clone(), TrayAction::ToggleWritesPaused);
    let _ = menu.append(&pause);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let mut item = |text: &str, act: TrayAction| {
        let it = MenuItem::new(text, true, None);
        map.insert(it.id().0.clone(), act);
        let _ = menu.append(&it);
    };
    item("Report a bug…", TrayAction::ReportBug);
    item("Settings", TrayAction::GotoSettings);
    item("Quit Neuron", TrayAction::Quit);

    (menu, map)
}

/// Register OS-wide hotkeys (best-effort; failures are non-fatal — the tray menu still works).
/// Ctrl+Alt+H toggles `HyperShift`, Ctrl+Alt+P pauses writes, Ctrl+Alt+N opens the window.
#[cfg(any(windows, target_os = "linux"))]
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

/// The tray icon: neuron's mark (see `mark.rs`).
#[cfg(any(windows, target_os = "linux"))]
fn load_icon() -> anyhow::Result<tray_icon::Icon> {
    Ok(tray_icon::Icon::from_rgba(crate::mark::rgba(32), 32, 32)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_says_what_neuron_is_doing() {
        let snap = |paused, armed| TraySnapshot { paused, armed, active: "work".into(), ..TraySnapshot::default() };
        assert_eq!(status_line(&snap(false, true)), "Neuron · live · work");
        assert_eq!(status_line(&snap(true, true)), "Neuron · device writes paused · work");
        assert_eq!(status_line(&snap(false, false)), "Neuron · input off (safe mode) · work");
    }

    #[test]
    fn the_update_line_offers_the_next_step_or_reports_progress() {
        assert!(matches!(update_line("ready", "0.1.8", 10), Some((t, Some(TrayAction::UpdateInstall))) if t == "Install v0.1.8 and restart"));
        assert!(matches!(update_line("downloading", "0.1.8", 4), Some((t, None)) if t == "Downloading v0.1.8 · 40%"));
        assert!(matches!(update_line("updated", "0.1.8", 0), Some((_, Some(TrayAction::UpdateNotes)))));
        assert!(update_line("", "0.1.8", 0).is_none());
    }
}
