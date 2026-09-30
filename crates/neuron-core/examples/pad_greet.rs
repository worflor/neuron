// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Greet every connected layout pad with its start-up sequence (from its layout file), then read
//! and decode its stream for N seconds, printing every change. Proves a pad works with no other
//! app driving it. Run: pad_greet [secs] [--no-init]
use neuron::layout;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let greet = !args.iter().any(|a| a == "--no-init");
    for i in neuron::transport::enumerate()? {
        let Some(l) = layout::find(i.vid, i.pid) else { continue };
        if !(i.usage_page == 0x01 && matches!(i.usage, 0x04 | 0x05)) || i.output_len == 0 {
            continue;
        }
        println!("{} ({:04x}:{:04x}) out={} in={}", l.name, i.vid, i.pid, i.output_len, i.input_len);
        let t = neuron::transport::open_path(&i.path)?;
        // What is it doing before we say anything?
        let mut buf = vec![0u8; 64.max(usize::from(i.input_len))];
        match t.read_input(&mut buf, 300) {
            Ok(n) => println!("  before greeting: report {:02x} ({n} bytes)", buf[0]),
            Err(_) => println!("  before greeting: silent"),
        }
        if args.iter().any(|a| a == "--trace") {
            // Step through the sequence by hand, printing every report that comes back.
            for step in &l.init {
                let mut out = step.out.clone();
                out.resize(usize::from(i.output_len), 0);
                let w = t.write_output(&out);
                println!("  -> {:02x?} {}", &step.out, if w.is_ok() { "" } else { "(write failed)" });
                let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
                while std::time::Instant::now() < until {
                    if let Ok(n) = t.read_input(&mut buf, 50) {
                        println!("     <- {:02x?}", &buf[..n.min(12)]);
                    }
                }
            }
            // Then just watch: which report ids arrive, how many.
            let mut ids: std::collections::BTreeMap<u8, u32> = std::collections::BTreeMap::new();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < until {
                if let Ok(n) = t.read_input(&mut buf, 100) {
                    let c = ids.entry(buf[0]).or_default();
                    if *c < 3 {
                        println!("     <- {:02x?}", &buf[..n.min(14)]);
                    }
                    *c += 1;
                }
            }
            println!("  report ids over 10s: {ids:02x?}");
        } else if greet {
            match layout::run_init(t.as_ref(), l, usize::from(i.output_len)) {
                Ok(()) => println!("  greeted: every step answered"),
                Err(e) => println!("  greeting failed: {e}"),
            }
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut last = None;
        let mut reports = 0u32;
        while std::time::Instant::now() < until {
            let Ok(n) = t.read_input(&mut buf, 100) else { continue };
            reports += 1;
            if args.iter().any(|a| a == "--raw") && n > 11 {
                let lx = layout::bits(&buf, 48, 12).unwrap_or(0) / 256;
                let ly = layout::bits(&buf, 60, 12).unwrap_or(0) / 256;
                let line = format!("id {:02x} btn {:02x} {:02x} {:02x} Lstick~({lx},{ly}) len {n}", buf[0], buf[3], buf[4], buf[5]);
                if last.as_ref() != Some(&line) {
                    println!("  {line}");
                    last = Some(line);
                }
                continue;
            }
            if let Some(pad) = l.decode(&buf[..n]) {
                let q = |v: f32| (v * 10.0).round() / 10.0;
                let line = format!(
                    "buttons {:05x} L ({},{}) R ({},{}) ZL {} ZR {}",
                    pad.buttons, q(pad.left_x), q(pad.left_y), q(pad.right_x), q(pad.right_y), pad.left_trigger, pad.right_trigger
                );
                if last.as_ref() != Some(&line) {
                    println!("  {line}");
                    last = Some(line);
                }
            }
        }
        println!("  {reports} reports in {secs}s");
    }
    Ok(())
}
