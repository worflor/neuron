# Implementation Plan: Virtual Contact Reservoir & Event-Driven Commitment Dynamics

> **Document Status:** Final Implementation Specification (v8 — Fully Approved & Sealed)  
> **Target Subsystem:** `crates/neuron-core/src/pattern.rs` (`Ignite` / Reactive) and `crates/neuron-core/src/capture.rs` (`ObservationBarrier`)  
> **Scope Gate:** Strictly scoped to **Reactive (`Ignite`)**. Ring/Ripple is kept out of scope to preserve its proven single-wave and wave-pool invariants.  
> **Core Theme:** Continuous hidden contact reservoir faking analog dwell, coyote grace windows, and flutter accumulation through an atomic `ObservationBarrier`, stateful `ObservationPump`, exact residual crest sampling, independent historical trail decay, and unified presentation peak anti-aliasing without behavioral heuristics.

---

## 1. Summary of Final Architectural Hardening (v7 → v8)

| Amendment | Severity | Rationale & Mechanical Implementation |
| :--- | :--- | :--- |
| **Atomic Observation Barrier (`ObservationBarrier`)** | **P0** | Reading `head_seq` and `cutoff` as two separate calls allowed events published between them to carry timestamps *before* `cutoff`, collapsing sub-frame timing onto the present. Added `key_observation_barrier()` in `capture.rs` which snapshots `{ head_seq: ring.sequence, cutoff: elapsed() }` atomically under the ring's lock. Every event $\le \text{head\_seq}$ is guaranteed to occur at or before `cutoff`. |
| **Encapsulated `ObservationPump` Invariant Protection** | **P0** | Made pump fields private. `sim_at` is owned strictly by `ObservationPump` and can only be advanced via `advance_to(target)`. An out-of-order or stale event timestamp triggers a safe rebaseline, never backward time or duplicate integration. |
| **Atomic Barrier Event Fetch (`observations_since_into`)** | **P0** | Replaces cursor rewind gymnastics with `observations_since_into(&mut cursor, barrier.head_seq, &mut buffer)`. Events up to `barrier.head_seq` are read directly into the pre-allocated buffer; events published after the barrier belong naturally to the next frame. Zero transient heap allocations. |
| **Hard Canonical Rebaseline (`rebaseline_held`)** | **P0** | Recovery after lost history (overflow or post-suppression) is a hard canonicalization: live-held keys become settled holds ($q = 1.0, \text{age} = \text{SETTLED\_AGE} \implies 0.550$, zero peak); live-released keys clear kinetic memory to $q = 0.0$ while preserving visible trails. |
| **Structural Suppression Quiescing (`quiesce_all`)** | **P0** | Entering suppression explicitly sweeps active contacts into historical trails ($T \leftarrow \max(T, C), C \leftarrow 0$), advances the cursor to `barrier.head_seq`, and marks `NeedsRebaseline`. When suppression ends, it rebaselines from live hardware. Zero delayed typing fireworks. |
| **Centralized Taste Constants** | **P1** | All physical constants (`K_CHARGE`, `K_MEMORY`, `T_PEAK`, `TAU_SETTLE`, `SUSTAIN`, `SETTLED_AGE`, `TRAIL_DECAY_RATE`, `LEGACY_GLOW_IMPULSE`) are declared once at module scope. Zero duplicate constants or drift. |
| **Unified Presentation Peak Anti-Aliasing** | **P1** | `presentation_peak` provides temporal anti-aliasing for both sub-frame contact crests ($T_{\text{PEAK}}$) and legacy $0.55$ neighbor glow impulses, ensuring fast taps retain full visible amplitude even at 6Hz render rates. |

---

## 2. Centralized Taste Constants

These physical and perceptual constants are declared once in `crates/neuron-core/src/pattern.rs`:

