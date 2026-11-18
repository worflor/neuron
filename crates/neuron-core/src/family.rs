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
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

enum Link {
    Hidpp(crate::hidpp::ColourLink),
    Via(crate::via::ColourLink),
}

fn links() -> &'static Mutex<HashMap<String, Arc<Mutex<Link>>>> {
    static LINKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Link>>>>> = OnceLock::new();
    LINKS.get_or_init(Mutex::default)
}

/// The unit's link, taking its lighting first when neuron doesn't hold it yet.
fn link(dev: &Device, def: &LightingDef) -> Result<Arc<Mutex<Link>>> {
    if let Some(l) = links().lock().unwrap_or_else(PoisonError::into_inner).get(&dev.dpi_unit) {
        return Ok(l.clone());
    }
    let t = dev.transport();
    let taken = match def.protocol {
        Protocol::Hidpp => Link::Hidpp(crate::hidpp::take_lighting(t, &dev.def)?),
        Protocol::Via | Protocol::Vialrgb => Link::Via(crate::via::take_lighting(t, def)?),
        Protocol::Legacy | Protocol::Matrix => anyhow::bail!("a Razer lighting block has no family link"),
    };
    let l = Arc::new(Mutex::new(taken));
    links().lock().unwrap_or_else(PoisonError::into_inner).insert(dev.dpi_unit.clone(), l.clone());
    Ok(l)
}

/// Take the unit's lighting, if neuron doesn't hold it already.
pub(crate) fn take(dev: &Device, def: &LightingDef) -> Result<()> {
    link(dev, def).map(|_| ())
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
    let l = link(dev, def)?;
    let mut l = l.lock().unwrap_or_else(PoisonError::into_inner);
    let t = dev.transport();
    Ok(match &mut *l {
        Link::Hidpp(h) => h.paint(t, &led_colours(def, px)),
        Link::Via(v) => v.paint(t, def, px),
    })
}

/// Give the unit's lighting back to the device, putting back what neuron changed to take it.
pub(crate) fn release(dev: &Device) {
    let Some(l) = links().lock().unwrap_or_else(PoisonError::into_inner).remove(&dev.dpi_unit) else { return };
    let l = l.lock().unwrap_or_else(PoisonError::into_inner);
    let t = dev.transport();
    match &*l {
        Link::Hidpp(h) => h.release(t),
        Link::Via(v) => v.release(t),
    }
}
