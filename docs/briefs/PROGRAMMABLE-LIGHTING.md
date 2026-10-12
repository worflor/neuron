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

This service is a numerical ABI, not a per-sample Python or GPU-to-host callback.
Use staged palette data and compatible kernel-library functions. Lookup-table
acceleration requires a declared error contract; it cannot replace the exact
legacy bypass with an undisclosed approximation.

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

The current compositor does not expose a reusable field node: it evaluates a
pattern and immediately resolves its `Field` to RGB. Introduce an internal
evaluated-field snapshot/stage adapter in that existing path. Legacy patterns keep
their normal scheduled `field_at_time` evaluation; session-capable patterns retain
their advance/emit contract. Downstream modifiers read the result, never call the
pattern again. This seam must be demonstrated before claiming native-node reuse.

The snapshot also retains native palette addressing and effective geometry. Some
native layers replace raw `Cell.u` with a placement coordinate before sampling.
Expose the effective palette-query coordinate separately from optional raw producer
data; a coordinate modifier must not edit a value that a later native adapter
silently ignores.

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

Descriptors declare their output schema, available field channels and palette
bindings for the instantiated configuration. A direct-color output can still read
a palette internally; output type alone does not determine which Spectrum controls
are useful. Replace the static `has_spectrum` assumption with declared binding
capabilities for programs while retaining native behavior. An unexpected output
variant or missing required channel is a checked error, not a fabricated scalar
field or a silently active control.

## Authoring model

Three objects have distinct lifetimes:

| Object | Responsibility |
| --- | --- |
| Program | Immutable compiled code, entry points, reflected types, dependencies and execution requirements. |
| Node binding | Serializable program/entry-point reference, role, parameters, resource declarations and connections within a layer. |
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

### Effect properties as program inputs

Every node can bind read-only properties of its owning effect through the same
descriptor schema that drives the inspector. Do not maintain a second shader
parameter list or add branches such as "if Fire, copy speed and density". The
current `PatternDef.params` plus accepted `Params` provide the starting information;
the compiled interface adds genuine typed values and declared units.

An effect-property snapshot contains declared property keys, types, accepted
values, defaults, ranges, units and conditional availability. It is prepared once
per accepted configuration/render boundary and packed into typed uniform data.
Resolve names/bindings outside numerical kernels; no string-map lookup, reflection
callback or UI read occurs per LED. Cache descriptor layouts rather than calling
the allocating native schema function every tick. The live scene canonicalizer
already fills defaults; reuse that accepted representation rather than adding a
second normalization rule. Preserve native enum rounding/toggle interpretation
through its compatibility adapter, without pretending the stored float is the
future program type.

Property provenance is stage-specific. Present-only bindings may use a new
presentation/property revision with the last committed simulation field. Advance
and Initialize bindings take effect at their appropriate simulation boundaries.
All consumers within one stage evaluation read the same immutable property block.
Computed exports carry the field/tick/config generation that produced them; they
are not paired with whichever accepted definition is newest when a reader asks.
The output records both simulation and presentation provenance. Previously emitted
pixels retain their rendered revision even when a newer scene has been accepted;
the controls may display that pending/accepted value separately.

A modifier can bind its own frequency to the effect's speed, or its strength to
the effect's density, using a serialized property reference and an explicit value
mapping where needed. The UI offers that link next to the existing parameter,
shows its source/live value and permits unlinking to a constant. A new catalog
effect automatically exposes its declared properties without app-specific wiring.

Bindings reference stable layer/node identity, source descriptor/schema identity
and property key, not selected row, translated label or numeric parameter position.
Layer identity alone is insufficient: replacing an effect preserves the layer ID
while constructing a different generator. On replacement, validate type, units,
semantic role and conditional availability. A source-slot link may follow the
replacement only under its declared compatibility policy; an instance-pinned link
does not silently acquire another instance. Matching the key `speed`, even with
matching numeric ranges, does not by itself authorize retargeting.
Missing or incompatible properties use an explicitly
declared fallback or mark the binding unavailable; never silently read zero or
attach to a similarly named setting on another layer.

Conditional properties retain both their stored value and availability. For example,
Meter focus remains authored while its source uses CPU rather than audio, although
the native effect ignores that setting in the CPU mode. A link becomes unavailable
or uses its declared fallback, then restores the stored source value when the
condition becomes active. Do not delete hidden values or treat UI visibility as
evidence that a stored value has disappeared. Evaluate availability using the
declared typed gate semantics, not a separate ad hoc float conversion.

Program enums have stable variant IDs and schema-version migrations. Native enum
indices remain a compatibility encoding, not durable semantic IDs: reordering
option labels must not retarget a saved binding. Labels can change independently
of identity. Unknown legacy parameter entries are not automatically exported as
declared properties merely because they exist in the accepted parameter map.