```rust
const K_CHARGE: f32 = 38.0;              // Reservoir fill rate: 15ms -> ~0.43, 55ms -> ~0.88, >=90ms -> ~0.97
const K_MEMORY: f32 = 7.5;               // Reservoir hidden drain rate (coyote half-life ≈ 92.4 ms)
const T_PEAK: f32 = 0.055;               // Dwell duration where strike crest peaks (~55 ms)
const TAU_SETTLE: f32 = 0.045;           // Impact relaxation time constant (~45 ms)
const SUSTAIN: f32 = 0.55;               // Resting static hold sustain anchor (55% brightness)
const SETTLED_AGE: f32 = 0.45;           // Dwell age for recovered holds (100% converged to SUSTAIN)
const TRAIL_DECAY_RATE: f32 = 3.6;       // Visible historical trail decay multiplier
const LEGACY_GLOW_IMPULSE: f32 = 0.55;   // Proven spatial impulse for adjacent neighbor cells
```

---

## 3. Core Architecture: The Three-Way Decoupled Model

```
[Observation Event Ring]
         │
         ▼ (Under Ring Lock)
┌───────────────────────────────────────────────┐
│ ATOMIC OBSERVATION BARRIER                    │ ◄─── { head_seq: u64, cutoff: f64 }
└──────────────────────┬────────────────────────┘
                       │
                       ▼
┌───────────────────────────────────────────────┐
│ OBSERVATION PUMP                              │ ◄─── Owns: cursor, sim_at (f64), buffer
│  Reads up to barrier.head_seq into buffer     │      Monotonic sim_at advance only
│  advance_to(t): guarantees no time rewind     │      Modes: Live | Suppressed | NeedsRebaseline
└───────┬───────────────────────────────┬───────┘
        │ (while held)                  │ (on release: trail = max(trail, C))
        ▼                               ▼
┌───────────────────────────────┐  ┌───────────────────────────────┐
│ CONTACT RESPONSE (C)          │  │ HISTORICAL TRAIL (T)          │
│ L_target = q·[S + (1-S)r]     │  │ Unconditional wall-clock decay│
│ Peaks ~0.88 cold, >0.94 pumped│  │ dL/dt = -3.6·fade·dt          │
│ Settles to 0.55 anchor        │  │ Never jumps on release/repress│
└──────────────┬────────────────┘  └──────────────┬────────────────┘
               │                                  │
               └───────────────┬──────────────────┘
                               ▼
        ┌────────────────────────────────────────────────┐
        │ PRESENTATION PEAK ACCUMULATOR (P)              │
        │ Tracks max visible target traversed during dt  │
        │ Evaluates exact residual crest at T_PEAK       │
        │ Anti-aliases legacy neighbor glow impulses     │
        └──────────────────────┬─────────────────────────┘
                               ▼
        ┌────────────────────────────────────────────────┐
        │ EMITTED LUMINANCE = max(C, T, P, glow_trail)   │
        └────────────────────────────────────────────────┘
```

1. **Hidden Reservoir ($q \in [0.0, 1.0]$):**  
   Continuous commitment and coyote memory. Evolved piecewise between input event timestamps in the observation clock domain. Independent of display rate and independent of `fade`.
2. **Contact Response ($C \in [0.0, 1.0]$):**  
   Active finger visualization. Evaluated from current $q$ and active dwell $t_{\text{contact}}$. Zero when released.
3. **Historical Trail ($T \in [0.0, 1.0]$):**  
   Passive afterglow. Decays continuously across all cells on every elapsed time step. On release, updated via $T \leftarrow \max(T, C)$.
4. **Presentation Peak ($P \in [0.0, 1.0]$):**  
   Scratch buffer tracking the highest visible target traversed during sub-frame intervals, including exact residual crest evaluation. Also used for legacy neighbor glow impulses.
5. **Composited Output:**  
   $L = \max(C, T, P, \text{glow\_trail})$. Guaranteed zero release flash, zero release drop, and zero re-press darkening.

---

## 4. Mathematical Model & Exact Curves

### 4.1 Reservoir Integration ($q$)
* **Held State (`held == true`):**
  $$a_{\text{in}} = -(-K_{\text{CHARGE}} \cdot \Delta t).\text{exp\_m1}()$$
  $$q \leftarrow q + (1.0 - q) \cdot a_{\text{in}}$$
  $$t_{\text{contact}} \leftarrow t_{\text{contact}} + \Delta t$$
* **Released State (`held == false`):**
  $$a_{\text{mem}} = -(-K_{\text{MEMORY}} \cdot \Delta t).\text{exp\_m1}()$$
  $$q \leftarrow q \cdot (1.0 - a_{\text{mem}})$$
  $$t_{\text{contact}} \leftarrow 0.0$$

