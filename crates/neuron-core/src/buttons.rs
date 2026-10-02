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
//! Separate plate banks sharing a stock control receive the same function; plate-scoped rules
//! select that function, while base binds work before the first plate announcement.
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
const ID_BUTTON_TABLE: u8 = 0x84;
const BUTTON_FN_SIZE: u8 = 0x0A;
/// The volatile "direct" profile; 1..5 are onboard slots.
const DIRECT_PROFILE: u8 = 0x00;
const CATEGORY_KEYBOARD: u8 = 0x02;

/// Keys handed out one per stock control for host-performed binds: F13..F24, allocated from F24 down
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
    read_in(d, DIRECT_PROFILE, button, false)
}

/// Read one button's function from `profile` (0 direct, 1..5 onboard), on the HyperShift layer or not.
pub fn read_in(d: &Device, profile: u8, button: u8, hypershift: bool) -> Result<Record> {
    let shift = u8::from(hypershift);
    let a = d.exec_dynamic(CLASS_BUTTONS, ID_BUTTON_FN_GET, BUTTON_FN_SIZE, &[profile, button, shift])?;
    if a[0] != profile || a[1] != button || a[2] != shift {
        bail!("button {button:#04x} read-back echoed {:02x?}, not the button asked for", &a[..3]);
    }
    let mut r = [0u8; 7];
    r.copy_from_slice(&a[3..10]);
    Ok(Record(r))
}

/// Every physical button id the firmware lists (`02/84`: `[count, id ...]`).
pub fn table(d: &Device) -> Result<Vec<u8>> {
    let a = d.exec_dynamic(CLASS_BUTTONS, ID_BUTTON_TABLE, 0x20, &[])?;
    let n = usize::from(a[0]).min(a.len() - 1);
    Ok(a[1..=n].to_vec())
}

/// Write one button's function on the direct profile and verify it against the `02/8C` read-back.
pub fn write(d: &Device, button: u8, rec: Record) -> Result<()> {
    write_impl(d, button, rec, false)
}

fn write_impl(d: &Device, button: u8, rec: Record, safety_stock: bool) -> Result<()> {
    let is_stock = d.def.buttons.iter().find(|b| b.id == button).is_some_and(|b| rec == Record::stock(b.stock_usage));
    if crate::writes::writes_paused() && !(safety_stock && is_stock) {
        bail!("writes paused");
    }
    if safety_stock && !is_stock { bail!("safety restore accepts stock functions only"); }
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
    plan_for(def, rules, held, Plate::Any, taken)
}

/// Which side plate's binds a plan counts. A plate changes which physical buttons exist, not what
/// the firmware holds per id, so a button two plates bind differently is planned for the seated one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plate<'a> {
    /// Every plate's binds: a plan read for review, with no plate known.
    Any,
    /// The live mouse: this plate layer is seated, or `None` when no plate has been announced.
    Seated(Option<&'a str>),
}

impl Plate<'_> {
    fn counts(self, layer: Option<&str>) -> bool {
        match (self, layer) {
            (Plate::Seated(seated), Some(l)) if crate::engine::is_plate_layer(l) => seated == Some(l),
            _ => true,
        }
    }
}

