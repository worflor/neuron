# v0.1.5 promotional draft

Local review material; not published or approved for release. No AI-generated artwork is included.

- source-inspector-fixture.png and source-single-frame-fixture.png: fresh Slint software-renderer snapshots from the v0.1.5 candidate's disarmed spectrum-controls GUI regression, 2026-10-11. Mock device, no hardware writes. The scene fixture uses the shipping editor and callbacks; it is not physical LED read-back.
- diagonal-screenshot-draft.png: deterministic crop-and-layout of the inspector screenshot, using compose.py and Pillow. UI pixels are cropped and resized; no controls, colours or text inside the UI are invented.

The candidate is based on implementation 9382b13 with the workspace version changed to 0.1.5. The capture test passed in a fresh process with NEURON_REQUIRE_GUI=1 and NEURON_INSPECTOR_SHOTS pointing to a private output directory. It is apptest::spectrum_controls_reverse_only_the_active_frame_and_draw_relevant_properties.

Run python compose.py from this directory to reproduce the card. The source screenshots remain alongside it for comparison. Review composition and release copy before publishing.