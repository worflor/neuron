# Dev ad: "i'm lazy. set up my devices."

A 23-second vertical (1080×1920, 30 fps) promo for developers. One continuous Blender shot: a
terminal pane floats behind the rig, an agent drives the `neuron` CLI, and each command sends a
thread of light from its line in the terminal to the device it configures.

Draft, not approved for publication.

## What is real and what is staged

- Every command in the terminal is real `neuron` CLI syntax (checked against v0.1.4 `--help`).
  The devices are the owner's: Naga V2 Pro, BlackWidow Chroma V2, Seiren V3 Mini.
- The agent's dialogue is written copy, not a recorded session.
- The keyboard is the generic BlackWidow fixture from `.local/showcase` (core 6×22 key map, not a
  scan). The mouse and mic are stylised stand-ins.
- The keyboard's light is visualisation, except where a capture is passed: `gen_lighting.py
  --capture` replaces the aurora beat with real frames recorded by
  `.local/showcase/tools/capture_controller.py`. Without one, the aurora is a stand-in field in the
  brand palette.

## Build

All timing lives in `timeline.py`. Output goes to `D:\build-cache\promo\dev-ad` (`PROMO_OUT`).

```powershell
python gen_terminal.py      # term/#####.png + anchors.json
python gen_lighting.py      # lighting.npz  (add --capture FILE for real aurora frames)
& D:\tools\blender-5.2.2-windows-x64\blender.exe -b --factory-startup --python build_scene.py -- --resume
python assemble.py          # end card + neuron-dev-ad.mp4
```

`build_scene.py` takes `--frames 1,90,300`, `--range a:b`, `--scale 0.5` and `--samples N` for test
passes; a full render is about 2.6 s/frame on an RTX 3060 (OptiX, 24 samples).
