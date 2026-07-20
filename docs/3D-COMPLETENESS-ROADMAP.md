# 3D acceleration completeness roadmap (own-remoting Vulkan)

Design for taking the forwarded 3D path from "renders the built-in triangle / a single bufferless shader draw"
to "runs a real Vulkan app". It sequences the audit's completeness findings (A1–A9) into buildable phases and
fixes the correctness landmines (B1) they expose.

## Implementation status (updated as phases land)

- **Phase 2b (A1 VBO/IBO + A3 multi-draw + A7 viewport) — DONE end-to-end (host A5000-verified; guest ICD
  recording compile-verified).** `vk_op::FORWARDED_CMDLIST` (ABI 0.8) carries a mesh: a vertex buffer, optional
  index buffer, a vertex-input layout, and an ordered draw list with per-draw viewport. Host render
  (`infinigpu-replay`), device decode (`decode_forwarded_cmdlist`), and the guest wire ENCODER
  (`infinigpu_encode_forwarded_cmdlist` + C↔Rust conformance) all implemented and tested. **Guest ICD recording
  now wired too** (`infinigpu_{pipeline,cmd_buffer,sync}.c`): captures the pipeline vertex-input layout + depth,
  records `CmdBindVertexBuffers2`/`CmdBindIndexBuffer2`/`CmdDrawIndexed`/`CmdSetViewport`/`CmdPushConstants`, and
  forwards real meshes via the encoder from `driver_submit`. Builds clean in the Mesa substrate; runtime
  render-validation (a real app in a GPU VM) is the one owner-env step left. Textures/UBO descriptor RECORDING in
  the ICD is the next guest piece (host+wire+device textures are already done — see Phase 2c below).
- **Phase 2d depth (A4) — DONE, A5000-verified.** Optional depth attachment (D32) + depth test/write/compare,
  forwarded via the `ForwardedCmdListTail.depth_flags` bitfield. Host + ABI + device + guest wire + tests.
  (A5 static state — blend/cull/MSAA — is NOT yet done; see 2d below.)
- **Phase 2c transform (push constants) — DONE, A5000-verified.** A push-constant block (an MVP `mat4`) is
  forwarded via `ForwardedCmdListTail.push_const_len` (ABI 0.9, tail 48→52 B) + a trailing section, applied to
  VERTEX|FRAGMENT before the draws. Geometry can now leave raw NDC (camera/model transform). Host + ABI + device
  + guest wire + tests.
- **Phase 2c textures — DONE, A5000-verified.** A sampled texture (RGBA8 pixels + dims + sampler cfg) is forwarded
  via `ForwardedCmdListTail.tex_count` (ABI 0.10, tail 52→56 B) + a `TextureDescWire` in the fixed-array region +
  a trailing RGBA8 pixel region. The host uploads the pixels to a device-local image (staging buffer + copy +
  UNDEFINED→TRANSFER_DST→SHADER_READ_ONLY layout transitions), builds a descriptor set (set 0: binding 0 = sampled
  image, binding 1 = sampler) and a sampler (linear/nearest × repeat/clamp), and binds it for the fragment shader
  to `textureSample`. Host + ABI + device (fail-closed decode: `data_len == w*h*4`) + guest C encoder + cbindgen
  header + conformance interop, all tests green (`forwarded_texture_samples_onto_a_quad` on the A5000; C↔Rust
  round-trip). Single-texture; multi-texture + SSBO (multiple bindings, storage buffers) are the follow-up.
- **Phase 2c textures — GUEST half DONE too (compile-verified).** The guest ICD now has a real descriptor
  subsystem (`infinigpu_descriptor.c`: layouts/samplers/pools/sets/update, all driver-owned — Mesa backfills
  none), records `CmdBindDescriptorSets` + a `CmdCopyBufferToImage2` texture upload, and at submit reads the
  bound sampled image's RGBA8 + sampler flags into the forwarded command list. So a textured app end-to-end works
  (host was already A5000-verified). Validation app `guest/icd/infinigpu_tex_test.c` (textured quad through the
  full Vulkan API). Runtime render-validation in a GPU VM is the owner-env step.