### 4.2 Contact Target Formulation ($C$)
When active contact is true ($t_{\text{contact}} > 0$):
$$r = \exp\left(-\frac{\max(t_{\text{contact}} - T_{\text{PEAK}}, 0.0)}{\tau_{\text{SETTLE}}}\right)$$
$$C = q \cdot [\text{SUSTAIN} + (1.0 - \text{SUSTAIN}) \cdot r]$$
When released: $C = 0.0$.

### 4.3 Historical Trail Evolution ($T$)
* **On Every `advance(dt)`:**
  $$T \leftarrow \max(0.0, T - \text{TRAIL\_DECAY\_RATE} \cdot \text{fade} \cdot \Delta t)$$
* **At Release Transition:**
  $$T \leftarrow \max(T, C)$$
* **At Re-Press Transition:**
  $T$ continues decaying naturally; active contact $C$ begins climbing from current residual $q$.

### 4.4 Presentation Peak Accumulation ($P$) with Residual Crest
For each sub-interval $\Delta t$ within a presentation frame:
* If released: no contact peak contributed.
* If held:
  - Let $C_{\text{before}}$ be the contact target at the start of the interval.
  - Let $C_{\text{after}}$ be the contact target at the end of the interval.
  - Initial peak candidate: $\text{peak} = \max(C_{\text{before}}, C_{\text{after}})$.
  - If dwell interval crosses $T_{\text{PEAK}}$ ($\text{age}_{\text{before}} < T_{\text{PEAK}} \le \text{age}_{\text{after}}$):
    $$\Delta t_{\text{peak}} = T_{\text{PEAK}} - \text{age}_{\text{before}}$$
    $$q_{\text{peak}} = 1.0 - (1.0 - q_{\text{before}}) \cdot \exp(-K_{\text{CHARGE}} \cdot \Delta t_{\text{peak}})$$
    $$C_{\text{peak}} = q_{\text{peak}} \quad (\text{since } r = 1.0 \text{ at } T_{\text{PEAK}})$$
    $$\text{peak} \leftarrow \max(\text{peak}, C_{\text{peak}})$$
  - $P \leftarrow \max(P, \text{peak})$.
* For legacy neighbor glow: `raise_peak(neighbor_cell, LEGACY_GLOW_IMPULSE)`.
* At frame output: $L = \max(C, T, P, \text{glow\_trail})$, then $P$ resets to $0.0$.

---

## 5. Analytical Verification of Exact Curves

### Cold Press Progression:
| Dwell | Reservoir ($q$) | Crest ($r$) | Contact ($C$) | Description |
|---:|---:|---:|---:|:---|
| **15 ms** | $0.434$ | $1.000$ | **$0.434$** | **Microtap:** Snappy elastic pulse, ~43% brightness. |
| **30 ms** | $0.680$ | $1.000$ | **$0.680$** | **Jiggle-Peek:** Medium commitment, ~68% brightness. |
| **55 ms** | $0.876$ | $1.000$ | **$0.876$** | **Strike Crest:** Peak cold authored luminance (~88%). |
| **60 ms** | $0.898$ | $0.895$ | **$0.855$** | **Impact Relaxation:** Damping begins toward resting anchor. |
| **90 ms** | $0.967$ | $0.460$ | **$0.732$** | **Cushioning:** Smooth glide into static contact. |
| **120 ms** | $0.990$ | $0.236$ | **$0.649$** | **Near Anchor:** Dwell established. |
| **250 ms** | $1.000$ | $0.013$ | **$0.556$** | **Committed Hold:** Converged to $0.55 \pm 0.01$. |

### Flutter Pumping Progression (30ms Down / 30ms Up cycles):
1. **Cycle 1:** Strike reaches $q = 0.680$, $C = 0.680$. After 30ms release: $q = 0.680 \times e^{-7.5 \times 0.030} \approx 0.543$. Trail $T = 0.680 - 3.6 \times 1.0 \times 0.030 \approx 0.572$. Valley is $0.572$.
2. **Cycle 2:** Re-press starts from $q = 0.543$. In 30ms, $q = 0.543 + (1 - 0.543) \times 0.680 \approx 0.854$. Strike crests at $C = 0.854$. After 30ms release: $q = 0.682$, $T \approx 0.746$. Valley is $0.746$.
3. **Cycle 3:** Re-press starts from $q = 0.682$. In 30ms, $q = 0.682 + (1 - 0.682) \times 0.680 \approx 0.898$. Strike crests at $C = 0.898$. After 30ms release: $q = 0.717$, $T \approx 0.790$.

