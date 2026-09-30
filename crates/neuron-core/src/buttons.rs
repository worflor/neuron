// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Firmware button functions, and the planner that decides who performs a bind.
//!
//! A device that can reassign a button in firmware (razer `0x02/0x0C`, read back at `0x02/0x8C`)
//! performs a bind itself whenever it can express the action: the button emits the target key at
//! the source, with no hook and nothing for the host to do. A bind the firmware can't express (a
//! macro, an app action, anything on a HyperShift layer) is handed to the host on a PRIVATE key:
//! the button is reassigned to a key no other device sends (F13..F24), which the interceptor can
//! swallow without guessing which keyboard it came from, and the ingress translates back to the
//! button's stock identity so the bind matches unchanged.
//!
//! A bind is always stored against the button's STOCK usage (what it emits from the factory), so
//! the engine, the rule files and capture never see a remapped key. Capture restores the stock
//! table while it listens.
//!
//! Writes go to the volatile direct profile (0): nothing touches onboard flash, and the device
//! forgets them on replug or power loss, so the app re-applies on connect and wake.

use crate::action::Action;
use crate::device::Device;
use crate::engine::{Rule, Trigger};
use crate::registry::{ButtonSpec, CanonicalPid, DeviceDef};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const CLASS_BUTTONS: u8 = 0x02;
const ID_BUTTON_FN_SET: u8 = 0x0C;
const ID_BUTTON_FN_GET: u8 = 0x8C;
const BUTTON_FN_SIZE: u8 = 0x0A;
/// The volatile "direct" profile; 1..5 are onboard slots.
const DIRECT_PROFILE: u8 = 0x00;
const CATEGORY_KEYBOARD: u8 = 0x02;

/// Keys handed out one per button for host-performed binds: F13..F24, allocated from F24 down
/// because macro pads and hotkey tools favour F13 upward. Unique across every device at once.
const PRIVATE_POOL: std::ops::RangeInclusive<u8> = 0x68..=0x73;

/// A button's function record: `[category, len, data x5]`, the tail of the `02/0C` payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record(pub [u8; 7]);

impl Record {
    /// The factory form of a keyboard button, as the firmware reports it (`len` 1, `[00, usage]`).
    #[must_use]
    pub fn stock(usage: u8) -> Self {
        Record([CATEGORY_KEYBOARD, 0x01, 0x00, usage, 0, 0, 0])
    }

    /// A keyboard function: `mods` is the HID modifier bitmask (bit 0 = left ctrl … bit 7 = right
    /// GUI), `usage` the key.
    #[must_use]
    pub fn keyboard(mods: u8, usage: u8) -> Self {
        Record([CATEGORY_KEYBOARD, 0x02, mods, usage, 0, 0, 0])
    }

    /// `(mods, usage)` when this is a keyboard function, in either the factory or the written form.
    #[must_use]
    pub fn as_keyboard(&self) -> Option<(u8, u8)> {
        (self.0[0] == CATEGORY_KEYBOARD).then_some((self.0[2], self.0[3]))
    }
}

/// Read one button's function on the direct profile.
pub fn read(d: &Device, button: u8) -> Result<Record> {
    let a = d.exec_dynamic(CLASS_BUTTONS, ID_BUTTON_FN_GET, BUTTON_FN_SIZE, &[DIRECT_PROFILE, button, 0x00])?;
    if a[0] != DIRECT_PROFILE || a[1] != button || a[2] != 0x00 {
        bail!("button {button:#04x} read-back echoed {:02x?}, not the button asked for", &a[..3]);
    }
    let mut r = [0u8; 7];
    r.copy_from_slice(&a[3..10]);
    Ok(Record(r))
}

/// Write one button's function on the direct profile and verify it against the `02/8C` read-back.
pub fn write(d: &Device, button: u8, rec: Record) -> Result<()> {
    if crate::writes::writes_paused() {
        bail!("writes paused");
    }
    let mut args = [0u8; 10];
    args[..3].copy_from_slice(&[DIRECT_PROFILE, button, 0x00]);
    args[3..].copy_from_slice(&rec.0);
    d.exec_dynamic(CLASS_BUTTONS, ID_BUTTON_FN_SET, BUTTON_FN_SIZE, &args)?;
    let got = read(d, button)?;
    if got != rec {
        bail!("VERIFY FAILED on button {button:#04x}: wrote {:02x?}, device reports {:02x?}", rec.0, got.0);
    }
    Ok(())
}

