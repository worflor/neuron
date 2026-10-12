# Programmable lighting

Status: implementation proposal. The architecture and API names below are design
targets, not shipped capabilities. Compiler selection remains subject to the
execution and deployment gates in this document.

## Product contract

One programmable system supplies palette looks and complete lighting effects.
Authors can express procedural fields, particles, persistent simulations, feedback,
multiple passes and mathematical color transformations. The system must not be
limited to a fixed list of grade operations or a fragment-only execution model.

Python is the proposed primary authoring syntax. Numerical kernels compile into
an executable program; ordinary rendering does not require interpreting Python
per LED or per frame. Rust owns execution through Neuron's existing lighting
producer. Native Rust patterns remain valid implementations of the same product.

A consumer installs a pack, chooses its look or effect and adjusts useful controls.
They do not install a development toolchain, manage compiler processes or select
an execution backend to use ordinary portable effects.

Full computational expressiveness and unrestricted host access are separate
properties. Device access, filesystem access and synthesized input are not lighting
primitives. Resource budgets protect responsiveness; they must not become arbitrary
artistic restrictions or a permanently small catalog of allowed formulas.

## Where it fits in the product

The existing user model remains **effect + spectrum = layer**, followed by stack
composition. Programmability extends those roles; it is not a third independent
lighting system or a competing catalog that users must learn before creating a look.

An effect supplies the evolving field: flames, falling columns, waves, stars,
particles or a static surface. A spectrum supplies color: solid/gradient sources,
their interpolation, motion and timeline. Compiled code can implement either role,
and the same source pack can export both. Native and installed implementations
appear together in the relevant existing editor surface.

This distinction describes responsibility, not complexity. A spectrum can generate
a sophisticated spatial or animated appearance on a static field. It does not need
to be promoted to an effect merely because its math is expensive or its appearance
is dramatic. An effect can be simple. Both use the same instance owner and execution
substrate, with capabilities declared by the author rather than inferred from code
length, performance or visual intensity.

There are three useful module roles within that model:

| Role | Operates on | User home |
| --- | --- | --- |
| Effect producer | Geometry, inputs and simulation resources; emits scalar fields, named channels or direct color. | Existing effect catalog and effect properties. |
| Spectrum program | The source palette, field/sample attributes, geometry, time and declared input/state resources; resolves appearance. | Existing Spectrum section, alongside its source and motion controls. |
| Modifier | An existing palette, field or layer-color result; transforms it non-destructively. | Inline under the property or layer it modifies. |

A grade is a palette modifier, not the definition of all programmable lighting.
Field and layer-color modifiers use the same program contract. A small ordered
modifier list is an additive authoring feature, but it does not own another scene,
device stream or simulation clock. Do not force every advanced module into grade
chips or expand every chip into a separate lighting layer.

### The layer flow

```text
geometry + snapshots + parameters + committed state
                         |
                 effect producer
                         |
          scalar / named field channels / direct color
                         |
             optional field processing
                         |
         spectrum resolution, when the field uses it
         source palette/timeline -> sampling + grade chain
                         |
               resolved layer color
                         |
            optional layer-color processing
                         |
          placement, opacity and stack composition
                         |
          existing base transition and output path
```

This is a responsibility diagram, not a request to materialize every arrow as an
allocated buffer. Uniform work can be prepared once; compatible pure operations
can be fused. Stateful advance passes run on the producer's simulation schedule
and provide committed resources to the relevant presentation stages.

Direct-color effects take the direct-color branch, preserving their original
colors instead of inventing a luminance-to-palette conversion. A layer-color
modifier can still grade, filter or otherwise process that result. Using a palette
inside a direct-color program is an explicit author choice with declared bindings.

### Concrete authoring paths

| Starting point | Edit | Result and ownership |
| --- | --- | --- |
| Fire + ember gradient | Add Pastel to the palette. | The same flame simulation uses a transformed palette; removing Pastel restores its source. |
| Wave + rainbow | Reverse palette order. | Color order changes; Wave phase and direction remain intact. |
| Static + a gradient | Select a spatial animated spectrum program. | The spectrum supplies the animated appearance over the static field; it uses the same layer owner. |
| Fire + any palette | Add a field-distortion module. | Distortion processes Fire's existing field without reconstructing or rendering Fire a second time. |
| A direct-color effect | Add a layer-color grade. | Its native color result is transformed without pretending it originally sampled the spectrum. |
| Any layer | Select a community particle effect. | A program-backed producer occupies the existing effect slot and presents ordinary parameters. |