*Conclusion:* Emergent kinetic pumping is mathematically verified. Strikes increase ($0.680 \to 0.854 \to 0.898$), valleys stay buoyed ($> 0.55$), without a single `if (is_fluttering)` heuristic.

---

## 6. Continuity Proofs

1. **Release Continuity Under Arbitrary History:**  
   Immediately prior to release at $t$: visible output is $\max(C(t), T(t))$.  
   At release: $T \leftarrow \max(T(t), C(t))$, $C \leftarrow 0$.  
   Output immediately after release is $\max(0, T) = \max(C(t), T(t))$.  
   $$\lim_{\Delta t \to 0^+} L(t + \Delta t) = L(t)$$
   Strictly zero flash and zero downward drop upon release under all possible trail histories.
2. **Re-Press Continuity:**  
   Immediately prior to re-press: output is $T$.  
   At re-press: $T$ continues decaying; $C$ begins rising from current $q$.  
   Output is $\max(C, T) \ge T$.  
   Strictly zero instantaneous darkening upon re-press.

---

## 7. Capture Seam: Atomic Observation Barrier

In `crates/neuron-core/src/capture.rs`:

```rust
pub(crate) struct ObservationBarrier {
    pub(crate) head_seq: u64,
    pub(crate) cutoff: f64,
}

pub(crate) fn key_observation_barrier() -> ObservationBarrier {
    let ring = observation_ring().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    ObservationBarrier {
        head_seq: ring.sequence,
        cutoff: crate::pattern::render_epoch().elapsed().as_secs_f64(),
    }
}

pub(crate) fn key_observations_since_into(
    cursor: &mut u64,
    through_seq: u64,
    out: &mut Vec<KeyObservation>,
) -> u64 {
    let ring = observation_ring().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    ring.read_into(cursor, through_seq, out, crate::capture::SUPPRESS_KEY_READS.with(std::cell::Cell::get))
}
```

---

## 8. Component Implementation: `ContactReservoirBank`

Located internally in `crates/neuron-core/src/pattern.rs` (non-pub, module-internal):