/// Who performs a button's bind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Unmanaged: the factory function, and any bind on it runs the host's own way.
    Stock,
    /// The firmware emits this keyboard function itself; the host must not also act on it.
    Performed { mods: u8, usage: u8 },
    /// The button emits this private key; the host swallows it and runs the bind.
    Private { usage: u8 },
}

/// One button's planned function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ButtonPlan {
    pub id: u8,
    pub stock_usage: u8,
    pub role: Role,
}

impl ButtonPlan {
    #[must_use]
    pub fn record(&self) -> Record {
        match self.role {
            Role::Stock => Record::stock(self.stock_usage),
            Role::Performed { mods, usage } => Record::keyboard(mods, usage),
            Role::Private { usage } => Record::keyboard(0, usage),
        }
    }

    /// The keyboard usage this button emits under the plan.
    #[must_use]
    pub fn emits(&self) -> u8 {
        match self.role {
            Role::Stock => self.stock_usage,
            Role::Performed { usage, .. } | Role::Private { usage } => usage,
        }
    }
}

/// `"ctrl+shift+s"` → `(mods, usage)`, when one keyboard record can carry it.
#[must_use]
pub fn keyboard_function(key: &str) -> Option<(u8, u8)> {
    let mut parts: Vec<&str> = key.split('+').map(str::trim).collect();
    let main = crate::action::hid_usage_for_key(parts.pop()?)?;
    let mut mods = 0u8;
    for m in parts {
        match crate::action::hid_usage_for_key(m)? {
            u @ 0xE0..=0xE7 => mods |= 1 << (u - 0xE0),
            _ => return None, // two ordinary keys: not one keyboard record
        }
    }
    Some((mods, main))
}

/// Decide each of `def`'s buttons' role from the live rule set. `held` is a control a host feature
/// owns as a HELD trigger (the cast trigger); such a button always goes to the host. `taken` holds
/// the private keys other devices already use; this device's are added to it. A button that finds
/// the pool empty stays stock and its bind is served by the input interceptor instead.
#[must_use]
pub fn plan(def: &DeviceDef, rules: &[Rule], held: Option<crate::controls::ControlRef>, taken: &mut std::collections::BTreeSet<u8>) -> Vec<ButtonPlan> {
    let Some(first) = def.modes.first() else { return Vec::new() };
    let pid = CanonicalPid::of(first.product_id);
    let mut next_private = || {
        let usage = PRIVATE_POOL.rev().find(|u| !taken.contains(u))?;
        taken.insert(usage);
        Some(usage)
    };
    def.buttons
        .iter()
        .map(|&ButtonSpec { id, stock_usage }| {
            let on_button = |page: u16, usage: u16, p: Option<CanonicalPid>| {
                page == 0x07 && usage == u16::from(stock_usage) && p == Some(pid)
            };
            let binds: Vec<&Rule> = rules
                .iter()
                .filter(|r| matches!(r.trigger, Trigger::Input { page, usage, pid: p } if on_button(page, usage, p)))
                .collect();
            let is_held = held.is_some_and(|c| on_button(c.page, c.usage, c.pid));
            let role = if binds.is_empty() && !is_held {
                Role::Stock
            } else {
                let firmware = match binds.as_slice() {
                    [only] if !is_held && only.layer.as_deref().is_none_or(|l| l.starts_with("plate:")) => match &only.action {
                        Action::Key { key } => keyboard_function(key),
                        _ => None,
                    },
                    _ => None,
                };
                match firmware {
                    Some((mods, usage)) => Role::Performed { mods, usage },
                    None => next_private().map_or(Role::Stock, |usage| Role::Private { usage }),
                }
            };
            ButtonPlan { id, stock_usage, role }
        })
        .collect()
}

/// What each device's firmware holds, as written and verified by [`apply`], and the input-path
/// questions asked of it. The process has one ([`APPLIED`]); it's a type so it can be tested alone.
#[derive(Default, Debug)]
pub struct Table(BTreeMap<CanonicalPid, Vec<ButtonPlan>>);

impl Table {
    fn managed(&self, pid: CanonicalPid) -> impl Iterator<Item = &ButtonPlan> {
        self.0.get(&pid).into_iter().flatten().filter(|p| p.role != Role::Stock)
    }

    /// Emitted `usage` → the stock usage of the button that emitted it. `drift` is set when a
    /// managed button emitted its own stock key: the device lost its functions, so its table is
    /// dropped (the host performs its binds again) until re-applied.
    pub fn translate(&mut self, pid: CanonicalPid, usage: u16) -> (u16, bool) {
        if let Some(p) = self.managed(pid).find(|p| u16::from(p.emits()) == usage) {
            return (u16::from(p.stock_usage), false);
        }
        let drift = self.managed(pid).any(|p| u16::from(p.stock_usage) == usage);
        if drift {
            self.0.remove(&pid);
        }
        (usage, drift)
    }

