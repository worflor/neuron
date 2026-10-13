# v0.1.5 release card

An exploded view of a two-layer lighting scene beside the spectrum editor. No AI-generated artwork is included.

- source-inspector-fixture.png and source-single-frame-fixture.png: fresh Slint software-renderer snapshots from the v0.1.5 candidate's disarmed spectrum-controls GUI regression, 2026-10-11. Mock device, no hardware writes. The scene fixture uses the shipping editor and callbacks; it is not physical LED read-back.
- source-exploded-layers.png: a stylized Blender keyboard with floating Fire and Wave fields, aligned through the existing core-exported key map. Their separation and guides explain the stack; they are not a physical device feature. Geometry and optics are a visualization.
- source-layer-frame.json: exact Wave, Fire and combined RGB frames from neuron-core at the recorded source commit. Fire uses Screen blending at 65% opacity over Wave. Rendering uses deterministic provider policy and suppressed live key reads; nothing is written to hardware or synthesized as input.
- diagonal-screenshot-draft.png: composition from the source render and real editor crop, using compose.py and Pillow. UI controls and values are not fabricated. The editor crop demonstrates spectrum controls separately from the layered scene.

The editor capture is based on implementation 9382b13 with workspace version 0.1.5. Its regression fixture is apptest::spectrum_controls_reverse_only_the_active_frame_and_draw_relevant_properties. The exploded render uses the engine at 4efed239a9ac755ee20c059e10c88f2997a008d1. The source render and RGB frame are retained for comparison.

Run python compose.py to reproduce the card layout from the retained source images. This reproduces the composition, not the Blender scene. Pillow and the Windows Bahnschrift/Cascadia Mono fonts are required.