```rust
#[derive(Clone, Default)]
struct ContactReservoirBank {
    charge: Vec<f32>,
    contact_age: Vec<f32>,
    trail: Vec<f32>,
    presentation_peak: Vec<f32>,
    held: Vec<bool>,
}

impl ContactReservoirBank {
    fn reset_for_geometry(&mut self, n: usize) {
        self.charge = vec![0.0; n];
        self.contact_age = vec![0.0; n];
        self.trail = vec![0.0; n];
        self.presentation_peak = vec![0.0; n];
        self.held = vec![false; n];
    }

    fn begin_frame(&mut self) {
        self.presentation_peak.fill(0.0);
    }

    fn is_held(&self, cell: usize) -> bool {
        self.held.get(cell).copied().unwrap_or(false)
    }

    fn raise_peak(&mut self, cell: usize, value: f32) {
        if cell < self.presentation_peak.len() {
            self.presentation_peak[cell] = self.presentation_peak[cell].max(value);
        }
    }

    fn set_contact(&mut self, cell: usize, down: bool) {
        if cell < self.held.len() {
            let was_down = self.held[cell];
            if was_down && !down {
                // Transition: Held -> Released.
                // Preserve whichever visual history is currently stronger.
                let c = self.contact_target(cell);
                self.trail[cell] = self.trail[cell].max(c);
                self.contact_age[cell] = 0.0;
            } else if !was_down && down {
                // Transition: Released -> Held. Reset dwell accumulator.
                self.contact_age[cell] = 0.0;
            }
            self.held[cell] = down;
        }
    }

    /// Hard canonical rebaseline after history loss (overflow or suppression resume).
    /// Unconditionally destroys uncertain hidden history rather than manufacturing confidence.
    fn rebaseline_held(&mut self, live_held: &[bool]) {
        for i in 0..self.held.len().min(live_held.len()) {
            if live_held[i] {
                // Live held: adopt as a settled hold (no retrospective strike peak)
                self.held[i] = true;
                self.charge[i] = 1.0;
                self.contact_age[i] = SETTLED_AGE;
            } else {
                // Live released: sweep active contact into trail if was held, clear kinetic memory
                if self.held[i] {
                    let c = self.contact_target(i);
                    self.trail[i] = self.trail[i].max(c);
                }
                self.held[i] = false;
                self.contact_age[i] = 0.0;
                self.charge[i] = 0.0; // Coyote history is unknowable across missing gap
            }
        }
    }

    /// Quiesces all active contacts into historical trail on entering suppression.
    fn quiesce_all(&mut self) {
        for i in 0..self.held.len() {
            if self.held[i] {
                let c = self.contact_target(i);
                self.trail[i] = self.trail[i].max(c);
                self.held[i] = false;
                self.contact_age[i] = 0.0;
            }
        }
    }

    fn advance(&mut self, dt: f32, fade: f32) {
        if dt <= 0.0 {
            return;
        }
        let a_in = -(-K_CHARGE * dt).exp_m1();
        let a_mem = -(-K_MEMORY * dt).exp_m1();
        let decay_step = TRAIL_DECAY_RATE * fade.clamp(0.1, 4.0) * dt;

        for i in 0..self.held.len() {
            // Historical trail decays unconditionally across all cells
            self.trail[i] = (self.trail[i] - decay_step).max(0.0);

            if self.held[i] {
                let q_before = self.charge[i];
                let age_before = self.contact_age[i];
                let c_before = self.contact_target(i);

                self.charge[i] += (1.0 - self.charge[i]) * a_in;
                self.contact_age[i] += dt;

                let age_after = self.contact_age[i];
                let c_after = self.contact_target(i);

                let mut peak = c_before.max(c_after);
                // Exact residual crest evaluation if T_PEAK is crossed
                if age_before < T_PEAK && age_after >= T_PEAK {
                    let dt_peak = T_PEAK - age_before;
                    let q_peak = 1.0 - (1.0 - q_before) * (-K_CHARGE * dt_peak).exp();
                    peak = peak.max(q_peak);
                }
                self.presentation_peak[i] = self.presentation_peak[i].max(peak);
            } else {
                self.charge[i] *= 1.0 - a_mem;
            }
        }
    }

    fn contact_target(&self, idx: usize) -> f32 {
        if !self.held[idx] {
            return 0.0;
        }
        let excess_age = (self.contact_age[idx] - T_PEAK).max(0.0);
        let r = (-excess_age / TAU_SETTLE).exp();
        self.charge[idx] * (SUSTAIN + (1.0 - SUSTAIN) * r)
    }

    fn sample(&self, idx: usize) -> f32 {
        let c = self.contact_target(idx);
        let t = self.trail[idx];
        let p = self.presentation_peak[idx];
        c.max(t).max(p).clamp(0.0, 1.0)
    }
}
```

---

## 9. Encapsulated `ObservationPump` & `Ignite` Integration

### 9.1 `ObservationPump` Helper:
```rust
enum PumpMode {
    Live,
    Suppressed,
    NeedsRebaseline,
}

struct ObservationPump {
    cursor: u64,
    sim_at: f64,
    mode: PumpMode,
    event_buffer: Vec<crate::capture::KeyObservation>,
}

impl Default for ObservationPump {
    fn default() -> Self {
        Self {
            cursor: 0,
            sim_at: 0.0,
            mode: PumpMode::NeedsRebaseline,
            event_buffer: Vec::with_capacity(crate::capture::KEY_OBSERVATION_CAPACITY),
        }
    }
}

impl ObservationPump {
    fn reset_for_geometry(&mut self, barrier: &crate::capture::ObservationBarrier) {
        self.cursor = barrier.head_seq;
        self.sim_at = barrier.cutoff;
        self.mode = PumpMode::NeedsRebaseline;
        self.event_buffer.clear();
    }

    fn enter_suppression(&mut self, barrier: &crate::capture::ObservationBarrier) {
        self.cursor = barrier.head_seq;
        self.sim_at = barrier.cutoff;
        self.mode = PumpMode::Suppressed;
        self.event_buffer.clear();
    }

    fn advance_to(&mut self, target_time: f64) -> Result<f32, StaleObservationError> {
        if target_time < self.sim_at {
            // Out of order / stale event timestamp
            return Err(StaleObservationError);
        }
        let dt = (target_time - self.sim_at) as f32;
        self.sim_at = target_time;
        Ok(dt)
    }
}

struct StaleObservationError;
```