/// [`plan`] with a seated plate: a bind on another plate's layer can never fire, so it neither
/// claims a button nor keeps the seated plate's bind out of firmware.
#[must_use]
pub fn plan_for(def: &DeviceDef, rules: &[Rule], held: Option<crate::controls::ControlRef>, plate: Plate<'_>, taken: &mut std::collections::BTreeSet<u8>) -> Vec<ButtonPlan> {
    let Some(first) = def.modes.first() else { return Vec::new() };
    let pid = CanonicalPid::of(first.product_id);
    let mut next_private = || {
        let usage = PRIVATE_POOL.rev().find(|u| !taken.contains(u))?;
        taken.insert(usage);
        Some(usage)
    };
    // Mutually exclusive side plates can expose the same stock control through separate banks.
    // Both banks must emit the same private key so startup needs no guessed plate identity.
    let mut private_by_stock = BTreeMap::new();
    def.buttons
        .iter()
        .map(|&ButtonSpec { id, stock_usage }| {
            let on_button = |page: u16, usage: u16, p: Option<CanonicalPid>| {
                page == 0x07 && usage == u16::from(stock_usage) && p == Some(pid)
            };
            let binds: Vec<&Rule> = rules
                .iter()
                .filter(|r| plate.counts(r.layer.as_deref()))
                .filter(|r| matches!(r.trigger, Trigger::Input { page, usage, pid: p } if on_button(page, usage, p)))
                .collect();
            let is_held = held.is_some_and(|c| on_button(c.page, c.usage, c.pid));
            let role = if binds.is_empty() && !is_held {
                Role::Stock
            } else {
                let firmware = match binds.as_slice() {
                    [only] if !is_held && only.layer.as_deref().is_none_or(crate::engine::is_plate_layer) => match &only.action {
                        Action::Key { key } => keyboard_function(key),
                        _ => None,
                    },
                    _ => None,
                };
                match firmware {
                    Some((mods, usage)) => Role::Performed { mods, usage },
                    None => {
                        let private = private_by_stock.get(&stock_usage).copied().or_else(|| {
                            let usage = next_private()?;
                            private_by_stock.insert(stock_usage, usage);
                            Some(usage)
                        });
                        private.map_or(Role::Stock, |usage| Role::Private { usage })
                    }
                }
            };
            ButtonPlan { id, stock_usage, role }
        })
        .collect()
}

/// What each device's firmware holds, as written and verified by [`apply`], and the input-path
/// questions asked of it. The process has one ([`APPLIED`]); it's a type so it can be tested alone.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeviceKey {
    pid: CanonicalPid,
    instance: String,
}

#[derive(Default, Debug)]
pub struct Table(BTreeMap<DeviceKey, Vec<ButtonPlan>>);

impl Table {
    fn record_applied_plan(&mut self, key: DeviceKey, plan: &[ButtonPlan], succeeded: &std::collections::BTreeSet<u8>) {
        let previous = self.0.remove(&key).unwrap_or_default();
        let mut next = Vec::with_capacity(plan.len());
        for p in plan {
            if succeeded.contains(&p.id) {
                next.push(*p);
            } else if let Some(old) = previous.iter().find(|old| old.id == p.id) {
                next.push(*old);
            }
        }
        if !next.is_empty() {
            self.0.insert(key, next);
        }
    }

    /// Emitted `usage` → the stock usage of the button that emitted it. `drift` is set when a
    /// managed button emitted its own stock key: the device lost its functions, so its table is
    /// dropped (the host performs its binds again) until re-applied.
    pub fn translate(&mut self, pid: CanonicalPid, instance: &str, usage: u16) -> (u16, bool) {
        let key = DeviceKey { pid, instance: instance.to_string() };
        if let Some(p) = self.0.get(&key).into_iter().flatten().find(|p| p.role != Role::Stock && u16::from(p.emits()) == usage) {
            return (u16::from(p.stock_usage), false);
        }
        let drift = self.0.get(&key).into_iter().flatten().any(|p| p.role != Role::Stock && u16::from(p.stock_usage) == usage);
        if drift {
            self.0.remove(&key);
        }
        (usage, drift)
    }

    /// Does the firmware perform the bind on stock key `usage`?
    #[must_use]
    pub fn performs(&self, pid: CanonicalPid, usage: u16) -> bool {
        let mut units = self.0.iter().filter(|(key, _)| key.pid == pid).peekable();
        units.peek().is_some() && units.all(|(_, plan)| plan.iter().any(|p| matches!(p.role, Role::Performed { .. }) && u16::from(p.stock_usage) == usage))
    }

    /// Is stock key `usage` managed at all (performed or private)?
    #[must_use]
    pub fn covers(&self, pid: CanonicalPid, usage: u16) -> bool {
        let mut units = self.0.iter().filter(|(key, _)| key.pid == pid).peekable();
        units.peek().is_some() && units.all(|(_, plan)| plan.iter().any(|p| p.role != Role::Stock && u16::from(p.stock_usage) == usage))
    }

    /// The device and stock usage private key `usage` stands for.
    #[must_use]
    pub fn private_source(&self, usage: u16) -> Option<(CanonicalPid, String, u8)> {
        let mut found = None;
        for (key, plan) in &self.0 {
            for p in plan {
                if matches!(p.role, Role::Private { usage: u } if u16::from(u) == usage) {
                    let source = (key.pid, key.instance.clone(), p.stock_usage);
                    if found.as_ref().is_some_and(|old| *old != source) { return None; }
                    found = Some(source);
                }
            }
        }
        found
    }

