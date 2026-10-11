# Bugs to fix

## Multiple running instances and lighting state

Observed on Windows on 2026-10-04, with the owner's BlackWidow keyboard and two processes running the official Neuron v0.1.4 build. These are user-reported hardware observations; the exact failure sequence has not been reproduced or profiled yet.

- Two Neuron instances can run together, and their lighting output fights on the same keyboard.
- During that overlap, the keyboard partially painted only the top keys, retained part of the first instance's pattern, and displayed lighting in the wrong positions.
- Closing the older process did not restore correct keyboard lighting. The owner tried scanning and applying on the Device page, but neither recovered it. Fully closing and relaunching the remaining Neuron instance reset the keyboard state and fixed the lighting.

The duplicate was created by opening the installed executable while its tray instance was already running. Both processes used the same executable and release build; this was not a conflict between different installed versions.

### Work to investigate later

- Establish a singleton app lifecycle. Opening Neuron again should bring the existing instance forward. Inspect existing process and device ownership mechanisms before deciding where enforcement belongs.
- Reproduce overlapping lighting output safely and determine whether partial or misplaced output comes from competing writers, stale placement or layer state, frame dimensions, or a failure to restore a complete frame. These are hypotheses, not established causes.
- Verify recovery after an instance exits: one writer owns the device, the active setup is correct, and the complete keyboard frame resumes without restarting or unplugging the hardware.
- Consider a small **Refresh lighting** button in the lighting feature's existing space. It should flush stale lighting work, restart the lighting state and output lifecycle, and repaint the active setup without requiring a full app restart. Investigate which state actually needs resetting before defining its behavior; scanning and applying device settings did not provide that recovery in this incident.

### Adversarial testing and profiling

Exercise different lighting setups, layer stacks, palettes, placements, regions, masks, and blend modes. Change them while effects are running, including transitions between a full-board setup and a small region. Include rapid edits, preset and profile changes, and empty or disabled stacks.

Check arming and disarming transitions, repeated toggles, opening and closing the editor, tray startup, a second launch, and instance shutdown during an edit. Observe whether software previews and physical lighting agree throughout, and whether changing one part of the setup leaves stale pixels elsewhere.

Record the precise reproduction sequence, configuration snapshots, process IDs, device ownership, and before/after frames. Profile frame generation, device write cadence and latency, queue depth, missed or partial updates, CPU/GPU usage, and memory growth where instrumentation exists. Separate confirmed behavior from suspected causes and compare a normal single-instance baseline with each broken state.

Automated tests must remain disarmed and use simulated input and mock transport. Any live arming, HID interception, or device-write testing requires the owner's explicit authorization and the existing safety and hardware-verification gates. Do not implement fixes or start live adversarial tests as part of recording this backlog.