A pack may export a reusable spectrum, an effect, modifiers and a saved composite
look that wires them together. Selecting the saved look installs those bindings
through existing scene authoring; it does not start a hidden renderer. A creator
can offer the same underlying animation as a ready-made catalog effect and as a
reusable spectrum module where both entry points genuinely support those roles.

### Palette, spectrum and modifier boundaries

Keep the source palette independently editable. A spectrum program samples that
palette through a stable host interface rather than copying its stops into opaque
kernel constants. The default spectrum program implements the existing interpolation,
motion and timeline behavior through its existing native Rust implementation;
programmability does not require rewriting that math in a kernel language.
Existing motion controls remain bindings to this
default program; they must not also run an undisclosed second motion system around
a custom program. A custom program declares whether it consumes the standard
coordinate/motion result, replaces it or exposes compatible parameter bindings.

The palette service distinguishes source sampling from graded sampling, preventing
accidental recursion or duplicate grading. A grade chain wraps palette queries;
it does not imply baking transformed stops. Modules consuming a graded palette
receive that service explicitly. Modules needing its original colors bind the
source service explicitly. Persist those bindings with the authored definition.

Pure palette grades depend on palette data, query coordinate and declared uniform
parameters/assets. They have no independent simulation, time or device geometry.
They may inspect the full source palette and sample its neighbors: purity does not
limit them to a fixed pointwise color formula. Their mathematical result is the same
in the gradient strip, live layer and export for the same query.

Spatial, animated or input-reactive color programs belong to the spectrum-program
role. They may use per-layer state with explicit advance/present access. That state
is part of the existing layer instance, not a global state attached to a reusable
palette preset. A static effect plus such a program is a valid complete animation.

Field modifiers consume the producer's field and declared channels. Layer-color
modifiers consume its resolved color result. This makes blur, distortion, edge
extraction, spatial color grading and temporal feedback expressible without
mislabeling them as palette-only grades or requiring a new scene engine.

Graph nesting and exports do not duplicate instances implicitly. A native Fire
node referenced by one layer is evaluated once for that scheduled cycle; multiple
downstream consumers read its result. A second independently authored Fire node
is a distinct stateful instance. Sharing state across layers requires explicit
resource bindings and an ownership contract, never a matching program name.

## Existing architecture to extend

The production live path uses `LiveComposite` around `Compositor`. Local and
hosted rendering share that adapter and publish complete `BaseFrame` snapshots.
This remains the authority for a physical surface. A program is an implementation
of a layer or spectrum stage, not another device owner or scene renderer.

`Pattern` has legacy `field` rendering and a newer simulation/presentation
contract. Only verified implementations may use the latter. Introducing programs
does not migrate unconverted built-ins to `LightingSession` or give a mixed stack
two independently advancing authorities.

Use the existing contracts for:

- Stable layer identity, checked reconciliation and presentation-only updates.
- Monotonic timing, sequenced input observations and provider policy.
- Layer placement, opacity, Normal/Add/Screen/Cut composition and live transitions.
- Complete versioned preview frames and physical-unit targeting.
- Transport pacing, deduplication, ownership, write gates and shutdown.
- Spectrum selection, motion, interpolation and persisted authored palettes.

Extend the registry's descriptor resolution to support native and compiled
implementations. The current static `PatternDef` table cannot alone represent
installed dynamic modules. Keep one resolver and typed capability vocabulary;
do not disguise an unknown pack ID as an existing native pattern key.

## Authoring model

Three objects have distinct lifetimes:

| Object | Responsibility |
| --- | --- |
| Program | Immutable compiled code, entry points, reflected types, dependencies and execution requirements. |
| Effect definition | Serializable program reference, parameters, resource declarations and pass connections. |
| Instance | One layer's effect, spectrum and modifier resources, input cursors and accepted configuration. |

Two layers using the same program may share compiled code. They never implicitly
share mutable simulation state. Reordering moves an instance by layer ID.

A pack is a folder containing source modules, a small manifest and optional assets.
The manifest wires code and declares metadata; it is not a second mathematical
recipe language. Installable TOML expressions are not the foundation of this system.
An initial manifest format may use TOML without constraining the program itself.

The definition declares typed parameters with defaults, ranges, units and display
metadata. Support booleans, integers, floats, vectors, colors, enums and bounded
buffer/asset references. Do not force everything through the existing all-float
parameter representation. Reflection and validation use one schema in the editor,
CLI, persistence and runtime.