Matching a key named `speed` does not establish a common physical meaning. One
effect's setting may be a dimensionless multiplier; another may expose cells per
second. Optional semantic roles such as temporal rate or particle density can be
declared alongside units to help reusable modules discover compatible properties.
Do not infer those roles from labels or pretend every effect has them. Normalized
range mappings are explicit mappings, not proof of equivalent physical behavior.

Linking rate values is not phase synchronization. A program that evaluates
`time*rate` can jump when the rate changes, whereas a native effect may integrate
rate over time. Continuous variable-rate motion uses explicit accumulated phase
or a declared phase export, with pure presentation reading that state. Do not
promise phase lock from a common speed value or guess a phase for an effect that
has none. Units, conversions and any value-remapping domain are explicit.

Mappings validate their target type/range and declare out-of-domain behavior.
Normalizing a constant source range has a defined constant result or rejects the
mapping; it cannot divide by zero. An existing binding does not silently change
its mapping because a later schema expands a slider's range. Pin authored mapping
domains or explicitly opt into following the source range. Enum-to-number and
boolean-to-number conversions are deliberate mappings, not implicit coercions.

Accepted settings, effective simulation quantities and live field channels are
distinct. The parameter snapshot reports the accepted control value. A native
pattern may apply internal clamps, modes or multipliers; an actual velocity, phase,
particle count or temperature requires a declared computed property or channel.
Export those through the existing descriptor/snapshot contract where meaningful,
without inspecting private Rust fields or parsing source code. Compute exports
once at their declared configuration/tick boundary; reading them cannot advance
simulation. Do not duplicate native configuration math in a generic adapter and
call the result authoritative.

An Initialize/Advance/Present entry point declares which property bindings it
reads, just as it declares palette dependencies. Editing a linked property updates
all consumers at the same boundary without recompiling. Simulation, presentation
and required reinitialization follow those declared dependencies; linking a
shader parameter does not grant write access to its source effect's settings.

Initial parameter links read the owning effect's authored accepted controls.
Computed exports and field channels use typed snapshot dependencies, not an event
bus or implicit live getter. Reject current-cycle dependency loops; feedback needs
an explicit previous-tick resource. Do not let a modifier output write back into
the property that drives its own Advance. Link/source/value UI updates retain row
identity; unlinking freezes the last resolved consumer value, rather than jumping
back to the parameter's original default.

Required binding regressions cover sparse versus explicit defaults; typed enum and
toggle reads; conditional focus preservation across source changes; same-layer-ID
effect replacement and reordering; rejected edits retaining the old snapshot;
new presentation over old simulation with truthful provenance; deferred Advance
and declared Initialize behavior; cycle rejection; and a shared producer evaluated
once for multiple consumers.

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

The current matrix snapshot exposes press events and held state, not a complete
ordered release-event projection. Release-aware programs require extending the
existing sequenced observation projection; reconstructing releases from two held
snapshots would lose short transitions. Reuse the process input reservoir rather
than introducing a program-specific hardware listener.

Input cursors commit with successful simulation state, independently of whether the
resulting image is displayed. Dropping a preview frame cannot discard key edges.
Execution faults must not reuse partially mutated state as if it were committed.

Presentation cannot mutate hidden state, consume random numbers or read fresh input.
Enforce that through resource access and execution scheduling. Do not rely solely on
a Python function being named `present`.

The restriction concerns mutable random streams, not random-looking mathematics.
Pure noise/hash functions keyed by seed, node/sample identity and an explicit time
or tick coordinate are valid presentation operations. Repeating the same query
must produce the same result without advancing any stream. If time is quantized,
that sampling policy is declared rather than accidentally tied to preview FPS.

## Color and palette semantics

Use floating working color with explicit encoded-sRGB, linear-RGB and OKLab/OKLCH
interfaces. Apply gamut mapping and quantization at defined boundaries; do not
repeatedly round intermediate passes to bytes. Existing authored interpolation and
Cut compatibility remain intact unless a separately reviewed change replaces them.

Two resolution contracts are explicit. The legacy adapter retains the existing
byte-sRGB interpolation, motion, timeline mixing, rounded encoded-channel gain and
byte composition. No program/no modifier and zero-strength bypass use that native
path unchanged. A linear program operates on floating color/radiance and converts
at the existing byte-composition boundary; it does not silently upgrade the whole
stack to linear blending. Exact bypass and the linear program's semantics are
separate promises.