    /// Does the firmware perform the bind on stock key `usage`?
    #[must_use]
    pub fn performs(&self, pid: CanonicalPid, usage: u16) -> bool {
        self.managed(pid).any(|p| matches!(p.role, Role::Performed { .. }) && u16::from(p.stock_usage) == usage)
    }

    /// Is stock key `usage` managed at all (performed or private)?
    #[must_use]
    pub fn covers(&self, pid: CanonicalPid, usage: u16) -> bool {
        self.managed(pid).any(|p| u16::from(p.stock_usage) == usage)
    }

    /// The device and stock usage private key `usage` stands for.
    #[must_use]
    pub fn private_source(&self, usage: u16) -> Option<(CanonicalPid, u8)> {
        self.0.iter().find_map(|(pid, plan)| {
            plan.iter().find_map(|p| match p.role {
                Role::Private { usage: u } if u16::from(u) == usage => Some((*pid, p.stock_usage)),
                _ => None,
            })
        })
    }

    /// Every assigned private key, `(pid, usage)`.
    #[must_use]
    pub fn private_keys(&self) -> Vec<(CanonicalPid, u8)> {
        self.0
            .iter()
            .flat_map(|(pid, plan)| {
                plan.iter().filter_map(move |p| match p.role {
                    Role::Private { usage } => Some((*pid, usage)),
                    _ => None,
                })
            })
            .collect()
    }
}

static APPLIED: Mutex<Table> = Mutex::new(Table(BTreeMap::new()));

fn applied() -> std::sync::MutexGuard<'static, Table> {
    APPLIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set when the applied table changes, so the live loop re-arms the interceptor's private claims.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// Set when a managed button was seen emitting its stock key: the device dropped its volatile
/// functions (idle revert, reconnect) and needs re-applying.
static DRIFTED: AtomicBool = AtomicBool::new(false);