    fn forget_instance(&mut self, pid: CanonicalPid, instance: &str) -> bool {
        self.0.remove(&DeviceKey { pid, instance: instance.to_string() }).is_some()
    }

    /// Every assigned private key, `(pid, usage)`.
    #[must_use]
    pub fn private_keys(&self) -> Vec<(CanonicalPid, u8)> {
        self.0
            .iter()
            .flat_map(|(key, plan)| {
                plan.iter().filter_map(move |p| match p.role {
                    Role::Private { usage } => Some((key.pid, usage)),
                    _ => None,
                })
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

static APPLIED: Mutex<Table> = Mutex::new(Table(BTreeMap::new()));
static PRIVATE_POOL_QUARANTINED: AtomicBool = AtomicBool::new(false);

/// Suppress every private-pool key while stock restoration is unverified.
#[must_use]
pub fn private_pool_quarantined(usage: u16) -> bool {
    u8::try_from(usage).ok().is_some_and(|u| PRIVATE_POOL.contains(&u))
        && PRIVATE_POOL_QUARANTINED.load(Ordering::Acquire)
}

/// Keep private-pool input suppressed until an authoritative stock restore completes.
pub fn begin_private_pool_quarantine() {
    PRIVATE_POOL_QUARANTINED.store(true, Ordering::Release);
}

/// Clear private-pool suppression after every discovered device verifies stock functions.
pub fn clear_private_pool_quarantine() {
    PRIVATE_POOL_QUARANTINED.store(false, Ordering::Release);
}

fn applied() -> std::sync::MutexGuard<'static, Table> {
    APPLIED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set when the applied table changes, so the live loop re-arms the interceptor's private claims.
static CHANGED: AtomicBool = AtomicBool::new(false);
/// Set when a managed button was seen emitting its stock key: the device dropped its volatile
/// functions (idle revert, reconnect) and needs re-applying.
static DRIFTED: AtomicBool = AtomicBool::new(false);

/// Bring the device's firmware to `plan`, writing only buttons that differ. Every write is
/// verified; a failed button keeps its last verified plan. Returns how many requested non-stock
/// functions verified successfully.
pub fn apply(d: &Device, plan: &[ButtonPlan]) -> Result<usize> {
    apply_impl(d, plan, false, false)
}

fn apply_impl(d: &Device, plan: &[ButtonPlan], live_guard: bool, safety_stock: bool) -> Result<usize> {
    let key = DeviceKey {
        pid: CanonicalPid::of(d.def.modes.first().map_or(d.pid, |m| m.product_id)),
        instance: d.dpi_unit.clone(),
    };
    let mut live = Vec::with_capacity(plan.len());
    let mut succeeded = std::collections::BTreeSet::new();
    let mut first_err = None;
    for p in plan {
        let _arm = if live_guard && p.role != Role::Stock {
            match crate::safety::while_input_armed() {
                Some(guard) => Some(guard),
                None => return Err(anyhow::anyhow!("input disarmed")),
            }
        } else {
            None
        };
        let want = p.record();
        let ok = match read(d, p.id) {
            Ok(have) if have == want || (p.role == Role::Stock && have.as_keyboard() == Some((0, p.stock_usage))) => Ok(()),
            _ => write_impl(d, p.id, want, safety_stock),
        };
        match ok {
            Ok(()) => {
                live.push(*p);
                succeeded.insert(p.id);
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    let managed = live.iter().filter(|p| p.role != Role::Stock).count();
    {
        let mut t = applied();
        let previous = t.0.get(&key).cloned();
        t.record_applied_plan(key.clone(), plan, &succeeded);
        if t.0.get(&key) != previous.as_ref() {
            CHANGED.store(true, Ordering::Release);
        }
    }
    match first_err {
        Some(e) if live_guard => Err(e),
        Some(e) if plan.iter().all(|p| p.role == Role::Stock) => Err(e),
        Some(e) if managed == 0 && plan.iter().any(|p| p.role != Role::Stock) => Err(e),
        _ => Ok(managed),
    }
}

/// Apply one live GUI plan. Every managed-button read/write/readback transaction holds the arm
/// transition lease so a disarm cannot race through an in-flight firmware remap.
pub fn apply_live(d: &Device, plan: &[ButtonPlan]) -> Result<usize> {
    apply_impl(d, plan, true, false)
}

/// Return every button of `d` to its factory function and forget the device's applied table.
pub fn restore_stock(d: &Device) -> Result<()> {
    restore_stock_with(d, false)
}

/// Return every button to stock even while device writes are paused. This exception accepts only
/// the factory record for each declared button and still requires the device readback to match.
pub fn restore_stock_safety(d: &Device) -> Result<()> {
    restore_stock_with(d, true)
}

fn restore_stock_with(d: &Device, safety_stock: bool) -> Result<()> {
    let stock: Vec<ButtonPlan> = d
        .def
        .buttons
        .iter()
        .map(|b| ButtonPlan { id: b.id, stock_usage: b.stock_usage, role: Role::Stock })
        .collect();
    apply_impl(d, &stock, false, safety_stock).map(|_| ())
}

/// Forget one physical unit whose HID collection disappeared.
pub fn forget_instance(pid: u16, instance: &str) {
    if applied().forget_instance(CanonicalPid::of(pid), instance) {
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

/// Instance-aware ingress translation for same-model physical devices.
#[must_use]
pub fn translate_instance(raw_pid: u16, instance: &str, usage: u16) -> u16 {
    let (usage, drift) = applied().translate(CanonicalPid::of(raw_pid), instance, usage);
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
    applied().private_source(usage).is_some_and(|(p, _, _)| p == pid)
}

/// Stock buttons currently held through their private key, per device.
static PRIVATE_HELD: Mutex<BTreeMap<CanonicalPid, Vec<(u16, u16)>>> = Mutex::new(BTreeMap::new());

/// The keyboard hook saw private key `usage` go down or up: deliver the button's stock edge to the
/// engine on the device's deferred stream, and tell the hook to swallow it. The hook can resolve
/// the device without Raw Input because only that device emits the key; it has to, because a
/// swallowed keystroke never reaches Raw Input. `false` when `usage` isn't a private key.
pub fn deliver_private(usage: u16, down: bool) -> bool {
    let Some((pid, _, stock)) = applied().private_source(usage) else { return false };
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
    fn partial_stock_restore_keeps_failed_private_mapping() {
        let pid = CanonicalPid::of(0x00A7);
        let key = DeviceKey { pid, instance: "unit-a".into() };
        let private = ButtonPlan { id: 1, stock_usage: 0x2D, role: Role::Private { usage: 0x73 } };
        let performed = ButtonPlan { id: 2, stock_usage: 0x2E, role: Role::Performed { mods: 0, usage: 0x0A } };
        let stock = [
            ButtonPlan { role: Role::Stock, ..private },
            ButtonPlan { role: Role::Stock, ..performed },
        ];
        let mut table = Table::default();
        table.0.insert(key.clone(), vec![private, performed]);

        table.record_applied_plan(key, &stock, &std::collections::BTreeSet::from([2]));

        assert_eq!(table.private_source(0x73), Some((pid, "unit-a".into(), 0x2D)));
        assert!(!table.performs(pid, 0x2E));
        assert!(table.covers(pid, 0x2D));
        assert!(!table.covers(pid, 0x2E));
    }

    #[test]
    fn partial_live_apply_keeps_failed_private_mapping() {
        let pid = CanonicalPid::of(0x00A7);
        let key = DeviceKey { pid, instance: "unit-a".into() };
        let previous = ButtonPlan { id: 1, stock_usage: 0x2D, role: Role::Private { usage: 0x73 } };
        let requested = ButtonPlan { role: Role::Performed { mods: 0, usage: 0x0A }, ..previous };
        let mut table = Table::default();
        table.0.insert(key.clone(), vec![previous]);

        table.record_applied_plan(key, &[requested], &std::collections::BTreeSet::new());

        assert_eq!(table.private_source(0x73), Some((pid, "unit-a".into(), 0x2D)));
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
        assert_eq!(first, vec![0x73, 0x72, 0x73, 0x72], "both plate banks share each stock control's key, F24 first");
        assert_eq!(taken.len(), 4, "two logical controls per device consume four private keys");
        assert!(second.iter().all(|u| !first.contains(u)), "{first:?} vs {second:?}");
    }

    #[test]
    fn six_button_bank_uses_saved_stock_binds_without_a_plate_announcement() {
        let pid = CanonicalPid::of(0x00A7);
        let rules = [bind(0x1E, key("h"), None), bind(0x1F, Action::MouseButton { button: crate::action::MouseButtonKind::Middle }, None)];
        let mut taken = Default::default();
        let plan = plan_for(&naga(), &rules, None, Plate::Seated(None), &mut taken);
        let role = |id| plan.iter().find(|p| p.id == id).unwrap().role;
        assert_eq!(role(0x50), Role::Performed { mods: 0, usage: 0x0B });
        assert_eq!(role(0x50), role(0x40));
        assert_eq!(role(0x51), Role::Private { usage: 0x73 });
        assert_eq!(role(0x51), role(0x41));
        assert_eq!(taken.len(), 1, "aliased banks must not exhaust the private pool");
        let stock = plan_for(&naga(), &[], None, Plate::Seated(None), &mut Default::default());
        for (id, usage) in (0x50..=0x55).zip(0x1E..=0x23) {
            let p = stock.iter().find(|p| p.id == id).unwrap();
            assert_eq!(p.record(), Record::stock(usage));
        }
        let mut table = Table::default();
        table.0.insert(DeviceKey { pid, instance: "unit-a".into() }, plan);
        assert_eq!(table.private_source(0x73), Some((pid, "unit-a".into(), 0x1F)));
        assert_eq!(table.private_keys(), vec![(pid, 0x73)]);
        assert_eq!(table.translate(pid, "unit-a", 0x73), (0x1F, false));
        assert_eq!(table.translate(pid, "unit-a", 0x0B), (0x1E, false));
    }

    #[test]
    fn six_button_bank_tracks_plate_scoped_overrides() {
        let rules = [
            bind(0x1E, key("h"), Some("plate:6-button")),
            bind(0x1E, key("g"), Some("plate:12-button")),
        ];
        for (plate, expected) in [
            (Some("plate:6-button"), Role::Performed { mods: 0, usage: 0x0B }),
            (Some("plate:12-button"), Role::Performed { mods: 0, usage: 0x0A }),
            (None, Role::Stock),
        ] {
            let plan = plan_for(&naga(), &rules, None, Plate::Seated(plate), &mut Default::default());
            for id in [0x40, 0x50] {
                assert_eq!(plan.iter().find(|p| p.id == id).unwrap().role, expected);
            }
        }
    }

    #[test]
    fn all_naga_host_binds_fit_the_private_pool_across_both_banks() {
        let rules: Vec<_> = (0x1E..=0x27).chain([0x2D, 0x2E])
            .map(|usage| bind(usage, Action::MouseButton { button: crate::action::MouseButtonKind::Middle }, None))
            .collect();
        let mut taken = Default::default();
        let plan = plan_for(&naga(), &rules, None, Plate::Seated(None), &mut taken);
        assert_eq!(plan.len(), 18);
        assert!(plan.iter().all(|p| matches!(p.role, Role::Private { .. })));
        assert_eq!(taken.len(), 12);
    }

    #[test]
    fn private_aliases_cannot_name_different_controls_on_one_unit() {
        let pid = CanonicalPid::of(0x00A7);
        let mut table = Table::default();
        table.0.insert(DeviceKey { pid, instance: "unit-a".into() }, vec![
            ButtonPlan { id: 0x40, stock_usage: 0x1E, role: Role::Private { usage: 0x73 } },
            ButtonPlan { id: 0x51, stock_usage: 0x1F, role: Role::Private { usage: 0x73 } },
        ]);
        assert_eq!(table.private_source(0x73), None);
    }

    #[test]
    fn a_button_two_plates_bind_differently_is_planned_for_the_seated_plate() {
        // `-` is the 12-button plate's Space and the 2-button plate's Back.
        let rules = [
            bind(0x2D, key("space"), Some("plate:12-button")),
            bind(0x2D, Action::MouseButton { button: crate::action::MouseButtonKind::Back }, Some("plate:2-button")),
        ];
        let role = |plate| plan_for(&naga(), &rules, None, plate, &mut Default::default()).into_iter().find(|p| p.id == 0x4A).unwrap().role;
        assert_eq!(role(Plate::Seated(Some("plate:12-button"))), Role::Performed { mods: 0, usage: 0x2C });
        assert!(matches!(role(Plate::Seated(Some("plate:2-button"))), Role::Private { .. }), "a click is the host's");
        assert_eq!(role(Plate::Seated(None)), Role::Stock, "no plate announced: neither plate's bind can fire");
        assert!(matches!(role(Plate::Any), Role::Private { .. }), "a review plan counts both");
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
        t.0.insert(DeviceKey { pid, instance: "unit-a".into() }, plan);

        // Firmware-performed: the emitted 'g' is the '=' button; the engine must not act on it.
        assert_eq!(t.translate(pid, "unit-a", 0x0A), (0x2E, false));
        assert!(t.performs(pid, 0x2E));
        assert!(t.covers(pid, 0x2E) && t.covers(pid, 0x2D) && !t.covers(pid, 0x1E));
        // Host-performed: the private key names its device and stock button by itself.
        assert_eq!(t.private_source(u16::from(private)), Some((pid, "unit-a".into(), 0x2D)));
        assert_eq!(t.private_source(0x0A), None);
        assert_eq!(t.private_keys(), vec![(pid, private)]);
        // Unmanaged keys pass untouched.
        assert_eq!(t.translate(pid, "unit-a", 0x1E), (0x1E, false));

        // The '=' button emitting its stock key means the firmware lost its function.
        assert_eq!(t.translate(pid, "unit-a", 0x2E), (0x2E, true));
        assert!(!t.performs(pid, 0x2E), "the host performs its binds again until re-applied");
        assert!(t.private_keys().is_empty());
    }

    #[test]
    fn applied_translation_is_scoped_to_the_physical_unit() {
        let pid = CanonicalPid::of(0x00A7);
        let plan = plan(
            &naga(),
            &[bind(0x2D, Action::Run { cmd: "calc".into() }, None)],
            None,
            &mut Default::default(),
        );
        let mut t = Table::default();
        t.0.insert(DeviceKey { pid, instance: "unit-a".into() }, plan);
        assert_ne!(t.translate(pid, "unit-a", 0x73), (0x73, false));
        assert_eq!(t.translate(pid, "unit-b", 0x73), (0x73, false));
    }

    #[test]
    fn multi_unit_queries_fail_closed_and_forget_only_the_removed_unit() {
        let pid = CanonicalPid::of(0x00A7);
        let performed = plan(&naga(), &[bind(0x2E, key("g"), None)], None, &mut Default::default());
        let mut t = Table::default();
        t.0.insert(DeviceKey { pid, instance: "unit-a".into() }, performed.clone());
        t.0.insert(DeviceKey { pid, instance: "unit-b".into() }, performed.clone());
        assert!(t.performs(pid, 0x2E));
        assert!(t.covers(pid, 0x2E));

        let stock = plan(&naga(), &[], None, &mut Default::default());
        t.0.insert(DeviceKey { pid, instance: "unit-b".into() }, stock);
        assert!(!t.performs(pid, 0x2E));
        assert!(!t.covers(pid, 0x2E));

        assert!(t.forget_instance(pid, "unit-a"));
        assert_eq!(t.0.len(), 1);
        assert!(t.0.contains_key(&DeviceKey { pid, instance: "unit-b".into() }));
    }

    #[test]
    fn duplicate_private_usages_are_not_attributed_to_either_unit() {
        let pid = CanonicalPid::of(0x00A7);
        let plan = plan(&naga(), &[bind(0x1F, Action::Run { cmd: "calc".into() }, None)], None, &mut Default::default());
        let private = plan.iter().find_map(|p| match p.role { Role::Private { usage } => Some(usage), _ => None }).unwrap();
        let mut t = Table::default();
        for instance in ["unit-a", "unit-b"] {
            t.0.insert(DeviceKey { pid, instance: instance.into() }, plan.clone());
        }
        assert_eq!(t.private_source(u16::from(private)), None);
    }

    #[test]
    fn a_held_trigger_always_goes_to_the_host() {
        let held = crate::controls::ControlRef { page: 0x07, usage: 0x2E, pid: Some(CanonicalPid::of(0x00A7)) };
        let p = plan(&naga(), &[bind(0x2E, key("g"), None)], Some(held), &mut Default::default());
        assert!(matches!(p.iter().find(|p| p.stock_usage == 0x2E).unwrap().role, Role::Private { .. }));
    }
}