The numerical authoring scope includes functions, arithmetic, vectors, matrices,
typed structs, conditionals, loops, indexed buffers, explicit random state and
composable passes. Large fields, particle systems and reductions are legitimate
programs. Additional precisions and backend features carry explicit requirements.

Dynamic Python object behavior is not automatically a numerical kernel. The chosen
frontend must document what compiles and give source-located errors for unsupported
constructs. It must never silently insert an interpreted fallback into a realtime
kernel. Prefer an established compiler frontend over creating a new Python compiler.

Authors may construct definitions with Python objects, but downloaded module import
must not execute arbitrary host Python as an unchecked installation step. Determine
whether definitions are statically discoverable or evaluated in a confined build
worker before choosing the authoring API.

The kernel language must remain powerful enough for general numerical algorithms.
A required language feature should lead to an explicit compiler/backend extension
or capability declaration, rather than an unexplained prohibition in the product API.

## Stages and resources

Stage roles describe when code runs, not a fixed number of passes:

| Role | Contract |
| --- | --- |
| Initialize | Seed instance resources for a new or incompatible instance. |
| Advance | Consume the scheduled simulation interval and its input batch; mutate designated state. |
| Present | Read committed simulation state at presentation time and produce a field without advancing it. |
| Map coordinate | Transform resolved spectrum/sample coordinates. |
| Map color | Transform floating color samples, with an explicit color-domain contract. |

Stages can contain multiple connected kernels. Current-cycle dependencies form a
validated acyclic graph. Feedback is expressed through explicit previous-tick
resources or ping-pong buffers, so it does not create an accidental scheduling cycle.
Pass ordering, read/write access and synchronization have defined semantics.

Simulation resources can belong to an effect, spectrum or modifier slot. Each has
a stable node identity beneath its owning layer. Changing a palette does not reset
the effect; changing one modifier does not reset unrelated modifier or spectrum
state. Replacing an incompatible node replaces only that node's resources. The
renderer coordinates all advances and commits; presentation never independently
ticks a stateful color module just because the palette strip is drawn.

Resources include typed sample buffers, particle/struct buffers, multidimensional
fields, images and scratch storage. Persistent and temporary lifetimes are explicit.
The host supplies shape and layout metadata; a pack cannot assume every device is
a dense rectangle. Geometry exposes valid sample identities and normalized
positions, with optional physical coordinates and neighbor relationships when known.

## Low-level author contract

Expose the information needed to build numerical algorithms, not just a
`color_for_key(index)` callback. Typed numerical buffers and pass connections are
the low-level interface; product roles give them an understandable editor home.

| Surface | Required information and operations |
| --- | --- |
| Sample geometry | Stable sample IDs, valid mask, board and layer-local coordinates, placement transform and declared units; optional physical positions, adjacency and UV mappings with availability metadata. |
| Field data | Palette coordinate, intensity and separate coverage; declared named scalar/vector channels and direct linear color. Examples include velocity, heat, age or distance when the producer supplies them. |
| Palette service | Source stops, stop positions, interpolation/topology policy and sampling functions; resolve active timeline data once where possible and preserve unwrapped coordinates until the declared boundary stage. |
| Timing | Monotonic high-precision elapsed time, simulation interval, tick/cycle identity and presentation interpolation fraction; no arbitrary global phase wrap. |
| Inputs | Ordered mapped press/release observations, held states and declared provider snapshots with sequence, age, gap and availability metadata. |
| State and scratch | Typed persistent buffers, multidimensional fields, particle structs, images, previous-tick resources and temporary storage with defined lifetime/access. |
| Numerical library | Scalar/vector/matrix math, sampling/filtering, noise/hash functions, explicit seeded random state, color-space conversion, reductions and supported synchronization/atomic operations. |
| Composition | Read-only upstream graph results and explicit palette/color outputs; coverage and radiance remain distinct from layer opacity and blend mode. |

Do not fabricate unavailable channels for existing patterns or physical distances
for a device with only a logical grid. A module declares required channels and
providers; bindings validate availability and may use an explicitly authored
fallback. Existing native producers initially expose their current field, then
can add meaningful channels without a compulsory rewrite of every pattern.

Sampling has a defined domain, boundary policy, filter and coordinate convention.
Neighbor access and field resolutions are not implicitly keyboard-row operations.
An offscreen simulation field may have a different resolution from the LEDs and
be sampled onto their positions at presentation. This supports fluid fields,
distance functions and image-based algorithms without allocating a pretend LED
for every simulation cell.