/// Bring the device's firmware to `plan`, writing only buttons that differ. Every write is
/// verified; a button that fails stays out of the applied table (the host keeps handling its bind
/// the stock way). Returns how many buttons now run a non-stock function.
pub fn apply(d: &Device, plan: &[ButtonPlan]) -> Result<usize> {
    let pid = CanonicalPid::of(d.def.modes.first().map_or(d.pid, |m| m.product_id));
    let mut live = Vec::with_capacity(plan.len());
    let mut first_err = None;
    for p in plan {
        let want = p.record();
        let ok = match read(d, p.id) {
            Ok(have) if have == want || (p.role == Role::Stock && have.as_keyboard() == Some((0, p.stock_usage))) => Ok(()),
            _ => write(d, p.id, want),
        };
        match ok {
            Ok(()) => live.push(*p),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    let managed = live.iter().filter(|p| p.role != Role::Stock).count();
    {
        let mut t = applied();
        if t.0.get(&pid) != Some(&live) {
            CHANGED.store(true, Ordering::Release);
        }
        t.0.insert(pid, live);
    }
    match first_err {
        Some(e) if managed == 0 && plan.iter().any(|p| p.role != Role::Stock) => Err(e),
        _ => Ok(managed),
    }
}

/// Return every button of `d` to its factory function and forget the device's applied table.
pub fn restore_stock(d: &Device) -> Result<()> {
    let stock: Vec<ButtonPlan> = d
        .def
        .buttons
        .iter()
        .map(|b| ButtonPlan { id: b.id, stock_usage: b.stock_usage, role: Role::Stock })
        .collect();
    apply(d, &stock).map(|_| ())
}

/// Forget a device's applied table without touching it (it was unplugged; its volatile functions
/// are already gone).
pub fn forget(pid: u16) {
    if applied().0.remove(&CanonicalPid::of(pid)).is_some() {
        CHANGED.store(true, Ordering::Release);
    }
}

/// Did the applied table change since the last call?
pub fn take_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}

/// Did a device drop its functions since the last call?
pub fn take_drift() -> bool {
    DRIFTED.swap(false, Ordering::AcqRel)
}

/// Ingress translation: a keyboard usage `raw_pid` emitted → the stock usage of the button that
/// emitted it, so binds and capture see the physical button whatever its firmware function. See
/// [`Table::translate`] for drift.
#[must_use]
pub fn translate(raw_pid: u16, usage: u16) -> u16 {
    let (usage, drift) = applied().translate(CanonicalPid::of(raw_pid), usage);
    if drift {
        CHANGED.store(true, Ordering::Release);
        DRIFTED.store(true, Ordering::Release);
    }
    usage
}

/// Is the bind on stock key `(page, usage)` of `raw_pid` performed by the firmware? The dispatcher
/// skips these edges: the device already emitted the result.
#[must_use]
pub fn performs(page: u16, usage: u16, raw_pid: u16) -> bool {
    page == 0x07 && applied().performs(CanonicalPid::of(raw_pid), usage)
}

/// Does the firmware plan handle stock key `usage` of `pid`, so the interceptor must leave it be?
#[must_use]
pub fn covers(pid: CanonicalPid, usage: u16) -> bool {
    applied().covers(pid, usage)
}

/// Is `usage` from `raw_pid` one of its private keys? The raw-input ingress drops these: the hook
/// already swallowed the key and delivered the button's edge ([`deliver_private`]).
#[must_use]
pub fn is_private(raw_pid: u16, usage: u16) -> bool {
    let pid = CanonicalPid::of(raw_pid);
    applied().private_source(usage).is_some_and(|(p, _)| p == pid)
}

/// Stock buttons currently held through their private key, per device.
static PRIVATE_HELD: Mutex<BTreeMap<CanonicalPid, Vec<(u16, u16)>>> = Mutex::new(BTreeMap::new());

/// The keyboard hook saw private key `usage` go down or up: deliver the button's stock edge to the
/// engine on the device's deferred stream, and tell the hook to swallow it. The hook can resolve
/// the device without Raw Input because only that device emits the key; it has to, because a
/// swallowed keystroke never reaches Raw Input. `false` when `usage` isn't a private key.
pub fn deliver_private(usage: u16, down: bool) -> bool {
    let Some((pid, stock)) = applied().private_source(usage) else { return false };
    let key = (0x07, u16::from(stock));
    let hits = {
        let mut held = PRIVATE_HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = held.entry(pid).or_default();
        if down {
            if set.contains(&key) {
                return true; // auto-repeat: already down, still ours to swallow
            }
            set.push(key);
        } else {
            set.retain(|k| *k != key);
        }
        set.clone()
    };
    crate::controls::inject_event(crate::controls::ControlEvent {
        pid: Some(pid),
        stream: crate::controls::Stream::Private,
        hits,
        raw: Vec::new(),
    });
    true
}

/// The private keys currently assigned, per device: `(pid, private usage)` for the interceptor to
/// swallow. Only that device can emit them, so the claim never touches another keyboard.
#[must_use]
pub fn private_keys() -> Vec<(CanonicalPid, u8)> {
    applied().private_keys()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naga() -> DeviceDef {
        toml::from_str(include_str!("../devices/razer-naga-v2-pro.toml")).unwrap()
    }

    fn bind(usage: u16, action: Action, layer: Option<&str>) -> Rule {
        let mut r = Rule::new(
            Trigger::Input { page: 0x07, usage, pid: Some(CanonicalPid::of(0x00A7)) },
            action,
        );
        r.layer = layer.map(str::to_string);
        r
    }

    fn key(k: &str) -> Action {
        Action::Key { key: k.into() }
    }

    #[test]
    fn records_match_the_live_wire_bytes() {
        // 2026-09-29, Naga V2 Pro: stock '=' read back as [02 01 00 2e], the 'g' write as [02 02 00 0a].
        assert_eq!(Record::stock(0x2E).0, [0x02, 0x01, 0x00, 0x2E, 0, 0, 0]);
        assert_eq!(Record::keyboard(0, 0x0A).0, [0x02, 0x02, 0x00, 0x0A, 0, 0, 0]);
        assert_eq!(Record::stock(0x2E).as_keyboard(), Some((0, 0x2E)));
    }

    #[test]
    fn chords_fold_into_one_keyboard_record() {
        assert_eq!(keyboard_function("g"), Some((0, 0x0A)));
        assert_eq!(keyboard_function("ctrl+s"), Some((0x01, 0x16)));
        assert_eq!(keyboard_function("ctrl+shift+s"), Some((0x03, 0x16)));
        assert_eq!(keyboard_function("a+b"), None, "two ordinary keys are not one record");
        assert_eq!(keyboard_function("media-play-pause"), None);
    }

    #[test]
    fn the_firmware_performs_what_it_can_and_hands_the_rest_to_the_host() {
        let def = naga();
        let rules = vec![
            bind(0x2E, key("g"), None),                                     // '=' → g
            bind(0x1E, key("ctrl+s"), None),                                // '1' → ctrl+s
            bind(0x1F, Action::Run { cmd: "calc".into() }, None),           // '2' → host action
            bind(0x20, key("h"), Some("hypershift")),                       // '3' layered → host
            bind(0x21, key("j"), Some("plate:12-button")),                  // '4' plate layer = base
        ];
        let plan = plan(&def, &rules, None, &mut Default::default());
        let role = |stock: u8| plan.iter().find(|p| p.stock_usage == stock).unwrap().role;
        assert_eq!(role(0x2E), Role::Performed { mods: 0, usage: 0x0A });
        assert_eq!(role(0x1E), Role::Performed { mods: 0x01, usage: 0x16 });
        assert!(matches!(role(0x1F), Role::Private { .. }));
        assert!(matches!(role(0x20), Role::Private { .. }));
        assert_eq!(role(0x21), Role::Performed { mods: 0, usage: 0x0D });
        assert_eq!(role(0x22), Role::Stock, "unbound buttons stay factory");
        let privates: Vec<u8> = plan.iter().filter_map(|p| match p.role { Role::Private { usage } => Some(usage), _ => None }).collect();
        assert!(privates.iter().all(|u| PRIVATE_POOL.contains(u)));
        assert_ne!(privates[0], privates[1], "each host-performed button gets its own private key");
    }

    #[test]
    fn two_devices_never_share_a_private_key_and_f13_is_the_last_handed_out() {
        let def = naga();
        let rules = vec![bind(0x1F, Action::Run { cmd: "calc".into() }, None), bind(0x20, Action::Run { cmd: "x".into() }, None)];
        let mut taken = std::collections::BTreeSet::new();
        let privates = |plan: &[ButtonPlan]| -> Vec<u8> {
            plan.iter().filter_map(|p| match p.role { Role::Private { usage } => Some(usage), _ => None }).collect()
        };
        let first = privates(&plan(&def, &rules, None, &mut taken));
        let second = privates(&plan(&def, &rules, None, &mut taken));
        assert_eq!(first, vec![0x73, 0x72], "F24 first");
        assert!(second.iter().all(|u| !first.contains(u)), "{first:?} vs {second:?}");
    }

    #[test]
    fn a_device_any_bind_is_not_a_button_bind() {
        let mut r = bind(0x2E, key("g"), None);
        r.trigger = Trigger::Input { page: 0x07, usage: 0x2E, pid: None };
        assert!(plan(&naga(), &[r], None, &mut Default::default()).iter().all(|p| p.role == Role::Stock));
    }

    #[test]
    fn the_input_path_follows_what_the_firmware_holds() {
        let pid = CanonicalPid::of(0x00A7);
        let plan = plan(
            &naga(),
            &[bind(0x2E, key("g"), None), bind(0x2D, Action::Run { cmd: "calc".into() }, None)],
            None,
            &mut Default::default(),
        );
        let private = match plan.iter().find(|p| p.stock_usage == 0x2D).unwrap().role {
            Role::Private { usage } => usage,
            other => panic!("expected a private key, got {other:?}"),
        };
        let mut t = Table::default();
        t.0.insert(pid, plan);

        // Firmware-performed: the emitted 'g' is the '=' button; the engine must not act on it.
        assert_eq!(t.translate(pid, 0x0A), (0x2E, false));
        assert!(t.performs(pid, 0x2E));
        assert!(t.covers(pid, 0x2E) && t.covers(pid, 0x2D) && !t.covers(pid, 0x1E));
        // Host-performed: the private key names its device and stock button by itself.
        assert_eq!(t.private_source(u16::from(private)), Some((pid, 0x2D)));
        assert_eq!(t.private_source(0x0A), None);
        assert_eq!(t.private_keys(), vec![(pid, private)]);
        // Unmanaged keys pass untouched.
        assert_eq!(t.translate(pid, 0x1E), (0x1E, false));

        // The '=' button emitting its stock key means the firmware lost its function.
        assert_eq!(t.translate(pid, 0x2E), (0x2E, true));
        assert!(!t.performs(pid, 0x2E), "the host performs its binds again until re-applied");
        assert!(t.private_keys().is_empty());
    }

    #[test]
    fn a_held_trigger_always_goes_to_the_host() {
        let held = crate::controls::ControlRef { page: 0x07, usage: 0x2E, pid: Some(CanonicalPid::of(0x00A7)) };
        let p = plan(&naga(), &[bind(0x2E, key("g"), None)], Some(held), &mut Default::default());
        assert!(matches!(p.iter().find(|p| p.stock_usage == 0x2E).unwrap().role, Role::Private { .. }));
    }
}