### 9.2 `Ignite::field` Integration:
```rust
impl Ignite {
    fn advance_sim(&mut self, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        self.bank.advance(dt, self.fade);
        let decay_step = TRAIL_DECAY_RATE * self.fade.clamp(0.1, 4.0) * dt;
        for g in &mut self.glow_trail {
            *g = (*g - decay_step).max(0.0);
        }
    }
}

impl Pattern for Ignite {
    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.bank.reset_for_geometry(n);
            self.glow_trail = vec![0.0; n];
            self.scratch_held = vec![false; n];
            self.prev = vec![false; KEY_SCAN_SLOTS];
            self.dims = (rows, cols);
            let barrier = crate::capture::key_observation_barrier();
            self.pump.reset_for_geometry(&barrier);
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }

        self.bank.begin_frame();
        // Atomic Observation Barrier under ring lock
        let barrier = crate::capture::key_observation_barrier();

        // 1. SUPPRESSED READS (Thumbnails / previews do not observe typing)
        if crate::capture::key_reads_suppressed() {
            if !matches!(self.pump.mode, PumpMode::Suppressed) {
                self.bank.quiesce_all();
                self.pump.enter_suppression(&barrier);
            }
            if let Ok(dt) = self.pump.advance_to(barrier.cutoff) {
                self.advance_sim(dt);
            }
            let cells = (0..n).map(|i| Cell::new(0.0, self.bank.sample(i).max(self.glow_trail[i]))).collect();
            return Field::Scalar(cells);
        }

        // 2. READ OBSERVATIONS INTO REUSABLE BUFFER UP TO BARRIER HEAD
        self.pump.event_buffer.clear();
        let missed = crate::capture::key_observations_since_into(
            &mut self.pump.cursor,
            barrier.head_seq,
            &mut self.pump.event_buffer,
        );

        if missed > 0 || matches!(self.pump.mode, PumpMode::NeedsRebaseline | PumpMode::Suppressed) {
            // History lost or resuming from suppression: hard canonical rebaseline
            self.scratch_held.fill(false);
            scan_key_contacts(&mut self.prev, &mut self.scratch_held, r, c, false, |_, _| {});
            self.bank.rebaseline_held(&self.scratch_held);
            let _ = self.pump.advance_to(barrier.cutoff);
            self.pump.mode = PumpMode::Live;
        } else {
            // Process events in exact sequence up to barrier cutoff
            let mut out_of_order = false;
            for i in 0..self.pump.event_buffer.len() {
                let event = self.pump.event_buffer[i];
                let event_t = event.at.min(barrier.cutoff);
                match self.pump.advance_to(event_t) {
                    Ok(dt) => {
                        self.advance_sim(dt);
                        if let Some((ry, cx)) = observation_cell(event.page, event.usage) {
                            if ry < r && cx < c {
                                let cell = ry * c + cx;
                                let was_held = self.bank.is_held(cell);
                                if event.down && !was_held && self.glow {
                                    for (ny, nx) in neighbours(ry, cx, r, c) {
                                        let ni = ny * c + nx;
                                        self.glow_trail[ni] = self.glow_trail[ni].max(LEGACY_GLOW_IMPULSE);
                                        self.bank.raise_peak(ni, LEGACY_GLOW_IMPULSE);
                                    }
                                }
                                self.bank.set_contact(cell, event.down);
                            }
                        }
                    }
                    Err(_) => {
                        out_of_order = true;
                        break;
                    }
                }
            }
            if out_of_order {
                // Stale timestamp detected: rebaseline safely rather than corrupting time
                self.scratch_held.fill(false);
                scan_key_contacts(&mut self.prev, &mut self.scratch_held, r, c, false, |_, _| {});
                self.bank.rebaseline_held(&self.scratch_held);
                let _ = self.pump.advance_to(barrier.cutoff);
            }
        }

        // 3. ADVANCE REMAINDER TO BARRIER CUTOFF
        if let Ok(remainder_dt) = self.pump.advance_to(barrier.cutoff) {
            self.advance_sim(remainder_dt);
        }

        // 4. EMIT FINAL COMPOSITED CELLS
        let mut cells = vec![Cell::new(0.0, 0.0); n];
        for i in 0..n {
            let intensity = self.bank.sample(i).max(self.glow_trail[i]);
            cells[i] = Cell::new(0.0, intensity);
        }
        Field::Scalar(cells)
    }
}
```