Random streams have explicit seeds and ownership. Cross-backend behavior specifies
which operations are deterministic and which require a numerical tolerance;
parallel reductions and floating transcendental functions must not be advertised
as automatically byte-identical on every CPU/GPU. Existing bypass output retains
its exact compatibility contract.

Advanced scene processing may read explicitly bound authored-layer results or the
authored base composition through the existing compositor. Define graph ordering
and feedback resources before exposing it. It must not capture protocol-client
paint, access final hardware output or create a second base-scene authority. This
is an extension point in the same program contract, not an initial promise of
arbitrary cross-layer access.

Programs receive clock values, parameters, geometry and bounded provider snapshots.
They do not poll keyboard/audio/desktop providers or renew capture leases themselves.
Representative catalog execution and deterministic export use the same interface
with different policy and injected inputs.

Input cursors commit with successful simulation state, independently of whether the
resulting image is displayed. Dropping a preview frame cannot discard key edges.
Execution faults must not reuse partially mutated state as if it were committed.

Presentation cannot mutate hidden state, consume random numbers or read fresh input.
Enforce that through resource access and execution scheduling. Do not rely solely on
a Python function being named `present`.

## Color and palette semantics

Use floating working color with explicit encoded-sRGB, linear-RGB and OKLab/OKLCH
interfaces. Apply gamut mapping and quantization at defined boundaries; do not
repeatedly round intermediate passes to bytes. Existing authored interpolation and
Cut compatibility remain intact unless a separately reviewed change replaces them.

Radiance/intensity and coverage are separate outputs. Zero intensity does not
silently acquire transparency. The current emissive field converts to RGB before
the byte-domain stack; this proposal does not claim that Neuron already has an HDR
whole-scene compositor. Broader float composition can be an explicit extension.

The stage ABI distinguishes unattenuated palette color from resolved layer radiance.
The default scalar path applies its field intensity exactly once. Palette grading
occurs before that attenuation, so lifting a palette does not unintentionally light
cells whose effect intensity is zero. A general appearance program explicitly
declares whether its output still needs the field gain or already resolves it;
never infer this from pixel values. A layer-color filter intentionally operates
after resolution and can change dark colors, but coverage and placement remain
separate. Do not multiply a direct-color producer by an invented scalar intensity.

Palette looks are non-destructive program references with parameters and strength.
The original stops remain editable and clearing a look restores the original palette.
No look and zero strength bypass new math to preserve existing RGB bytes exactly.

Define whether a look transforms prepared stops or interpolated samples. These are
different operations: applying nonlinear grading before interpolation generally
differs from applying it afterward. The initial color-look role transforms resolved
floating samples; an explicit stop-preparation role may be added when needed.
Prepare uniform values once and fuse/cache work where dependency analysis permits.
Do not cache an animated or input-dependent look as a static stop list.

The first product examples are Pastel, Vivid, Watercolour and Mono, backed by the
same stage interface as community programs. A fixed op-chain implementation alone
does not establish support for arbitrary programmable effects.

Color-stage strength mixes the input and transformed color in its declared working
domain, with exact bypass at zero. It is not layer opacity. Field-processing
modules declare their own wet/dry or parameter behavior; do not impose color-lerp
semantics on a distortion, simulation solver or coverage operation.

Palette color-order reverse is a presentation setting distinct from motion reverse.
Reflect the resolved coordinate without destructively rearranging authored stops.
Specify endpoint behavior for clamp, wrap and reflected topology; test asymmetric
stop spacing and single-stop palettes. Neither reverse control resets simulation.

## Compiler and execution choice

Choose the backend after deployment tests, not solely from shader terminology.

| Candidate | Established evidence | Selection question |
| --- | --- | --- |
| Warp | Typed Python kernel compiled and exported as a CPU graph; standalone Rust replay succeeded without Python. | Stock runtime size, experimental native ABI, confined execution and CUDA-only GPU support. |
| Taichi | Documented Python compilation and AOT runtime with native C interfaces and CPU/Vulkan targets. | Demonstrate native CPU and cross-vendor GPU deployment on supported platforms. |
| Slang | Representative source compiled to C++ and SPIR-V; generated C++ executed through an installed Clang toolchain. | Demonstrate packaged CPU compilation without consumer development tools and reliable native integration. |
| Numba | Numerical JIT and C-callable functions are documented. | Artifact lifetime/deployment and native execution ownership; deprecated AOT route is not a foundation. |

