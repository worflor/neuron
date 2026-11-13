# Tidepool cinematic

A 12-second, 1920×1080, 30 fps shot for the release and promotion: one slow camera move from low
over the Esc corner up to a three-quarter view of the whole board, lit by the Tidepool preset.

## What is real and what is staged

- **Every LED colour is Neuron's own output.** `make_frames.py` runs `../pattern-export`, which links
  neuron-core and renders the `tidepool` preset through `pattern::Compositor`. The first 3 s are
  dropped so the water has settled; the 12 s kept are used frame for frame, unchanged.
- **No key is pressed.** Tidepool also ripples from keystrokes, but the keyboard fixture does not
  move its keys, so the shot shows only the ambient water: nothing on screen could have caused a
  ripple. Key reads are scripted all up (`capture::script_key_reads`).
- **The keyboard is a stylised model**, the generic 6×22 fixture from the ignored
  `.local/showcase` harness (its `render_showcase.py` builds it from Neuron's core key map), not a
  scan of a real board.
- **Staging lives in the scene:** the camera move and focus, the fixture's floor and
  studio lights and a soft glow in the compositor. `--led-power` sets the fixture's optical output
  and never changes an RGB value.

## Build

Requirements: the `.local/showcase` harness with its local bpy 4.5 wheel, Python 3.11 with
Pillow, Rust, ffmpeg. Output goes to `D:\build-cache\promo\tidepool` (`PROMO_OUT`).

```powershell
$env:CARGO_TARGET_DIR = "D:\build-cache\promo\target"
cargo build --release --manifest-path ..\pattern-export\Cargo.toml
python make_frames.py                      # tidepool-frames.json, through the real engine
$py = "C:\Users\Micha\AppData\Local\Programs\Python\Python311\python.exe"
& $py ..\..\..\.local\showcase\tools\run_bpy.py --script cinematic.py `
    --frames D:\build-cache\promo\tidepool\tidepool-frames.json `
    --output D:\build-cache\promo\tidepool\full --samples 40 --resume
ffmpeg -framerate 30 -i D:\build-cache\promo\tidepool\full\tidepool_%04d.png `
    -vf "fade=t=in:st=0:d=0.6,fade=t=out:st=11.3:d=0.7" -c:v libx264 -crf 16 -pix_fmt yuv420p `
    D:\build-cache\promo\tidepool\neuron-tidepool-cinematic.mp4
```

`cinematic.py` takes `--still N`, `--start/--count`, `--scale` and `--samples` for tests; a full
frame is about 3 s on an RTX 3060 (OptiX, 40 samples). The engine frames depend on neuron-core at
the time of the build: rebuild the exporter after core changes before trusting old output.
