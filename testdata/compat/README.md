# Offline protocol compatibility corpus

These 16 fixtures exercise Neuron’s production device selection, typed capabilities, lighting builders, framing, and read-back verification through a virtual HID bus. The runner is [`compatibility_corpus.rs`](../../crates/neuron-testkit/tests/compatibility_corpus.rs); it denies hardware access, opens a production `Device` over `ReplayBackend`, and requires every scripted request and reply to be consumed.

The corpus has six HID++ length/DPI cases, three legacy-lighting cases, two extended-matrix cases, four brightness verification cases, and one Razer framing/polling case. Each fixture declares `evidence = "source-derived"` and `replies = "synthetic"`; the runner rejects other grades. The request literals are transcribed from pinned source facts. Replies are constructed test inputs, not captures.

The files under `definitions/` use deliberately synthetic PIDs, usages, and report sizes to route the test through production selection. They are not hardware descriptors or evidence that those interfaces exist. Passing a fixture validates the encoded request and reply behavior against its stated source facts; it does not promote a device, capability, or protocol fact to hardware-supported status.

The corpus preserves exact report lengths. Separate HID++ unit regressions model [Windows' fixed-size read envelope](https://github.com/microsoft/Windows-driver-samples/blob/2dc3fd3a0cc84a2933f2194e7ec0871584979071/hid/hclient/report.c#L39-L62) explicitly: a complete short reply in a 20-byte OS buffer is normalized before decoding, while an exact-length transport still rejects overlong short replies. Padding is a transport property, not inferred from the host running the test.

Run the focused corpus with:

```powershell
cargo test -p neuron-testkit --test compatibility_corpus
```

The required workspace gate remains:

```powershell
.\validate.ps1
```

For future fixtures, keep request bytes literal and independently checked against the cited source. Do not generate expected bytes with Neuron’s encoders; that would make the test compare an implementation with itself. Record the upstream repository, full commit SHA, file, symbol or line range, and the narrow fact each source establishes. Keep uncertain or source-conflicting fields explicit. A synthetic reply must remain labelled synthetic even when it matches the documented reply shape.

Any future real capture needs owner authorization and privacy review before import. Preserve provenance such as device model and link mode, firmware, date, platform, capture tool, and the exact operation; remove serials, local paths, and other identifying data. Do not commit raw captures without that review. Respect source licenses and attribution, and do not copy upstream implementation code into fixtures.

## Extended Naga custom-frame boundary

The matrix fixtures use a synthetic source-shaped selector. They do not encode the production Naga definition. The extended custom-display packet is an unresolved source-versus-live boundary and should stay visible in review:

- At pinned OpenRazer commit [`477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d`](https://github.com/openrazer/openrazer/tree/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d), [`razerchromacommon.c`](https://github.com/openrazer/openrazer/blob/477f4d6d3ab7e9deb6310d5bd3742560c5cbe80d/driver/razerchromacommon.c#L701-L706) documents the report body literal `00 3f 0000 00 0c 0f 02 000008000000000000000000`: `0F/02`, `data_size=0C`, args `00 00 08 00 00 00 00 00 00 00 00 00`.
- Pinned OpenRGB commit [`6a10f77c15e4be47c792b89f1a3cc4e992389684`](https://github.com/CalcProgrammer1/OpenRGB/tree/6a10f77c15e4be47c792b89f1a3cc4e992389684) independently builds the same size and zero-prefixed args in [`RazerController.cpp`](https://github.com/CalcProgrammer1/OpenRGB/blob/6a10f77c15e4be47c792b89f1a3cc4e992389684/Controllers/RazerController/RazerController/RazerController.cpp#L1024-L1032). Its Naga V2 Pro entries use tx `1F` in [`RazerDevices.cpp`](https://github.com/CalcProgrammer1/OpenRGB/blob/6a10f77c15e4be47c792b89f1a3cc4e992389684/Controllers/RazerController/RazerDevices.cpp#L7185-L7217), and the controller selects LED ID zero for that model.
- Neuron’s production Naga definition uses LED ID `04` for lighting. The local live ledger records static region observations separately from the extended custom-display/row disagreement: [`razer/0f/02` and `razer/0f/03`](../../crates/neuron-core/protocol/ledger.toml#L1294), plus the region observations in [`razer/enum/matrix-led-ids`](../../crates/neuron-core/protocol/ledger.toml#L1355). That evidence does not make the upstream literals hardware captures or settle every custom-frame selector and length.

Keep production writes unchanged on the strength of this corpus alone. If future hardware evidence resolves the conflict, record the tested device/link mode and update the protocol ledger in the same change.