The Warp proof used 132 wave samples, 220 checked frames and 200 timed frames
after warmup on an Intel i5-11400F under Windows. Median time was 0.0053 ms,
including named time-input update, CPU replay and output copying. It was an
isolated fixture, not the app pipeline, a simulation-state test or LED verification.
The standard Windows wheel's two main DLLs total approximately 306 MiB.

Taichi remains unexecuted in this comparison. Slang's direct CPU shared-library
routes failed in the tested distribution; generated C++ worked using an existing
development compiler. SPIR-V compilation did not establish GPU execution.

CPU execution is the baseline for ordinary portable programs. GPU acceleration is
an additional execution choice, with declared features and measured benefit.
Heavy or hardware-specific packs can declare requirements instead of being silently
weakened. An unavailable target produces an honest explanation or an explicitly
authored fallback. Do not require a GPU to run a simple palette transform.

The public effect contract should not expose accidental backend implementation
details. However, one contract does not automatically make source languages or
artifacts interchangeable. Add another frontend/backend only with a real adapter
and tests; do not create a speculative universal IR to hide this work.

Compile/cache keys cover source, included dependencies, compiler/runtime versions,
Neuron ABI, target and relevant options. Parameter changes do not recompile code.
Compile away from the render/UI threads and retain the last valid artifact until a
replacement is fully prepared. Loading an installed pack must work offline.

## Runtime lifecycle and failure

Bind an instance to physical surface, owner generation, layer identity and resource
shape. Cache complete results per scheduled output cycle. A clone or preview read
must not advance simulation twice.

Configuration changes are classified as presentation, compatible simulation
configuration or instance replacement. Palette edits preserve state. Source changes
preserve state only when both resource schema and semantic compatibility are
declared and validated. Dimensions or layout changes follow an explicit reshape or
reinitialization policy. New instances use the existing scene-transition mechanism.

Resource accounting covers persistent memory, scratch memory, compilation work,
execution time and queued requests. Budgets are based on declared resources and
measured work, not the number of slider events. Complete latest-value updates replace
superseded work. Catalog demand continues to suspend offscreen evaluation.

Do not promise safe interruption of arbitrary native or GPU kernels. Choose a
failure-containment mechanism that can actually enforce its deadline. On rejection,
retain the current scene. On a fault after mutation, reconstruct a clean generation
from validated desired state and report the failed program through existing status.

## Community distribution and trust

Packs have stable identities, versions, dependencies, source hashes and immutable
installed revisions. Saved scenes pin the revision they use. Updating a pack must
not mutate a running instance behind its descriptor or change every saved scene
without an explicit migration. Side-by-side revisions support rollback.

Folder installation is the first distribution mechanism. The same package contract
can later serve Steam or another marketplace; do not couple program semantics to a
store API. Bundles contain source and assets, not privileged install scripts.

Compiler output is executable code. Downloaded Python import and native artifact
loading require enforced confinement, not just a separate process or an import
allowlist. Keep device handles, credentials and privileged macro helpers outside
that boundary. Existing macro BOUND mode is not a hostile-code sandbox.

Reuse bundled interpreter materialization and suitable worker infrastructure where
it fits. Do not reuse the macro trigger queue or string-return protocol as a frame
API. A typed buffer contract avoids text formatting, per-LED IPC and unnecessary
copies. Reuse infrastructure without sharing a privileged live macro execution role.

A trusted local-author mode may expose broader development facilities explicitly.
That does not redefine the trust policy for downloaded community packs.

## User and author experience

Installed looks appear near the spectrum they modify; effects appear in the existing
catalog. Look ordering and strength are inline, with live preview and easy removal.
The stack inspector retains stable rows while parameters and frames change.

The default view stays effect, spectrum and layer stack. Choosing a program replaces
or extends the relevant control in place and reveals only its useful parameters.
Pure grade suggestions remain small palette controls. Stateful/spatial spectrum
programs expose their own inline parameters rather than posing as a single grade
chip with an unexplained intensity slider. Advanced field/color modifier lists
expand within the selected layer's inspector instead of adding a global programs
page or another competing stack.

For pure palette grades, the strip is an exact palette-domain preview. A spatial or
stateful spectrum has no unique context-free strip: its preview must use a declared
probe context and the live hero remains the complete spatial result. Drawing that
probe does not advance the live instance. Custom direct-color effects must not
display a supposedly authoritative spectrum strip when they do not use it.