- **Phase 2c UBO (uniform buffers) — DONE end-to-end, A5000-verified.** A `var<uniform>` block (e.g. a per-frame
  MVP `mat4` — the piece push constants can't hold at scale) is forwarded via three new `ForwardedCmdListTail`
  fields (ABI 0.11, tail 56→68 B): `ubo_len`, `ubo_binding`, and `tex_binding`. The UBO and a texture share **one**
  descriptor set 0 at distinct declared bindings (UBO@`ubo_binding` VERTEX|FRAGMENT; image@`tex_binding` +
  sampler@`tex_binding+1` FRAGMENT). The host builds the `VkDescriptorSetLayout` dynamically from a `DescriptorSig`
  and caches it by signature; the UBO is a HOST_VISIBLE|HOST_COHERENT `UNIFORM_BUFFER` written per submit and bound
  in the same set as the texture. Device decode is fail-closed (rejects `ubo_len > 65536` and a `ubo_binding` that
  overlaps the texture bindings, before allocation). Guest ICD captures a `UNIFORM_BUFFER` descriptor write
  (`infinigpu_descriptor.c`), resolves `VK_WHOLE_SIZE`, and the C encoder emits the UBO blob after push-constants,
  before texpix. Host + ABI + device (hostile-input rejects) + guest C encoder + cbindgen header + conformance
  interop, all green — GPU tests `forwarded_uniform_only_offsets_geometry` (UBO offsets geometry in VERTEX) and
  `forwarded_uniform_and_texture_compose` (UBO + texture compose in one set) pass on the A5000; the texture-only
  guard `forwarded_texture_samples_onto_a_quad` still passes.
- **Next up:** SSBO / multiple descriptor sets / multi-texture (storage buffers + more than one set); A5 static
  state (blend/cull/MSAA); Phase 2a (format A6 / loadOp A8).

The rest of this doc is the original design; the per-phase wire/host/test shape it describes is what the landed
phases implemented.

The perf track (Fix A/B/D, fence-spin, the token-bucket fix) is **orthogonal**: these phases *add* per-submit
work (more state to forward and replay), they do not change the tail-latency story. Sequence them as a separate
"make it render real apps" track.

## Where the path is today (Phase-1 subset)

One `SUBMIT_FORWARDED` carries a `VulkanWorkload` (w, h, bg, scanout_addr) + a `ForwardedDrawTail` (one
`draw(vertexCount)`, topology, two SPIR-V blobs + entry points). The host (`render_forwarded`) clears to `bg`,
binds one pipeline built with an **empty** vertex-input + **empty** pipeline layout, issues **one** bufferless
`draw`, and copies an `R8G8B8A8_UNORM` result out. That renders only shaders that synthesize geometry from
`gl_VertexIndex` and read nothing — i.e. the triangle. Everything below is what a conformant app additionally
needs.

## The core architectural decision: forward a command LIST, not a single draw

The single biggest lever is replacing the one-draw header with a small **recorded command list** that mirrors
the guest command buffer. One decision subsumes four findings (A1 vertex buffers, A3 multi-draw, A7
viewport/scissor, and the dynamic-state half of A5). The guest ICD already sees the real `vkCmd*` stream in
`infinigpu_cmd_buffer.c` — instead of collapsing it to "last pipeline + last vertexCount", it records an ordered
op list and forwards it; the host replays the list between `begin/end render pass`.

Wire shape (a versioned successor to `ForwardedDrawTail`; keep the current tail as `version=1` for
back-compat, add `version=2` = command list):

```
ForwardedCmdList v2 = header{ version, op_count, resource_count, flags }
                      ops[op_count]        // tagged union, 4-byte aligned:
                        BindPipeline{ pipeline_id }
                        BindVertexBuffers{ first, count, (resource_id, offset)[] }
                        BindIndexBuffer{ resource_id, offset, index_type }
                        SetViewport{ x,y,w,h,minD,maxD }   SetScissor{ x,y,w,h }
                        BindDescriptorSets{ first, (set_id)[] }   PushConstants{ stage, offset, bytes[] }
                        Draw{ vtx, inst, firstV, firstI }  DrawIndexed{ idx, inst, firstIdx, vtxOff, firstI }
                      resources[resource_count]  // see "resource forwarding" below
```

The host keeps a per-VM **replay resource table** keyed by guest resource_id (pipelines, buffers, images,
descriptor sets) so unchanged resources are not re-uploaded every frame — the same memoize-by-id discipline Fix A
already uses for pipelines. This table is where the per-submit cost of the completeness features is contained.

## Phased plan (each phase: wire ABI Δ → guest ICD Δ → host Δ → golden test)

Ordered by value/effort. **A GPU golden-image test per phase** (render a known scene, compare to a reference)
is the gate — the guest C is not shell-testable, so each phase needs the owner's guest-build env.

### Phase 2a — cheap correctness (no command list): A6 format + A8 loadOp

- **A6 render-target format.** Add a `format: u32` to `VulkanWorkload` (map to `vk::Format` host-side: UNORM vs
  SRGB, RGBA vs BGRA). Host plumbing (`PipelineKey.format`, `build_render_pass(format)`) already exists and is
  only ever fed the `R8G8B8A8_UNORM` const — this just feeds it the real value. Fixes silent wrong colors on
  sRGB/BGRA targets. **Smallest high-value fix; do first.**
- **A8 loadOp.** Forward `loadOp`; on `LOAD`, seed the host color image from the current scanout/attachment
  contents instead of an unconditional `CLEAR`. Enables overlay/accumulation passes. (Fully useful only once
  multi-pass exists, i.e. after 2b, but the ABI+host bit is small and independent.)

### Phase 2b — the command list: A1 VBO/IBO + A3 multi-draw + A7 viewport/scissor + A5-dynamic

Implement `ForwardedCmdList v2` above. Guest: record the op list in `infinigpu_cmd_buffer.c` (stop overwriting
`draw_vertex_count`; capture `CmdBindVertexBuffers`/`CmdBindIndexBuffer`/`CmdDrawIndexed`/`CmdSetViewport`/
`CmdSetScissor`). Host: `render_forwarded` binds real vertex/index buffers (upload their contents as resources),
sets a non-empty `PipelineVertexInputStateCreateInfo` (attribute/binding descriptions forwarded with the
pipeline), and replays each draw. **This is the gate that makes any real mesh render** — nothing with a vertex
buffer draws today. Biggest single unblock.

### Phase 2c — descriptors: A2 UBO / SSBO / push constants / textures

The other half of "real apps". Forward: descriptor-set-layout + push-constant ranges (build a real
`VkPipelineLayout` instead of the empty one), plus the **resource contents** — UBO/SSBO bytes and sampled-image
pixels + sampler state — into host-side descriptor sets updated per submit. Requires the resource table (2b) to
cache buffers/images by id and re-upload only dirtied ranges. This is the largest phase (resource lifetime,
dirty-tracking, image layout transitions, sampler/format handling) and where per-submit cost most needs the
by-id memoization to stay off the hot path.

### Phase 2d — depth + fixed-function state: A4 + A5-static

- **A4 depth/stencil.** Add a depth attachment to the render pass/framebuffer + `PipelineDepthStencilStateCreateInfo`;
  forward the depth format + test/write/compare-op. Without this, 3D scenes have no hidden-surface removal
  (painter's-order artifacts). Needed by essentially every 3D app, but only meaningful once geometry (2b) exists.
- **A5 static state.** Forward a compact fixed-function block (blend factors/op+enable, sample count for MSAA,
  cull mode, front face, depth bias) and apply in `build_pipeline` (which today hardcodes FILL / cull NONE / CCW /
  1× / blend-off). Fixes transparency rendering opaque, MSAA silently downgrading, and back faces overwriting
  front faces.

### Phase 2e — sync/async: A9 (only if/when async frames land)

Timeline semaphores + honoring `in_fence`/`out_fence` (host currently ignores them; the blocking ioctl makes
waits trivially satisfied). Purely a capability boundary today, not a bug. Do this only alongside **Fix F**
(async/pipelined submit) from the perf track — they are the same "overlap frames" enabler; on their own neither
moves the current serial tail.

## Cross-cutting: finish the memoryOffset fix (full B1)

The interim `requiresDedicatedAllocation=true` (landed) forces images to offset 0. The full fix — needed before
sub-allocation is allowed back (VRAM efficiency) — threads `VkBindImageMemoryInfo::memoryOffset` through the
`SUBMIT_FORWARDED` ioctl (ABI bump) and folds `base+offset` into the host writeback target, matching the read
paths. Do this whenever the ABI is bumped for Phase 2b anyway.

## Effort / risk summary

| Phase | Findings | Effort | Risk | Unblocks | Status |
|-------|----------|--------|------|----------|--------|
| 2a | A6, A8 | S | low | correct colors; overlay passes | todo |
| 2b | A1, A3, A7, A5-dyn | **L** | med | **any real mesh renders** | **DONE** host/device/wire + guest ICD recording (compile-verified); runtime render-validation pending owner |
| 2c | A2 | **XL** | high | transformed + textured apps (UBO/tex) | **push-const transform + textures + UBO DONE end-to-end** (host/wire/device + guest ICD recording; A5000-verified); SSBO / multi-set / multi-texture todo (**next**) |
| 2d | A4, A5-static | M | med | depth-correct 3D, transparency, MSAA | **A4 depth DONE**; A5 static (blend/cull/MSAA) todo |
| 2e | A9 | M | med | async frames (with Fix F) | todo |

Recommended order: **2a → 2b → 2c → 2d**, with 2e deferred to the async-submit work. 2a is a few hours of
correctness wins; 2b is the real milestone (first real app frame); 2c is the bulk of the work. Every phase needs
the guest-build env and a golden render test on the A5000.