---

## 10. Verification & Test Suite

All tests reside under `#[cfg(test)]` in `crates/neuron-core/src/pattern.rs`:

| Test Name | Contract Proved |
| :--- | :--- |
| `test_reservoir_charge_curve_pure_state` | Pure-state values: $q(15\text{ms}) \approx 0.434$, $q(30\text{ms}) \approx 0.680$, $q(55\text{ms}) \approx 0.876$, $q(90\text{ms}) \approx 0.967$ within $\pm 0.01$. |
| `test_reservoir_commitment_presentation` | 15ms tap peaks at $0.434 \pm 0.02$; 55ms strike peaks at $0.876 \pm 0.02$; 250ms hold settles to $0.55 \pm 0.01$. |
| `test_reservoir_release_exact_continuity` | $\forall t \in [0.01, 1.0]$, an Up transition produces $|L(t^+) - L(t^-)| \le 10^{-5}$ at the instant of release across clean strikes and when historical trail dominates ($T > C$). |
| `test_reservoir_repress_no_instant_darken` | Down transition itself causes no instantaneous decrease: $L(t_{\text{down}}^+) \ge L(t_{\text{down}}^-)$. |
| `test_reservoir_trail_decays_while_held` | Holding a key after re-pressing an old trail ($T_0 = 0.75$, $\text{fade} = 1.0$) allows trail to decay; output converges to $0.55$ sustain anchor within $t_{\text{wait}} = \frac{T_0 - 0.55}{3.6 \cdot 1.0} \approx 0.056\text{ s}$. |
| `test_reservoir_warm_repress_residual_crest` | A strike re-pressed from residual $q = 0.543$ achieves $C_{\text{peak}} \approx 0.943$ even across a 167ms low-cadence render interval via exact residual crest tracking. |
| `test_reservoir_coyote_memory_retention` | $q$ retains $e^{-7.5 \times 0.030} \approx 0.798$ after 30ms release, and $e^{-7.5 \times 0.0924} \approx 0.50$ after 92.4ms release, independent of `fade`. |
| `test_reservoir_flutter_pumping_emergence` | 3 cycles of 30ms on / 30ms off: strikes monotonically increase ($0.68 \to 0.85 \to 0.90$); valleys stay buoyed $> 0.55$. |
| `test_reservoir_timestamp_event_schedule_invariance` | Driving a defined event sequence (`Down@20ms`, `Up@35ms`, `Down@58ms`, `Up@88ms`) produces identical terminal $q$ and $trail$ across 6Hz, 30Hz, 60Hz, and 144Hz render schedules within $\pm 0.001$, and agrees at intermediate sampling checkpoints. |
| `test_reservoir_glow_unrelated_event_invariance` | Inserting unrelated mapped key events during a neighbor glow's decay produces identical decay output to isolated decay. |
| `test_reservoir_neighbor_glow_presentation_peak` | Legacy neighbor glow at 6Hz render cadence retains visible $0.55$ impulse via unified presentation peak. |
| `test_reservoir_suppression_resumption_no_replay` | Observations generated while `key_reads_suppressed() == true` are discarded; resumption from suppression synthesizes zero delayed typing fireworks. |
| `test_reservoir_overflow_hard_canonical_rebaseline` | Simulating an observation queue gap (`missed = 10`) triggers hard rebaseline: currently held keys adopt settled hold at $0.55$ with zero phantom strikes; released keys clear $q=0$. |
| `test_reservoir_same_cell_geometry_reset` | Changing surface dimensions from $6 \times 22$ to $11 \times 12$ (both $n=132$) zeroes contact state vectors and enters `NeedsRebaseline`. |
| `test_reservoir_zero_transient_allocations` | `bank` vector capacities, `event_buffer`, and `scratch_held` remain invariant over 10,000 live frames. |