Compilation begins on source changes or first import, never a slider event. Keep
the last valid look visible while compilation runs. Source errors identify file,
line and relevant parameter or stage in an inline author drawer. Consumers see a
concise availability fact rather than a compiler transcript.

Export includes authored parameters, program revision and assets, never transient
simulation buffers or blended transition frames unless a deliberate recording
feature requests them. Opening an editor or browsing a catalog cannot alter the
live instance's input history or random sequence.

## Implementation sequence and gates

1. **Select the execution substrate.** Compare Warp CPU and Taichi AOT CPU/Vulkan
   using the same wave, color, particle and multipass fixtures. Test Windows and
   Linux, clean-machine delivery, compile latency, idle cost, package size and
   native execution without Python. Name untested hardware and platforms.
2. **Fix the contract.** Define program descriptors, typed parameters, resources,
   geometry, color domains, stage access and versioning. Design confinement and
   enforceable fault behavior before accepting downloaded code.
3. **Integrate one program-backed layer.** Use the current producer and registry
   resolver. Demonstrate persistent state, compatible edits, exact instance identity,
   provider isolation and complete snapshots. Keep existing native patterns working.
4. **Expose useful looks and reverse.** Add non-destructive palette looks and
   color-order reverse with consistent strip/live/export output and exact bypass.
   Extend render smoke coverage for inline controls and hover states.
5. **Exercise full programs.** Add particle, feedback-field and multipass examples.
   Test mixed native/program stacks, resizing, state replacement, cancellation,
   input history, resource exhaustion and last-good recovery.
6. **Ship folder packs.** Validate schemas, assets, dependencies, revisions, offline
   compile/cache behavior and rollback. Test malformed and hostile inputs within
   the chosen containment boundary. Marketplace integration follows this contract.
7. **Validate the product.** Run `validate.ps1`, then full/locked gates appropriate
   to the implementation. Measure settled and active cost in the real pipeline,
   inspect the GUI, and name devices used for authorized physical verification.

Tests must remain disarmed. HID interception, device-write paths and the Python
bridge require the owner approval specified in `AGENTS.md` before implementation.
This proposal does not authorize hardware experiments or public publication.

## Acceptance contract

- Python-authored numerical code executes without interpreted per-frame work.
- General stateful and multipass numerical effects are expressible, not only grades.
- Effect producers, spectrum programs and palette/field/color modifiers share the
  execution contract and remain in their existing feature spaces.
- Static plus a stateful/spatial spectrum is supported without a second renderer.
- A native producer's field can be processed without a second simulation evaluation.
- Direct-color effects remain correct and can receive explicit layer-color processing.
- Palette, motion, strength, coverage and layer opacity have distinct tested semantics.
- Custom spectrum programs do not receive unintended duplicate motion or grading;
  source and graded palette bindings are explicit and non-recursive.
- Pure palette grades preserve zero-intensity effect cells, while layer-color
  processing follows its explicitly different post-resolution contract.
- Stateful spectrum and modifier nodes retain independent identities; palette edits
  and preview reads do not reset or advance unrelated nodes.
- Field-channel and provider availability is explicit; missing data is not invented.
- One scene authority, one per-layer state owner and one physical device writer.
- Parameter/palette edits preserve compatible state and do not trigger compilation.
- Presentation and preview reads cannot advance simulation or consume input.
- Full frames, stable identities and physical-unit targeting cross worker boundaries.
- Native effects retain existing behavior and established compatibility semantics.
- Resource limits and actual containment are enforced; failure retains or safely
  reconstructs the last validated scene.
- Portable effects run without consumer development tools and work offline.
- Capability requirements are explicit; unsupported hardware is not misrepresented.
- Performance claims refer to named fixtures, machines and measured boundaries.
- Community pack revisions are reproducible, inspectable and recoverable.

## References

- Existing code: `pattern.rs`, `pattern/live.rs`, `pattern/session.rs`,
  `pattern/provider.rs`, `spectrum.rs`, `neuron-app/src/light_render.rs`.
- [Warp native execution](https://nvidia.github.io/warp/v1.18/user_guide/programming_model/cpp_cuda_workflows.html)
- [Warp CPU graph example](https://github.com/NVIDIA/warp/tree/v1.18.0/warp/examples/cpp/03_apic_visualization_cpu)
- [Taichi native deployment](https://docs.taichi-lang.org/docs/master/tutorial)
- [Numba C-callable kernels](https://numba.readthedocs.io/en/stable/user/cfunc.html)
- [Slang documentation](https://docs.shader-slang.org/)