Use typed stage results rather than overlapping flags: an unattenuated palette
sample still needs the adapter's field gain; a resolved appearance already owns
its radiance calculation. The consumer knows which type it receives. Linear
intermediates may carry extended or signed values where an algorithm needs them;
display conversion has an explicit gamut/tone policy. The initial final conversion
uses the existing clamped linear-to-RGB boundary unless a program applies a declared
mapping first. Non-finite display color or invalid coverage is an error, not an
implicit cast to black. Existing byte color helpers are not unquantized float APIs;
provide the latter without routing every color operation through an `Rgb` roundtrip.

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
differs from applying it afterward. The initial palette-grade role transforms
interpolated source queries, rather than baking transformed stops; an explicit
stop-preparation role may be added when needed.
Prepare uniform values once and fuse/cache work where dependency analysis permits.
Do not cache an animated or input-dependent look as a static stop list.

For the default adapter, each active timeline endpoint follows this order:
coordinate motion and boundary resolution; native stop interpolation; floating
palette-grade chain and strength; conversion to endpoint RGB; native color motion
(hue cycle or breathing); existing byte timeline crossfade; rounded encoded field
gain. A grade therefore recolors the palette while Breathe still modulates its
brightness. All nodes in the grade chain retain floating intermediates; conversion
occurs at the declared legacy endpoint boundary, not after each grade. The zero-
strength path delegates to the untouched native endpoint sampler.

Topology derives from the authored source and its declared overrides, not from
whether a grade happens to make the endpoint colors coincide. Grade strength and
time do not silently switch an open gradient between wrap and reflection. Custom
spectrum programs can replace this order through explicit bindings; they cannot
quietly inherit another application of native motion or grading.

Legacy Cut eligibility is captured from the original resolved byte picture before
every optional color transform, including palette grades. That original picture
uses the same evaluated native field; a second color resolution may be required,
but never another simulation evaluation. An originally empty cell remains absent
after a lift; an originally present cell transformed to black still occupies its
original footprint. Preserve the existing global Cut function for untouched
layers and use an explicit compatibility adapter for processed Cut layers.
Programmable cutouts normally use explicit coverage with Normal composition;
choosing legacy final-black Cut semantics remains an explicit compatibility choice.

Spatial filtering with coverage uses associated linear color internally: for
straight color `c` and coverage `a`, filter `p = a*c` and `a` with the same weights.
The stage adapter recovers straight color only when needed; zero coverage has a
defined zero-color result and no division. Typed straight/associated results name
the boundary owner and prevent a second coverage multiplication. Region masking
and layer opacity apply to effective coverage at composition, not to color gain
inside every pass. Initial composition still uses the existing byte blend path;
linear filter arithmetic does not imply linear whole-stack blending.

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

## Worked author contract: Ink spectrum

An Ink pack exports a spatial spectrum that can animate a Static layer, plus a
catalog look wiring those two bindings together. The source palette remains an
ordinary editable Neuron palette. Exporting another effect entry point can reuse
the solver code without implicitly sharing its buffers.

| Declaration | Example contract |
| --- | --- |
| Parameters | Integer seed; diffusion coefficient in declared grid units; injection strength; palette binding; boundary enum. Value edits update uniforms, not source compilation. |
| State | Committed concentration field and candidate fields over a declared 128x64 simulation grid; scratch and any interpolation history are separately accounted. These are not 8192 physical LEDs. |
| Initialize | Zero the field and establish explicit random seed. A palette is not an initialization dependency in this example. |
| Advance pass 1 | Map every ordered press event to grid coordinates and inject into candidate concentration, reading the previous committed field. |
| Advance pass 2 | Run a five-point diffusion stencil from one candidate field into another, with explicit boundary handling and substeps. Swap resource roles between substeps; publish only validated completion. |
| Present | Sample the committed field at actual LED positions, use concentration as a palette query, and return a typed appearance. A pure seed/time hash can add shimmer without consuming RNG state. |
| Edits | A palette change alters presentation immediately without clearing concentration. Diffusion changes future advances. A grid-shape change uses an explicit resample or reinitialize policy. |
| Preview | Hero reads the complete live result. A strip probe reads committed state at a declared position; a catalog demonstration owns a separate representative instance. |

For equal grid spacing, let `r = D*dt/dx^2`. The stencil has center weight
`1-4*r` and four neighbor weights `r`. For `0 <= r <= 1/4`, these weights are
nonnegative and sum to one: this specific averaging step cannot exceed its input
minimum/maximum under compatible boundary handling. Larger diffusivity requires
smaller substeps or another solver, not an arbitrary limitation on the author.
Substep cost belongs in the execution/resource contract. This statement is about
this discrete stencil, not a certificate for arbitrary fluid simulations.

This example must run through the existing producer with independent code/state
lifetimes, valid input-cursor commits and native/linear boundary semantics. Its
proof gate is stateful multi-pass execution from Rust without per-frame Python,
not just successful compilation of one color function.

## Mathematical witnesses for the contract

