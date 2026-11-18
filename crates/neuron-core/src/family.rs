// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Colour lighting on the non-Razer families (VIA, Vial, HID++). [`crate::lighting::Lights`] routes a
//! family device here; each family frames its own writes. A unit's hold on its lighting outlives a
//! single stream, so what neuron took (and what it must put back) is kept per unit until
//! [`release`], which [`crate::device::Device::release_custody`] calls.

use crate::device::Device;
use crate::dialect::Sent;
use crate::lighting::{LightingDef, Protocol, Rgb};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

enum Link {
    Hidpp(crate::hidpp::ColourLink),
    Via(crate::via::ColourLink),
}

/// One unit's hold. Taking, painting and releasing all happen under its lock, so two painters
/// can't both take, and a painter that held the slot through a release stops instead of painting.
#[derive(Default)]
struct Slot {
    link: Option<Link>,
    released: bool,
}

fn slots() -> &'static Mutex<HashMap<String, Arc<Mutex<Slot>>>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Slot>>>>> = OnceLock::new();
    SLOTS.get_or_init(Mutex::default)
}

fn slot_of(dev: &Device) -> Arc<Mutex<Slot>> {
    slots().lock().unwrap_or_else(PoisonError::into_inner).entry(dev.dpi_unit.clone()).or_default().clone()
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The slot's link, taking the unit's lighting first when neuron doesn't hold it yet.
fn held<'a>(slot: &'a mut Slot, dev: &Device, def: &LightingDef) -> Result<&'a mut Link> {
    anyhow::ensure!(!slot.released, "the lighting was handed back");
    if slot.link.is_none() {
        let t = dev.transport();
        slot.link = Some(match def.protocol {
            Protocol::Hidpp => Link::Hidpp(crate::hidpp::take_lighting(t, &dev.def)?),
            Protocol::Via | Protocol::Vialrgb => Link::Via(crate::via::take_lighting(t, def)?),
            Protocol::Legacy | Protocol::Matrix => anyhow::bail!("a Razer lighting block has no family link"),
        });
    }
    Ok(slot.link.as_mut().expect("just taken"))
}

/// Take the unit's lighting, if neuron doesn't hold it already.
pub(crate) fn take(dev: &Device, def: &LightingDef) -> Result<()> {
    let slot = slot_of(dev);
    let mut slot = lock(&slot);
    held(&mut slot, dev, def).map(|_| ())
}

/// The colour each of the device's LEDs shows from a `rows`×`cols` frame, in its wire order. An
/// LED with no cell shows the frame's mean, so nothing is left at a firmware default.
#[must_use]
pub fn led_colours(def: &LightingDef, px: &[Rgb]) -> Vec<(u16, Rgb)> {
    let cols = usize::from(def.cols);
    let mean = mean_of(px);
    def.leds
        .iter()
        .map(|&[id, row, col]| {
            let at = usize::from(row) * cols + usize::from(col);
            let c = if row == crate::hidpp::NO_CELL { mean } else { px.get(at).copied().unwrap_or(mean) };
            (id, c)
        })
        .collect()
}

/// The frame's average colour.
#[must_use]
pub fn mean_of(px: &[Rgb]) -> Rgb {
    if px.is_empty() {
        return Rgb::new(0, 0, 0);
    }
    let n = px.len() as u32;
    let sum = px.iter().fold([0u32; 3], |s, c| [s[0] + u32::from(c.r), s[1] + u32::from(c.g), s[2] + u32::from(c.b)]);
    Rgb::new((sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8)
}

/// Paint one frame on a family device, taking its lighting first if needed.
pub(crate) fn paint(dev: &Device, def: &LightingDef, px: &[Rgb]) -> Result<Sent> {
    let slot = slot_of(dev);
    let mut slot = lock(&slot);
    let t = dev.transport();
    Ok(match held(&mut slot, dev, def)? {
        Link::Hidpp(h) => h.paint(t, &led_colours(def, px)),
        Link::Via(v) => v.paint(t, def, px),
    })
}

/// The unit's slot when neuron holds its lighting.
fn holding(dev: &Device) -> Option<Arc<Mutex<Slot>>> {
    slots().lock().unwrap_or_else(PoisonError::into_inner).get(&dev.dpi_unit).cloned()
}

/// Set the brightness slider's level (0..=255) on a held whole-board colour, which scales what
/// neuron paints. `None` when neuron doesn't hold the unit that way; the caller sets the board.
pub(crate) fn set_level(dev: &Device, level: u8) -> Option<Result<()>> {
    let slot = holding(dev)?;
    let mut slot = lock(&slot);
    match slot.link.as_mut()? {
        Link::Via(v) => v.set_level(dev.transport(), level),
        Link::Hidpp(_) => None,
    }
}

/// The brightness slider's level on a held whole-board colour.
pub(crate) fn level(dev: &Device) -> Option<u8> {
    let slot = holding(dev)?;
    let slot = lock(&slot);
    match slot.link.as_ref()? {
        Link::Via(v) => v.level(),
        Link::Hidpp(_) => None,
    }
}

/// Give the unit's lighting back to the device, putting back what neuron changed to take it.
pub(crate) fn release(dev: &Device) {
    let Some(slot) = slots().lock().unwrap_or_else(PoisonError::into_inner).remove(&dev.dpi_unit) else { return };
    let mut slot = lock(&slot);
    slot.released = true;
    let t = dev.transport();
    match slot.link.take() {
        Some(Link::Hidpp(h)) => h.release(t),
        Some(Link::Via(v)) => v.release(t),
        None => {}
    }
}