Scryer exact rational/symbolic arithmetic and certified real enclosures confirm
the following ideal-arithmetic witnesses. They constrain stage ordering and form
reference cases for later tests; they do not prove Rust floating execution, GPU
agreement, physical LED transfer or subjective appearance.

| Witness | Derived result | Required design consequence |
| --- | --- | --- |
| Let `G(x)=2/5+(3/5)*x`, `x=1/5`, gain `i=1/4`. | `i*G(x)=13/100`; `G(i*x)=43/100`. | A lift before gain differs from a lift after gain; preserve dark effect cells through the palette-grade placement. |
| Let `G(x)=x^2` and stops be 0 and 1. | Grade of their midpoint is `1/4`; midpoint of graded stops is `1/2`. | Stop baking and query grading are different features; the editor cannot substitute one for the other. |
| Half linear energy encoded with the ideal sRGB transfer. | `1.055*(1/2)^(5/12)-0.055` is approximately `0.7353569830524494906`; times 255 is approximately `187.5160306783746201`. | Rounded linear half-white is 188; legacy half-byte-white is 128. Retain a named legacy adapter rather than claiming float conversion preserves the old gain. |
| Opaque red next to fully transparent blue, equal filter weights. | Filtered associated red is `1/2`, blue is 0, coverage is `1/2`; straight-color filtering instead introduces hidden blue. | Spatial processing filters associated color and coverage together. |
| Associated color `p=1/2`, coverage `a=1/2`, opacity `o=1/2`. | Correct opacity-scaled `p` is `1/4`; multiplying by coverage again gives `1/8`. | Typed straight/associated results and one boundary owner prevent double attenuation. |
| Constant-input recurrence `y_next=(9/10)*y+1/5`. | Its fixed point is 2. | Stable feedback can exceed display range; extended intermediate state and final mapping are separate contracts. |
| Diffusion coefficient `r=1/8`, compared with `r=3/10`. | Center weights are `1/2` and `-1/5` respectively; the first stencil's weights sum to 1. | Choose substeps/solver from the numerical method, rather than confusing a negative numerical artifact with valid color data. |

Decimal constants in these witnesses are exact written decimals. Shipped f32/f64
constants, rounding and gamut helpers require their own execution fixtures. The
existing five-step OKLab chroma search is a fast approximation, not a certified
universal gamut solver. A kernel color library needs explicit tolerances and
final-output policy rather than inheriting an unsupported precision claim.

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
configuration or instance replacement. Palette edits preserve compatible state. Source changes
preserve state only when both resource schema and semantic compatibility are
declared and validated. Dimensions or layout changes follow an explicit reshape or
reinitialization policy. New instances use the existing scene-transition mechanism.

Palette and parameter dependencies are declared per Initialize, Advance and Present
entry point and checked against actual bound access. A Present-only palette edit
does not reconfigure simulation. An Advance dependency uses the new palette on
future ticks while preserving compatible state. An Initialize dependency declares
whether an edit preserves existing resources or reinitializes the affected node.
Do not label every palette edit presentation-only or recompile code for value edits.

The per-layer runner coordinates node order, snapshots and commits within the
existing producer. There is at most one mutable job in flight per state owner.
Worker requests/replies carry surface, owner generation, graph revision, node
identity, simulation sequence and presentation cycle. A compiled advance writes
candidate resources; validated state and its input cursor commit together. The
backend must establish how it keeps committed resources separate without claiming
zero-cost rollback or copying all buffers unconditionally.

A presentation deadline miss keeps the last complete picture available. It does
not automatically discard a valid simulation advance: an ordered result from the
current owner can commit state even if its image is superseded. Do not accept stale
generations or out-of-order state; do not launch a competing job against an owner
still advancing. Pending scene configuration coalesces, input history does not.
An actual cancellation/fault leaves uncommitted cursors untouched and invokes the
declared recovery/gap policy if retained history expires.

Native legacy objects are not rollback-capable transactions. Their scheduled field
evaluation commits under the existing producer contract. A downstream program
failure can reject the new picture, but cannot pretend that native time/input/RNG
never advanced. Retain a healthy native instance and its evaluated snapshot when
safe; rebuild a compromised owner rather than reviving partially mutated state.
Whole-graph rollback is not promised by the word atomic.

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
- Dependency metadata distinguishes palette use in initialization, simulation and
  presentation; edits follow that dependency without blanket rebuilds.
- Legacy byte gain and Cut footprint remain stable under their compatibility
  adapters; linear programs and explicit coverage have declared output semantics.
- Spatial filter fixtures reject hidden-color bleed and double coverage/opacity.
- Late valid simulation commits are distinct from stale frame publication;
  cancellation and recovery cannot silently consume uncommitted input edges.
- Pure time-based noise supports repeated and arbitrary presentation queries
  without mutable RNG advancement.
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
